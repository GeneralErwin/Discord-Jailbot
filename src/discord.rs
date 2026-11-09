use anyhow::{Context, bail};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::commands::moderation::{
    JailCommand, JailConfigCommand, JailInfoCommand, PrisonersCommand, SetLogsCommand,
    UnjailCommand,
};
use crate::services::ratelimit::{self, Lane};

use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client as HttpClient;
use twilight_interactions::command::{CommandInputData, CreateCommand};

use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::ChannelType,
    guild::Permissions,
    http::{
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    },
    id::{
        Id,
        marker::{ApplicationMarker, ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    },
};

#[derive(CreateCommand)]
#[command(name = "ping", desc = "Check whether the bot is online")]
struct PingCommand;

#[derive(CreateCommand)]
#[command(
    name = "jailsetup",
    desc = "Create or find the Prisoner role",
    default_permissions = "moderator_permissions"
)]
struct JailSetupCommand;

#[derive(CreateCommand)]
#[command(
    name = "setjail",
    desc = "Set the channel prisoners can access",
    default_permissions = "manage_channels_permissions"
)]
struct SetJailCommand {
    /// The text channel prisoners will be allowed to use.
    #[command(channel_types = "guild_text")]
    channel: Id<ChannelMarker>,
}

fn moderator_permissions() -> Permissions {
    Permissions::MANAGE_ROLES
}

fn manage_channels_permissions() -> Permissions {
    Permissions::MANAGE_CHANNELS
}

/// How often queued repair work is flushed.
const REPAIR_TICK: Duration = Duration::from_secs(3);

/// Minimum gap between two event-triggered repair sweeps of the same server.
const REPAIR_INTERVAL: Duration = Duration::from_secs(20);

/// How long a server is remembered after its last repair sweep.
const REPAIR_HISTORY: Duration = Duration::from_secs(600);

pub async fn run(
    token: String,
    pool: SqlitePool,
    started: std::time::Instant,
) -> anyhow::Result<()> {
    let http = HttpClient::new(token.clone());

    let application_id = http.current_user_application().await?.model().await?.id;

    let commands = [
        PingCommand::create_command().into(),
        JailSetupCommand::create_command().into(),
        SetJailCommand::create_command().into(),
        JailCommand::create_command().into(),
        UnjailCommand::create_command().into(),
        SetLogsCommand::create_command().into(),
        PrisonersCommand::create_command().into(),
        JailInfoCommand::create_command().into(),
        JailConfigCommand::create_command().into(),
    ];

    http.interaction(application_id)
        .set_global_commands(&commands)
        .await?;

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Global slash commands registered"
    );

    let mut shard = Shard::new(
        ShardId::ONE,
        token,
        Intents::GUILDS | Intents::GUILD_MEMBERS,
    );

    let mut maintenance_tick = tokio::time::interval(Duration::from_secs(30));
    let mut reconciliation_tick = tokio::time::interval(Duration::from_secs(300));

    // Gateway events only mark a server as dirty. Repairs are coalesced and
    // paced, so a burst of member or channel changes cannot turn into one full
    // guild sweep per event.
    let mut repair_tick = tokio::time::interval(REPAIR_TICK);
    let mut pending_repairs: HashSet<u64> = HashSet::new();
    let mut last_repair: HashMap<u64, Instant> = HashMap::new();

    tracing::info!("Connecting to Discord...");

    loop {
        tokio::select! {
            _ = maintenance_tick.tick() => {
                if let Err(error) = process_due_jails(&http, &pool).await {
                    tracing::error!(?error, "Automatic jail release cycle failed");
                }
            }

            _ = reconciliation_tick.tick() => {
                if let Err(error) = queue_configured_guilds(&pool, &mut pending_repairs).await {
                    tracing::error!(?error, "Could not queue periodic jail reconciliation");
                }
            }

            _ = repair_tick.tick() => {
                flush_repairs(&http, &pool, &mut pending_repairs, &mut last_repair).await;
            }

            event = shard.next_event(EventTypeFlags::all()) => {
                let Some(event) = event else {
                    tracing::warn!("Discord gateway stream ended");
                    break;
                };

                match event {
                    Ok(Event::Ready(_)) => {
                        tracing::info!(
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "Connected to Discord successfully"
                        );

                        // Queue startup repair instead of sweeping inline, so the
                        // gateway loop stays responsive while it converges.
                        match queue_configured_guilds(&pool, &mut pending_repairs).await {
                            Ok(queued) => {
                                tracing::info!(queued, "Queued startup jail reconciliation")
                            }
                            Err(error) => {
                                tracing::error!(?error, "Could not queue startup jail reconciliation")
                            }
                        }

                        if let Err(error) = process_due_jails(&http, &pool).await {
                            tracing::error!(?error, "Startup automatic release cycle failed");
                        }

                        tracing::info!(
                            boot_ms = started.elapsed().as_millis() as u64,
                            "Boot complete"
                        );
                    }

                    Ok(Event::InteractionCreate(event)) => {
                        if let Err(error) =
                            handle_interaction(&http, application_id, event.0, &pool).await
                        {
                            tracing::error!(?error, "Failed to handle interaction");
                        }
                    }

                    Ok(Event::MemberRemove(event)) => {
                        let guild_id = event.guild_id;
                        let user_id = event.user.id;

                        match crate::services::JailService::new(pool.clone())
                            .member_left(&guild_id.to_string(), &user_id.to_string())
                            .await
                        {
                            Ok(true) => {
                                let message = format!(
                                    "**Prisoner left the server**\nMember: <@{user_id}>\nThe active jail record has been preserved."
                                );

                                if let Err(error) =
                                    crate::services::discord_moderation::send_moderation_log(
                                        &http, &pool, guild_id, &message
                                    ).await
                                {
                                    tracing::warn!(?error, "Could not send prisoner departure log");
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::error!(?error, user_id = %user_id, "Could not record member departure");
                            }
                        }
                    }

                    Ok(Event::MemberAdd(event)) => {
                        // A returning prisoner is re-jailed by the next repair pass.
                        pending_repairs.insert(event.guild_id.get());

                        tracing::debug!(
                            guild_id = %event.guild_id,
                            user_id = %event.member.user.id,
                            "Queued jail repair for a member who joined"
                        );
                    }

                    Ok(Event::MemberUpdate(event)) => {
                        let guild_id = event.guild_id;
                        let user_id = event.user.id;

                        match crate::database::repository::get_active_jail(
                            &pool,
                            &guild_id.to_string(),
                            &user_id.to_string(),
                        )
                        .await
                        {
                            Ok(Some(_)) => {
                                pending_repairs.insert(guild_id.get());

                                tracing::debug!(
                                    guild_id = %guild_id,
                                    user_id = %user_id,
                                    "Queued jail repair after a jailed member edit"
                                );
                            }
                            Ok(None) => {}
                            Err(error) => {
                                tracing::error!(?error, "Could not check active jail after member update");
                            }
                        }
                    }

                    Ok(Event::ChannelCreate(event)) => {
                        if let Some(guild_id) = event.0.guild_id {
                            pending_repairs.insert(guild_id.get());

                            tracing::debug!(
                                guild_id = %guild_id,
                                channel_id = %event.0.id,
                                "Queued jail repair for a new channel"
                            );
                        }
                    }

                    Ok(Event::ChannelUpdate(event)) => {
                        if let Some(guild_id) = event.0.guild_id {
                            pending_repairs.insert(guild_id.get());

                            tracing::debug!(
                                guild_id = %guild_id,
                                channel_id = %event.0.id,
                                "Queued jail repair after a channel update"
                            );
                        }
                    }

                    Ok(Event::ChannelDelete(event)) => {
                        let channel_id = event.0.id.to_string();

                        match crate::database::repository::forget_channel_snapshots(
                            &pool, &channel_id,
                        )
                        .await
                        {
                            Ok(0) => {}
                            Ok(removed) => tracing::debug!(
                                channel_id = %event.0.id,
                                removed,
                                "Discarded permission snapshots for a deleted channel"
                            ),
                            Err(error) => tracing::warn!(
                                ?error,
                                channel_id = %event.0.id,
                                "Could not discard permission snapshots for a deleted channel"
                            ),
                        }
                    }

                    Ok(_) => {}

                    Err(error) => {
                        tracing::warn!(?error, "Gateway event error");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Queues a repair pass for every server that has completed jail setup.
async fn queue_configured_guilds(
    pool: &SqlitePool,
    pending: &mut HashSet<u64>,
) -> anyhow::Result<usize> {
    let configs = crate::database::config_repository::list_guild_configs(pool).await?;
    let mut queued = 0_usize;

    for config in configs {
        if !config.setup_complete {
            continue;
        }

        match config.guild_id.parse::<u64>() {
            Ok(raw_guild_id) => {
                pending.insert(raw_guild_id);
                queued += 1;
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    guild_id = %config.guild_id,
                    "Skipping configuration with invalid guild ID"
                );
            }
        }
    }

    Ok(queued)
}

/// Flushes the coalesced repair queue.
///
/// Each server is repaired at most once per [`REPAIR_INTERVAL`], so an event
/// storm collapses into a single bounded sweep instead of one sweep per event.
async fn flush_repairs(
    http: &HttpClient,
    pool: &SqlitePool,
    pending: &mut HashSet<u64>,
    last_repair: &mut HashMap<u64, Instant>,
) {
    if pending.is_empty() {
        return;
    }

    let now = Instant::now();

    let due: Vec<u64> = pending
        .iter()
        .copied()
        .filter(|guild_id| {
            last_repair
                .get(guild_id)
                .is_none_or(|previous| now.duration_since(*previous) >= REPAIR_INTERVAL)
        })
        .collect();

    for raw_guild_id in due {
        pending.remove(&raw_guild_id);
        last_repair.insert(raw_guild_id, Instant::now());

        let guild_id = Id::<GuildMarker>::new(raw_guild_id);

        if let Err(error) =
            crate::services::discord_moderation::reconcile_guild(http, pool, guild_id).await
        {
            tracing::error!(
                ?error,
                guild_id = %guild_id,
                "Event-triggered jail reconciliation failed"
            );
        }
    }

    last_repair.retain(|_, previous| now.duration_since(*previous) < REPAIR_HISTORY);
}

/// Finds due sentences and releases them automatically.
/// A member who has left is handled without requiring them to be present.
async fn process_due_jails(http: &HttpClient, pool: &SqlitePool) -> anyhow::Result<()> {
    let service = crate::services::JailService::new(pool.clone());
    let due_records = service.list_due().await?;
    if due_records.is_empty() {
        return Ok(());
    }

    let configs = crate::database::config_repository::list_guild_configs(pool).await?;

    for record in due_records {
        let Some(config) = configs
            .iter()
            .find(|config| config.guild_id == record.guild_id)
        else {
            continue;
        };

        if !config.auto_release_enabled {
            continue;
        }

        let guild_id = match record.guild_id.parse::<u64>() {
            Ok(value) => Id::<GuildMarker>::new(value),
            Err(error) => {
                tracing::error!(
                    ?error,
                    record_id = record.id,
                    "Invalid guild ID in due jail record"
                );
                continue;
            }
        };

        let target_id = match record.user_id.parse::<u64>() {
            Ok(value) => Id::<UserMarker>::new(value),
            Err(error) => {
                tracing::error!(
                    ?error,
                    record_id = record.id,
                    "Invalid user ID in due jail record"
                );
                continue;
            }
        };

        let prisoner_role_id = match config.prisoner_role_id.as_deref() {
            Some(value) => match value.parse::<u64>() {
                Ok(value) => Id::<RoleMarker>::new(value),
                Err(error) => {
                    tracing::error!(?error, record_id = record.id, "Invalid Prisoner role ID");
                    continue;
                }
            },
            None => {
                tracing::error!(
                    record_id = record.id,
                    "Cannot auto-release: Prisoner role is not configured"
                );
                continue;
            }
        };

        let release_result = crate::services::discord_moderation::expire_member(
            http,
            pool,
            guild_id,
            target_id,
            prisoner_role_id,
        )
        .await;

        match release_result {
            Ok(record_id) => {
                tracing::info!(record_id, user_id = %target_id, "Sentence expired and member released");
            }
            Err(error) if is_discord_not_found(&error) => {
                tracing::warn!(
                    record_id = record.id,
                    user_id = %target_id,
                    "Member is absent; attempting overwrite-only expiry"
                );

                match crate::services::discord_moderation::expire_absent_member(
                    http, pool, guild_id, target_id,
                )
                .await
                {
                    Ok(Some(record_id)) => {
                        tracing::info!(record_id, user_id = %target_id, "Absent member's sentence expired");
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(
                            ?error,
                            record_id = record.id,
                            "Could not finish expiry for absent member; it will be retried"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    record_id = record.id,
                    user_id = %target_id,
                    "Automatic release failed; the active record will be retried"
                );
            }
        }
    }

    Ok(())
}

fn is_discord_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<twilight_http::error::Error>()
            .is_some_and(|discord_error| {
                matches!(
                    discord_error.kind(),
                    twilight_http::error::ErrorType::Response { status, .. } if *status == 404
                )
            })
    })
}

async fn handle_interaction(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: Interaction,
    pool: &SqlitePool,
) -> anyhow::Result<()> {
    let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_ref() else {
        return Ok(());
    };

    match command.name.as_str() {
        "ping" => {
            respond(
                http,
                application_id,
                &interaction,
                "Pong! The jail bot is online.",
            )
            .await?;
        }

        "jailsetup" => {
            let Some(guild_id) = interaction.guild_id else {
                respond(
                    http,
                    application_id,
                    &interaction,
                    "This command can only be used inside a server.",
                )
                .await?;

                return Ok(());
            };

            match setup_prisoner_role(http, pool, guild_id).await {
                Ok(role_id) => {
                    respond(
                        http,
                        application_id,
                        &interaction,
                        &format!("Prisoner role is ready. Role ID: `{role_id}`"),
                    )
                    .await?;
                }

                Err(error) => {
                    tracing::error!(?error, "Prisoner role setup failed");

                    respond(
                        http,
                        application_id,
                        &interaction,
                        "Setup failed. Check that the bot has Manage Roles permission and the correct role hierarchy.",
                    )
                    .await?;
                }
            }
        }

        "setjail" => {
            let Some(guild_id) = interaction.guild_id else {
                respond(
                    http,
                    application_id,
                    &interaction,
                    "This command can only be used inside a server.",
                )
                .await?;

                return Ok(());
            };

            let command_input = CommandInputData::from((**command).clone());

            let channel_id = command_input
                .parse_field::<Id<ChannelMarker>>("channel")?
                .ok_or_else(|| anyhow::anyhow!("The jail channel option was not provided"))?;

            match configure_jail_channel(http, pool, guild_id, channel_id).await {
                Ok(updated_count) => {
                    respond(
                        http,
                        application_id,
                        &interaction,
                        &format!(
                            "Jail channel configured successfully. Updated permissions in {updated_count} existing channels."
                        ),
                    )
                    .await?;
                }

                Err(error) => {
                    tracing::error!(?error, "Jail channel configuration failed");

                    respond(
                        http,
                        application_id,
                        &interaction,
                        "Failed to configure the jail channel. Check that the bot has Manage Channels permission.",
                    )
                    .await?;
                }
            }
        }

        "jail" => {
            let input = CommandInputData::from((**command).clone());

            let target_id = input
                .parse_field::<Id<UserMarker>>("member")?
                .context("The member option is missing")?;

            let duration = input.parse_field::<String>("duration")?;
            let reason = input.parse_field::<String>("reason")?;

            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let moderator_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            defer_interaction(http, application_id, &interaction).await?;

            let result: anyhow::Result<String> = async {
                ensure_moderator(http, guild_id, moderator_id, target_id).await?;

                let config = crate::database::config_repository::get_guild_config(
                    pool,
                    &guild_id.to_string(),
                )
                .await?
                .context("Run /jailsetup and /setjail before using /jail")?;

                let prisoner_role_id = Id::<RoleMarker>::new(
                    config
                        .prisoner_role_id
                        .as_deref()
                        .context("Prisoner role is not configured")?
                        .parse::<u64>()?,
                );

                let jail_channel_id = Id::<ChannelMarker>::new(
                    config
                        .jail_channel_id
                        .as_deref()
                        .context("Jail channel is not configured")?
                        .parse::<u64>()?,
                );

                let release_at = match duration {
                    Some(value) => {
                        Some(crate::services::duration::parse_duration(&value)?)
                    }
                    None => None,
                };

                let record_id = crate::services::discord_moderation::jail_member(
                    http,
                    pool,
                    guild_id,
                    target_id,
                    moderator_id,
                    prisoner_role_id,
                    jail_channel_id,
                    reason,
                    release_at,
                )
                .await?;

                Ok(format!(
                    "Member <@{target_id}> has been jailed successfully.\nJail record: `{record_id}`"
                ))
            }
            .await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not jail member: {error:#}"),
            };

            edit_deferred_response(http, application_id, &interaction, &message).await?;
        }

        "unjail" => {
            let input = CommandInputData::from((**command).clone());

            let target_id = input
                .parse_field::<Id<UserMarker>>("member")?
                .context("The member option is missing")?;

            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let moderator_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            defer_interaction(http, application_id, &interaction).await?;

            let result: anyhow::Result<String> = async {
                ensure_moderator(http, guild_id, moderator_id, target_id).await?;

                let config = crate::database::config_repository::get_guild_config(
                    pool,
                    &guild_id.to_string(),
                )
                .await?
                .context("Jail configuration was not found")?;

                let prisoner_role_id = Id::<RoleMarker>::new(
                    config
                        .prisoner_role_id
                        .as_deref()
                        .context("Prisoner role is not configured")?
                        .parse::<u64>()?,
                );

                let record_id = crate::services::discord_moderation::release_member(
                    http,
                    pool,
                    guild_id,
                    target_id,
                    moderator_id,
                    prisoner_role_id,
                )
                .await?;

                Ok(format!(
                    "Member <@{target_id}> has been released successfully.\nJail record: `{record_id}`"
                ))
            }
            .await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not release member: {error:#}"),
            };

            edit_deferred_response(http, application_id, &interaction, &message).await?;
        }

        "setlogs" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let moderator_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            let input = CommandInputData::from((**command).clone());

            let channel_id = input
                .parse_field::<Id<ChannelMarker>>("channel")?
                .context("The log channel option is missing")?;

            defer_interaction(http, application_id, &interaction).await?;

            let result: anyhow::Result<String> = async {
                ensure_permission(
                    http,
                    guild_id,
                    moderator_id,
                    Permissions::MANAGE_CHANNELS,
                    "You need Manage Channels permission to configure the log channel.",
                )
                .await?;

                let guild_id_string = guild_id.to_string();
                let channel_id_string = channel_id.to_string();

                crate::database::config_repository::set_logs_channel(
                    pool,
                    &guild_id_string,
                    Some(&channel_id_string),
                )
                .await?;

                Ok(format!("Moderation log channel set to <#{channel_id}>."))
            }
            .await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not configure the log channel: {error:#}"),
            };

            edit_deferred_response(http, application_id, &interaction, &message).await?;
        }

        "prisoners" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let moderator_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            defer_interaction(http, application_id, &interaction).await?;

            let result: anyhow::Result<String> = async {
                ensure_permission(
                    http,
                    guild_id,
                    moderator_id,
                    Permissions::MANAGE_ROLES,
                    "You need Manage Roles permission to view the prisoner list.",
                )
                .await?;

                let service = crate::services::JailService::new(pool.clone());
                let records = service.list_prisoners(&guild_id.to_string()).await?;

                if records.is_empty() {
                    return Ok("There are currently no jailed members.".to_owned());
                }

                let total = records.len();
                let mut message = format!("**Currently jailed members ({total})**\n");

                for record in records.iter().take(8) {
                    let reason = record.reason.as_deref().unwrap_or("No reason provided");
                    let reason = reason.replace('\n', " ");
                    let reason: String = reason.chars().take(80).collect();
                    let release = record
                        .release_at
                        .as_deref()
                        .map(|value| format!(" | releases: `{value}`"))
                        .unwrap_or_else(|| " | indefinite".to_owned());

                    message.push_str(&format!(
                        "\n• <@{}> — {}{}",
                        record.user_id, reason, release
                    ));
                }

                if total > 8 {
                    message.push_str(&format!("\n…and {} more.", total - 8));
                }

                Ok(message)
            }
            .await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not retrieve the prisoner list: {error:#}"),
            };

            edit_deferred_response(http, application_id, &interaction, &message).await?;
        }

        "jailinfo" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let moderator_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            let input = CommandInputData::from((**command).clone());

            let target_id = input
                .parse_field::<Id<UserMarker>>("member")?
                .context("The member option is missing")?;

            defer_interaction(http, application_id, &interaction).await?;

            let result: anyhow::Result<String> = async {
                ensure_permission(
                    http,
                    guild_id,
                    moderator_id,
                    Permissions::MANAGE_ROLES,
                    "You need Manage Roles permission to view jail information.",
                )
                .await?;

                let service = crate::services::JailService::new(pool.clone());
                let history = service
                    .get_history(&guild_id.to_string(), &target_id.to_string())
                    .await?;

                if history.is_empty() {
                    return Ok(format!("No jail history found for <@{target_id}>."));
                }

                let mut message = format!("**Jail history for <@{target_id}>**\n");

                for record in history.iter().take(5) {
                    let reason = record.reason.as_deref().unwrap_or("No reason provided");
                    let release = record.release_at.as_deref().unwrap_or("Indefinite");
                    let released = record.released_at.as_deref().unwrap_or("Not released");

                    message.push_str(&format!(
                        "\n**Record #{} — {}**\nJailed: `{}`\nRelease scheduled: `{}`\nReleased: `{}`\nReason: {}\n",
                        record.id,
                        record.status,
                        record.jailed_at,
                        release,
                        released,
                        reason
                    ));

                    if let Some(released_by) = record.released_by.as_deref() {
                        message.push_str(&format!("Released by: <@{released_by}>\n"));
                    }
                }

                if history.len() > 5 {
                    message.push_str(&format!(
                        "\nShowing the latest 5 of {} records.",
                        history.len()
                    ));
                }

                Ok(message)
            }
            .await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not retrieve jail information: {error:#}"),
            };

            edit_deferred_response(http, application_id, &interaction, &message).await?;
        }

        "jailconfig" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let moderator_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            defer_interaction(http, application_id, &interaction).await?;

            let result: anyhow::Result<String> = async {
                ensure_permission(
                    http,
                    guild_id,
                    moderator_id,
                    Permissions::MANAGE_ROLES,
                    "You need Manage Roles permission to view the jail configuration.",
                )
                .await?;

                let config = crate::database::config_repository::get_guild_config(
                    pool,
                    &guild_id.to_string(),
                )
                .await?;

                let Some(config) = config else {
                    return Ok(
                        "This server has no jail configuration yet. Run `/jailsetup` first."
                            .to_owned(),
                    );
                };

                let prisoner_role = config
                    .prisoner_role_id
                    .as_deref()
                    .map(|id| format!("<@&{id}>"))
                    .unwrap_or_else(|| "Not configured".to_owned());

                let jail_channel = config
                    .jail_channel_id
                    .as_deref()
                    .map(|id| format!("<#{id}>"))
                    .unwrap_or_else(|| "Not configured".to_owned());

                let logs_channel = config
                    .logs_channel_id
                    .as_deref()
                    .map(|id| format!("<#{id}>"))
                    .unwrap_or_else(|| "Not configured".to_owned());

                Ok(format!(
                    "**Jail Configuration**\nPrisoner role: {prisoner_role}\nJail channel: {jail_channel}\nLogs channel: {logs_channel}\nSetup complete: {}\nAutomatic release: {}",
                    if config.setup_complete { "Enabled" } else { "Incomplete" },
                    if config.auto_release_enabled { "Enabled" } else { "Disabled" },
                ))
            }
            .await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not retrieve jail configuration: {error:#}"),
            };

            edit_deferred_response(http, application_id, &interaction, &message).await?;
        }

        _ => {}
    }

    Ok(())
}

async fn setup_prisoner_role(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
) -> anyhow::Result<String> {
    let roles = http.roles(guild_id).await?.models().await?;

    let role = match roles
        .into_iter()
        .find(|role| role.name.eq_ignore_ascii_case("Prisoner"))
    {
        Some(role) => role,

        None => {
            http.create_role(guild_id)
                .name("Prisoner")
                .permissions(Permissions::empty())
                .await?
                .model()
                .await?
        }
    };

    let guild_id_string = guild_id.to_string();
    let role_id_string = role.id.to_string();

    crate::database::config_repository::set_prisoner_role(
        pool,
        &guild_id_string,
        Some(&role_id_string),
    )
    .await?;

    Ok(role_id_string)
}

async fn configure_jail_channel(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    jail_channel_id: Id<ChannelMarker>,
) -> anyhow::Result<usize> {
    let guild_id_string = guild_id.to_string();

    let config = crate::database::config_repository::get_guild_config(pool, &guild_id_string)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Run /jailsetup first to create the Prisoner role"))?;

    let prisoner_role_id = config
        .prisoner_role_id
        .ok_or_else(|| anyhow::anyhow!("Prisoner role is not configured. Run /jailsetup first"))?;

    let prisoner_role_id = prisoner_role_id
        .parse::<u64>()
        .context("Invalid saved Prisoner role ID")?;

    let prisoner_role_id = Id::<RoleMarker>::new(prisoner_role_id);

    let channels = http.guild_channels(guild_id).await?.models().await?;

    let selected_channel = channels
        .iter()
        .find(|channel| channel.id == jail_channel_id)
        .ok_or_else(|| anyhow::anyhow!("Selected channel does not belong to this server"))?;

    if selected_channel.kind != ChannelType::GuildText {
        bail!("The jail channel must be a normal text channel");
    }

    let mut updated_count = 0;

    for channel in &channels {
        let overwrite = if channel.id == jail_channel_id {
            PermissionOverwrite {
                allow: Some(
                    Permissions::VIEW_CHANNEL
                        | Permissions::SEND_MESSAGES
                        | Permissions::READ_MESSAGE_HISTORY,
                ),
                deny: Some(
                    Permissions::ATTACH_FILES
                        | Permissions::EMBED_LINKS
                        | Permissions::CREATE_PUBLIC_THREADS
                        | Permissions::CREATE_PRIVATE_THREADS
                        | Permissions::SEND_MESSAGES_IN_THREADS
                        | Permissions::SEND_VOICE_MESSAGES
                        | Permissions::SEND_POLLS,
                ),
                id: prisoner_role_id.cast(),
                kind: PermissionOverwriteType::Role,
            }
        } else {
            PermissionOverwrite {
                allow: Some(Permissions::empty()),
                deny: Some(Permissions::VIEW_CHANNEL),
                id: prisoner_role_id.cast(),
                kind: PermissionOverwriteType::Role,
            }
        };

        ratelimit::guard(
            Lane::Overwrite,
            http.update_channel_permission(channel.id, &overwrite),
        )
        .await
        .with_context(|| format!("Failed to update permissions for channel {}", channel.id))?;

        updated_count += 1;
    }

    let jail_channel_string = jail_channel_id.to_string();

    crate::database::config_repository::set_jail_channel(
        pool,
        &guild_id_string,
        Some(&jail_channel_string),
    )
    .await?;

    crate::database::config_repository::set_setup_complete(pool, &guild_id_string, true).await?;

    crate::services::discord_moderation::reconcile_guild(http, pool, guild_id).await?;

    Ok(updated_count)
}

async fn respond(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    content: &str,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(content.to_owned()),
            ..Default::default()
        }),
    };

    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id)
            .create_response(interaction.id, &interaction.token, &response),
    )
    .await?;

    Ok(())
}
async fn defer_interaction(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: None,
    };

    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id)
            .create_response(interaction.id, &interaction.token, &response),
    )
    .await?;

    Ok(())
}

async fn edit_deferred_response(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    content: &str,
) -> anyhow::Result<()> {
    ratelimit::guard(
        Lane::Message,
        http.interaction(application_id)
            .update_response(&interaction.token)
            .content(Some(content)),
    )
    .await?;

    Ok(())
}

async fn ensure_moderator(
    http: &HttpClient,
    guild_id: Id<GuildMarker>,
    moderator_id: Id<UserMarker>,
    target_id: Id<UserMarker>,
) -> anyhow::Result<()> {
    let guild = http.guild(guild_id).await?.model().await?;

    if moderator_id == target_id {
        bail!("You cannot moderate yourself.");
    }

    // The server owner can moderate any member.
    if moderator_id == guild.owner_id {
        return Ok(());
    }

    let moderator = http
        .guild_member(guild_id, moderator_id)
        .await?
        .model()
        .await?;

    let target = http
        .guild_member(guild_id, target_id)
        .await?
        .model()
        .await?;

    let roles = http.roles(guild_id).await?.models().await?;

    let everyone_id: Id<RoleMarker> = guild_id.cast();

    let mut moderator_permissions = Permissions::empty();

    if let Some(everyone) = roles.iter().find(|role| role.id == everyone_id) {
        moderator_permissions |= everyone.permissions;
    }

    let mut moderator_highest = 0_i64;

    for role_id in &moderator.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            moderator_permissions |= role.permissions;
            moderator_highest = moderator_highest.max(role.position);
        }
    }

    if !moderator_permissions.contains(Permissions::ADMINISTRATOR)
        && !moderator_permissions.contains(Permissions::MANAGE_ROLES)
    {
        bail!("You need Manage Roles permission to use this command.");
    }

    let mut target_highest = 0_i64;

    for role_id in &target.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            target_highest = target_highest.max(role.position);
        }
    }

    if target_highest >= moderator_highest {
        bail!("You cannot moderate a member with an equal or higher role.");
    }

    Ok(())
}

async fn ensure_permission(
    http: &HttpClient,
    guild_id: Id<GuildMarker>,
    member_id: Id<UserMarker>,
    required: Permissions,
    error_message: &str,
) -> anyhow::Result<()> {
    let guild = http.guild(guild_id).await?.model().await?;

    if member_id == guild.owner_id {
        return Ok(());
    }

    let member = http
        .guild_member(guild_id, member_id)
        .await?
        .model()
        .await?;

    let roles = http.roles(guild_id).await?.models().await?;
    let everyone_id: Id<RoleMarker> = guild_id.cast();

    let mut permissions = Permissions::empty();

    if let Some(everyone) = roles.iter().find(|role| role.id == everyone_id) {
        permissions |= everyone.permissions;
    }

    for role_id in &member.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            permissions |= role.permissions;
        }
    }

    if !permissions.contains(Permissions::ADMINISTRATOR) && !permissions.contains(required) {
        bail!("{error_message}");
    }

    Ok(())
}
