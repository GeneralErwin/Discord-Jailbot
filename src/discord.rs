use anyhow::{Context, bail};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::commands::moderation::{
    ChangelogCommand, DisableModuleCommand, EnableModuleCommand, HelpCommand, JailCommand,
    JailConfigCommand, JailInfoCommand, PrisonerCommand, PrisonersCommand, QuarantineCommand,
    SanitizeCommand, SetCommand, SetLogsCommand, SetModeratorCommands, UnjailCommand,
};
use crate::services::ratelimit::{self, Lane};

use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client as HttpClient;
use twilight_interactions::command::{CommandInputData, CommandModel, CreateCommand};

use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::ChannelType,
    channel::message::{Component, MessageFlags},
    channel::permission_overwrite::PermissionOverwriteType as ChannelOverwriteType,
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
    default_permissions = "admin_permissions"
)]
struct JailSetupCommand;

#[derive(CreateCommand)]
#[command(
    name = "setjail",
    desc = "Set the channel prisoners can access",
    default_permissions = "admin_permissions"
)]
struct SetJailCommand {
    /// The text channel prisoners will be allowed to use.
    #[command(channel_types = "guild_text")]
    channel: Id<ChannelMarker>,
}

fn admin_permissions() -> Permissions {
    Permissions::ADMINISTRATOR
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
        PrisonerCommand::create_command().into(),
        HelpCommand::create_command().into(),
        ChangelogCommand::create_command().into(),
        QuarantineCommand::create_command().into(),
        EnableModuleCommand::create_command().into(),
        DisableModuleCommand::create_command().into(),
        SanitizeCommand::create_command().into(),
        SetCommand::create_command().into(),
    ];

    http.interaction(application_id)
        .set_global_commands(&commands)
        .await?;

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Global slash commands registered"
    );

    let changelog_pool = pool.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = crate::services::github_changelog::sync(&changelog_pool).await {
                tracing::warn!(?error, "GitHub changelog synchronization failed");
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    let mut shard = Shard::new(
        ShardId::ONE,
        token,
        Intents::GUILDS
            | Intents::GUILD_MEMBERS
            | Intents::GUILD_MODERATION
            | Intents::GUILD_WEBHOOKS
            | Intents::GUILD_MESSAGES,
    );

    let mut maintenance_tick = tokio::time::interval(Duration::from_secs(30));
    let mut reconciliation_tick = tokio::time::interval(Duration::from_secs(300));

    // Gateway events only mark a server as dirty. Repairs are coalesced and
    // paced, so a burst of member or channel changes cannot turn into one full
    // guild sweep per event.
    let mut repair_tick = tokio::time::interval(REPAIR_TICK);
    let mut pending_repairs: HashSet<u64> = HashSet::new();
    let mut last_repair: HashMap<u64, Instant> = HashMap::new();
    let mut quarantine_runtime = crate::services::quarantine::Runtime::default();

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

                    Ok(Event::GuildAuditLogEntryCreate(event)) => {
                        if let Some(guild_id) = event.0.guild_id {
                            if let Err(error) = crate::services::quarantine::handle_audit_entry(
                                &http, &pool, guild_id, &event.0, &mut quarantine_runtime
                            ).await {
                                tracing::error!(?error, guild_id = %guild_id, "Quarantine audit response failed");
                            }
                        }
                    }

                    Ok(Event::MessageCreate(event)) => {
                        if let Err(error) = crate::services::quarantine::handle_webhook_message(
                            &http, &pool, &event, &mut quarantine_runtime
                        ).await {
                            tracing::error!(?error, "Webhook raid containment failed");
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

        match crate::services::discord_moderation::reconcile_guild(http, pool, guild_id).await {
            // The pass hit its budget: queue the rest for the next allowed pass
            // instead of waiting for the periodic sweep.
            Ok(outcome) if outcome.truncated => {
                pending.insert(raw_guild_id);
            }
            Ok(outcome) => {
                if outcome.updated > 0 {
                    tracing::debug!(
                        guild_id = %guild_id,
                        updated = outcome.updated,
                        "Jail state repaired"
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    guild_id = %guild_id,
                    "Event-triggered jail reconciliation failed"
                );

                // Transient failures retry on the next allowed pass.
                pending.insert(raw_guild_id);
            }
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
    let Some(data) = interaction.data.as_ref() else {
        return Ok(());
    };
    let command = match data {
        InteractionData::MessageComponent(component) => {
            return handle_component_interaction(
                http,
                application_id,
                &interaction,
                &component.custom_id,
                &component.values,
                pool,
            )
            .await;
        }
        InteractionData::ApplicationCommand(command) => command,
        _ => return Ok(()),
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

        "help" => {
            respond_embed(
                http,
                application_id,
                &interaction,
                crate::embeds::help(),
                crate::embeds::standard_components(),
            )
            .await?;
        }

        "changelog" => {
            defer_interaction(http, application_id, &interaction).await?;
            let (embed, components) = match changelog_page(pool, 0).await {
                Ok(page) => page,
                Err(error) => (
                    crate::embeds::response(&format!("Could not load the changelog: {error:#}")),
                    crate::embeds::standard_components(),
                ),
            };
            edit_deferred_response_embed(http, application_id, &interaction, embed, components)
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

            if let Err(error) = ensure_admin_or_owner(
                http,
                guild_id,
                interaction
                    .author_id()
                    .context("Could not identify the user")?,
            )
            .await
            {
                respond(
                    http,
                    application_id,
                    &interaction,
                    &format!("You cannot configure the jail system: {error:#}"),
                )
                .await?;
                return Ok(());
            }

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

            if let Err(error) = ensure_admin_or_owner(
                http,
                guild_id,
                interaction
                    .author_id()
                    .context("Could not identify the user")?,
            )
            .await
            {
                respond(
                    http,
                    application_id,
                    &interaction,
                    &format!("You cannot configure the jail system: {error:#}"),
                )
                .await?;
                return Ok(());
            }

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
                ensure_moderator(http, pool, guild_id, moderator_id, target_id).await?;

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

            match result {
                Ok(message) => {
                    edit_deferred_response_with_components(
                        http,
                        application_id,
                        &interaction,
                        &message,
                        crate::embeds::jail_release_components(guild_id.get(), target_id.get()),
                    )
                    .await?;
                }
                Err(error) => {
                    let message = format!("Could not jail member: {error:#}");
                    edit_deferred_response(http, application_id, &interaction, &message).await?;
                }
            }
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
                ensure_moderator(http, pool, guild_id, moderator_id, target_id).await?;

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

        "set" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;
            let actor_id = interaction
                .author_id()
                .context("Could not identify the user")?;
            let input = CommandInputData::from((**command).clone());
            let parsed = SetCommand::from_interaction(input)?;
            let SetCommand::Moderator(SetModeratorCommands::Role(set_role)) = parsed;

            let result: anyhow::Result<String> = async {
                ensure_admin_or_owner(http, guild_id, actor_id).await?;
                let roles = ratelimit::guard(Lane::Read, http.roles(guild_id)).await?.models().await?;
                let role = roles.iter().find(|role| role.id == set_role.role)
                    .context("That role does not belong to this server")?;
                let everyone_id: Id<RoleMarker> = guild_id.cast();
                if role.id == everyone_id {
                    bail!("The @everyone role cannot be used as the moderator role");
                }
                crate::database::config_repository::set_moderator_role(
                    pool, &guild_id.to_string(), Some(&role.id.to_string())
                ).await?;
                Ok(format!("Moderator role set to <@&{}>. Members with this role can jail members whose highest role is below theirs.", role.id))
            }.await;

            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not set the moderator role: {error:#}"),
            };
            respond(http, application_id, &interaction, &message).await?;
        }

        "quarantine" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;
            let actor_id = interaction
                .author_id()
                .context("Could not identify the user")?;
            let result: anyhow::Result<(bool, Vec<(&'static str, &'static str, bool)>)> = async {
                ensure_admin_or_owner(http, guild_id, actor_id).await?;
                let guild_string = guild_id.to_string();
                let config =
                    crate::database::config_repository::ensure_guild_config(pool, &guild_string)
                        .await?;
                let enabling = !config.quarantine_enabled;
                if enabling {
                    crate::database::config_repository::initialize_quarantine_modules(
                        pool,
                        &guild_string,
                        &crate::services::quarantine::module_keys(),
                    )
                    .await?;
                }
                crate::database::config_repository::set_quarantine_enabled(
                    pool,
                    &guild_string,
                    enabling,
                )
                .await?;
                quarantine_view(pool, guild_id).await
            }
            .await;
            match result {
                Ok((service_enabled, modules)) => {
                    respond_embed(
                        http,
                        application_id,
                        &interaction,
                        crate::embeds::quarantine_status_embed(service_enabled, &modules),
                        crate::embeds::quarantine_status_components(guild_id.get()),
                    )
                    .await?;
                }
                Err(error) => {
                    respond(
                        http,
                        application_id,
                        &interaction,
                        &format!("Could not update quarantine: {error:#}"),
                    )
                    .await?;
                }
            }
        }

        "enable" | "disable" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;
            let actor_id = interaction
                .author_id()
                .context("Could not identify the user")?;
            let enabled = command.name == "enable";
            let input = CommandInputData::from((**command).clone());
            let raw_module = if enabled {
                EnableModuleCommand::from_interaction(input)?.module
            } else {
                DisableModuleCommand::from_interaction(input)?.module
            };
            let result: anyhow::Result<(bool, Vec<(&'static str, &'static str, bool)>)> = async {
                ensure_admin_or_owner(http, guild_id, actor_id).await?;
                let key =
                    crate::services::quarantine::find_module_key(&raw_module).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Unknown module. Run `/quarantine` to see valid module names."
                        )
                    })?;
                crate::database::config_repository::set_quarantine_module(
                    pool,
                    &guild_id.to_string(),
                    key,
                    enabled,
                )
                .await?;
                quarantine_view(pool, guild_id).await
            }
            .await;
            match result {
                Ok((service_enabled, modules)) => {
                    respond_embed(
                        http,
                        application_id,
                        &interaction,
                        crate::embeds::quarantine_modules_embed(service_enabled, &modules),
                        crate::embeds::quarantine_module_components(guild_id.get(), &modules),
                    )
                    .await?;
                }
                Err(error) => {
                    respond(
                        http,
                        application_id,
                        &interaction,
                        &format!("Could not change quarantine module: {error:#}"),
                    )
                    .await?;
                }
            }
        }

        "sanitize" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;
            let actor_id = interaction
                .author_id()
                .context("Could not identify the user")?;
            defer_interaction(http, application_id, &interaction).await?;
            let result: anyhow::Result<String> = async {
                ensure_admin_or_owner(http, guild_id, actor_id).await?;
                let config = crate::database::config_repository::get_guild_config(
                    pool,
                    &guild_id.to_string(),
                )
                .await?
                .context("Run `/jailsetup` and `/setjail` first")?;
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
                let updated =
                    sanitize_jail_permissions(http, guild_id, prisoner_role_id, jail_channel_id)
                        .await?;
                Ok(format!(
                    "Sanitize complete. Corrected Prisoner role permissions in {updated} channels."
                ))
            }
            .await;
            let message = match result {
                Ok(message) => message,
                Err(error) => format!("Could not sanitize channel permissions: {error:#}"),
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
                ensure_admin_or_owner(http, guild_id, moderator_id).await?;

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

        "prisoner" | "prisoners" => {
            let guild_id = interaction
                .guild_id
                .context("This command can only be used in a server")?;

            let owner_id = interaction
                .author_id()
                .context("Could not identify the moderator")?;

            defer_ephemeral_interaction(http, application_id, &interaction).await?;

            let result = async {
                ensure_permission(
                    http,
                    guild_id,
                    owner_id,
                    Permissions::MANAGE_ROLES,
                    "You need Manage Roles permission to view the prisoner list.",
                )
                .await?;

                crate::prisoner_ui::page(http, pool, guild_id, owner_id, 1).await
            }
            .await;

            match result {
                Ok(page) => {
                    edit_deferred_prisoner_page(http, application_id, &interaction, page).await?;
                }
                Err(error) => {
                    edit_deferred_response(
                        http,
                        application_id,
                        &interaction,
                        &format!("Could not retrieve the prisoner list: {error:#}"),
                    )
                    .await?;
                }
            }
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
                ensure_admin_or_owner(http, guild_id, moderator_id).await?;

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
                let moderator_role = config
                    .moderator_role_id
                    .as_deref()
                    .map(|id| format!("<@&{id}>"))
                    .unwrap_or_else(|| "Not configured".to_owned());

                Ok(format!(
                    "**Jail Configuration**\nPrisoner role: {prisoner_role}\nJail channel: {jail_channel}\nLogs channel: {logs_channel}\nModerator role: {moderator_role}\nQuarantine service: {}\nSetup complete: {}\nAutomatic release: {}",
                    if config.quarantine_enabled { "Enabled" } else { "Disabled" },
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

    let channels = ratelimit::guard(Lane::Read, http.guild_channels(guild_id))
        .await?
        .models()
        .await?;

    let selected_channel = channels
        .iter()
        .find(|channel| channel.id == jail_channel_id)
        .ok_or_else(|| anyhow::anyhow!("Selected channel does not belong to this server"))?;

    if selected_channel.kind != ChannelType::GuildText {
        bail!("The jail channel must be a normal text channel");
    }

    let mut updated_count = 0;

    for channel in &channels {
        let (allow, deny) =
            crate::services::discord_moderation::prisoner_overwrite(channel.id, jail_channel_id);

        let overwrite = PermissionOverwrite {
            allow: Some(allow),
            deny: Some(deny),
            id: prisoner_role_id.cast(),
            kind: PermissionOverwriteType::Role,
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

async fn sanitize_jail_permissions(
    http: &HttpClient,
    guild_id: Id<GuildMarker>,
    prisoner_role_id: Id<RoleMarker>,
    jail_channel_id: Id<ChannelMarker>,
) -> anyhow::Result<usize> {
    let channels = ratelimit::guard(Lane::Read, http.guild_channels(guild_id))
        .await?
        .models()
        .await?;
    let mut corrected = 0;
    for channel in &channels {
        let (allow, deny) =
            crate::services::discord_moderation::prisoner_overwrite(channel.id, jail_channel_id);
        let current = channel
            .permission_overwrites
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|overwrite| {
                overwrite.kind == ChannelOverwriteType::Role
                    && overwrite.id == prisoner_role_id.cast()
            });
        let already_correct =
            current.is_some_and(|overwrite| overwrite.allow == allow && overwrite.deny == deny);
        if already_correct {
            continue;
        }

        let overwrite = PermissionOverwrite {
            allow: Some(allow),
            deny: Some(deny),
            id: prisoner_role_id.cast(),
            kind: PermissionOverwriteType::Role,
        };
        ratelimit::guard(
            Lane::Overwrite,
            http.update_channel_permission(channel.id, &overwrite),
        )
        .await
        .with_context(|| format!("Could not repair permissions for channel {}", channel.id))?;
        corrected += 1;
    }
    Ok(corrected)
}

async fn handle_component_interaction(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    custom_id: &str,
    values: &[String],
    pool: &SqlitePool,
) -> anyhow::Result<()> {
    if custom_id.starts_with("prisoner:") {
        return handle_prisoner_component(
            http,
            application_id,
            interaction,
            custom_id,
            values,
            pool,
        )
        .await;
    }

    match custom_id {
        "bot:help" => {
            return respond_component_ephemeral_embed(
                http,
                application_id,
                interaction,
                crate::embeds::help(),
                crate::embeds::standard_components(),
            )
            .await;
        }
        "bot:changelog" => match changelog_page(pool, 0).await {
            Ok((embed, components)) => {
                return respond_component_ephemeral_embed(
                    http,
                    application_id,
                    interaction,
                    embed,
                    components,
                )
                .await;
            }
            Err(error) => {
                return respond_component_ephemeral(
                    http,
                    application_id,
                    interaction,
                    &format!("Could not load the changelog: {error:#}"),
                )
                .await;
            }
        },
        _ => {}
    }

    if custom_id.starts_with("changelog:page:") {
        let offset = custom_id
            .strip_prefix("changelog:page:")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        match changelog_page(pool, offset).await {
            Ok((embed, components)) => {
                return update_component_message(
                    http,
                    application_id,
                    interaction,
                    embed,
                    components,
                )
                .await;
            }
            Err(error) => {
                return respond_component_ephemeral(
                    http,
                    application_id,
                    interaction,
                    &format!("Could not load the changelog: {error:#}"),
                )
                .await;
            }
        }
    }

    if custom_id.starts_with("quarantine:") {
        let mut parts = custom_id.splitn(4, ':');
        let _ = parts.next();
        let action = parts.next().unwrap_or_default();
        let expected_guild_id = parts.next().and_then(|value| value.parse::<u64>().ok());
        let Some(guild_id) = interaction.guild_id else {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                "Quarantine controls can only be used in a server.",
            )
            .await;
        };
        if expected_guild_id != Some(guild_id.get()) {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                "This control belongs to a different server.",
            )
            .await;
        }
        let actor_id = interaction
            .author_id()
            .context("Could not identify the user")?;
        if let Err(error) = ensure_admin_or_owner(http, guild_id, actor_id).await {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                &format!("You cannot configure quarantine: {error:#}"),
            )
            .await;
        }

        let module_key = if action == "toggle" {
            parts
                .next()
                .and_then(crate::services::quarantine::find_module_key)
        } else {
            None
        };
        if action == "toggle" && module_key.is_none() {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                "That quarantine module is not available.",
            )
            .await;
        }
        if !matches!(action, "configure" | "status" | "toggle") {
            return Ok(());
        }

        defer_component_update(http, application_id, interaction).await?;
        let result: anyhow::Result<(twilight_model::channel::message::Embed, Vec<Component>)> =
            async {
                if let Some(key) = module_key {
                    let enabled = crate::database::config_repository::quarantine_module_enabled(
                        pool,
                        &guild_id.to_string(),
                        key,
                    )
                    .await?;
                    crate::database::config_repository::set_quarantine_module(
                        pool,
                        &guild_id.to_string(),
                        key,
                        !enabled,
                    )
                    .await?;
                }
                let (service_enabled, modules) = quarantine_view(pool, guild_id).await?;
                if action == "configure" {
                    Ok((
                        crate::embeds::quarantine_modules_embed(service_enabled, &modules),
                        crate::embeds::quarantine_module_components(guild_id.get(), &modules),
                    ))
                } else if action == "status" {
                    Ok((
                        crate::embeds::quarantine_status_embed(service_enabled, &modules),
                        crate::embeds::quarantine_status_components(guild_id.get()),
                    ))
                } else {
                    Ok((
                        crate::embeds::quarantine_modules_embed(service_enabled, &modules),
                        crate::embeds::quarantine_module_components(guild_id.get(), &modules),
                    ))
                }
            }
            .await;
        match result {
            Ok((embed, components)) => {
                edit_deferred_response_embed(http, application_id, interaction, embed, components)
                    .await
            }
            Err(error) => {
                edit_deferred_response(
                    http,
                    application_id,
                    interaction,
                    &format!("Could not update quarantine controls: {error:#}"),
                )
                .await
            }
        }?;
        return Ok(());
    }

    if let Some(rest) = custom_id.strip_prefix("jail:release:") {
        let mut ids = rest.split(':');
        let expected_guild_id = ids.next().and_then(|value| value.parse::<u64>().ok());
        let target_id = ids.next().and_then(|value| value.parse::<u64>().ok());
        let Some(guild_id) = interaction.guild_id else {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                "Jail controls can only be used in a server.",
            )
            .await;
        };
        if expected_guild_id != Some(guild_id.get()) || target_id.is_none() || ids.next().is_some()
        {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                "This release control is invalid or belongs to another server.",
            )
            .await;
        }
        let target_id = Id::<UserMarker>::new(target_id.unwrap_or_default());
        let moderator_id = interaction
            .author_id()
            .context("Could not identify the moderator")?;
        if let Err(error) = ensure_moderator(http, pool, guild_id, moderator_id, target_id).await {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                &format!("You cannot release this member: {error:#}"),
            )
            .await;
        }
        let config =
            match crate::database::config_repository::get_guild_config(pool, &guild_id.to_string())
                .await
            {
                Ok(Some(config)) => config,
                Ok(None) => {
                    return respond_component_ephemeral(
                        http,
                        application_id,
                        interaction,
                        "Jail configuration is missing; ask an administrator to run `/jailsetup`.",
                    )
                    .await;
                }
                Err(error) => {
                    return respond_component_ephemeral(
                        http,
                        application_id,
                        interaction,
                        &format!("Could not load jail configuration: {error:#}"),
                    )
                    .await;
                }
            };
        let Some(role_id) = config
            .prisoner_role_id
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return respond_component_ephemeral(
                http,
                application_id,
                interaction,
                "The Prisoner role is not configured.",
            )
            .await;
        };

        defer_component_update(http, application_id, interaction).await?;
        match crate::services::discord_moderation::release_member(
            http,
            pool,
            guild_id,
            target_id,
            moderator_id,
            Id::<RoleMarker>::new(role_id),
        )
        .await
        {
            Ok(record_id) => {
                update_deferred_components(
                    http,
                    application_id,
                    interaction,
                    crate::embeds::standard_components(),
                )
                .await?;
                send_ephemeral_followup(
                    http,
                    application_id,
                    interaction,
                    &format!(
                        "Member <@{target_id}> released successfully. Jail record: `{record_id}`"
                    ),
                )
                .await?;
            }
            Err(error) => {
                send_ephemeral_followup(
                    http,
                    application_id,
                    interaction,
                    &format!("Could not release member: {error:#}"),
                )
                .await?;
            }
        }
        return Ok(());
    }

    Ok(())
}

async fn handle_prisoner_component(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    custom_id: &str,
    values: &[String],
    pool: &SqlitePool,
) -> anyhow::Result<()> {
    let parts = custom_id.split(':').collect::<Vec<_>>();
    if parts.len() < 4 || parts[0] != "prisoner" {
        return respond_component_ephemeral(
            http,
            application_id,
            interaction,
            "This prisoner control is invalid.",
        )
        .await;
    }
    let action = parts[1];
    let expected_guild_id = parts[2].parse::<u64>().ok();
    let owner_id = parts[3].parse::<u64>().ok();
    let Some(guild_id) = interaction.guild_id else {
        return respond_component_ephemeral(
            http,
            application_id,
            interaction,
            "Prisoner controls can only be used inside a server.",
        )
        .await;
    };
    let actor_id = interaction
        .author_id()
        .context("Could not identify the user")?;
    if expected_guild_id != Some(guild_id.get()) || owner_id != Some(actor_id.get()) {
        return respond_component_ephemeral(
            http,
            application_id,
            interaction,
            "This private prisoner panel belongs to another server or user.",
        )
        .await;
    }

    defer_component_update(http, application_id, interaction).await?;
    if let Err(error) = ensure_permission(
        http,
        guild_id,
        actor_id,
        Permissions::MANAGE_ROLES,
        "You need Manage Roles permission to use the prisoner panel.",
    )
    .await
    {
        send_ephemeral_followup(
            http,
            application_id,
            interaction,
            &format!("You cannot use this prisoner panel: {error:#}"),
        )
        .await?;
        return Ok(());
    }

    let requested_page = parts
        .get(4)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);

    let action_result: anyhow::Result<()> = async {
        match action {
            "page" => {
                let page = crate::prisoner_ui::page(http, pool, guild_id, actor_id, requested_page)
                    .await?;
                edit_deferred_prisoner_page(http, application_id, interaction, page).await?;
            }
            "choose_page" => {
                let selected_page = values
                    .first()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1);
                let page =
                    crate::prisoner_ui::page(http, pool, guild_id, actor_id, selected_page).await?;
                edit_deferred_prisoner_page(http, application_id, interaction, page).await?;
            }
            "inspect" | "release" => {
                let page = crate::prisoner_ui::page(http, pool, guild_id, actor_id, requested_page)
                    .await?;
                if page.records.is_empty() {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        "There are no active prisoners on this page.",
                    )
                    .await?;
                    return Ok(());
                }
                let title = if action == "inspect" {
                    "inspect"
                } else {
                    "release"
                };
                let embed = crate::prisoner_ui::selection_embed(
                    title,
                    page.page,
                    page.page_count,
                    page.total,
                );
                let mut embeds = page.embeds;
                if let Some(header) = embeds.first_mut() {
                    *header = embed;
                }
                let components = crate::prisoner_ui::selection_components(
                    guild_id,
                    actor_id,
                    page.page,
                    title,
                    &page.records,
                    &page.names,
                );
                edit_deferred_prisoner_panel(
                    http,
                    application_id,
                    interaction,
                    embeds,
                    components,
                    page.attachments,
                )
                .await?;
            }
            "select_inspect" | "select_release" => {
                let Some(raw_user_id) = values.first() else {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        "Choose a prisoner from the menu first.",
                    )
                    .await?;
                    return Ok(());
                };
                let target_id = match raw_user_id.parse::<u64>() {
                    Ok(id) => Id::<UserMarker>::new(id),
                    Err(_) => {
                        send_ephemeral_followup(
                            http,
                            application_id,
                            interaction,
                            "That prisoner selection is invalid.",
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let page = crate::prisoner_ui::page(http, pool, guild_id, actor_id, requested_page)
                    .await?;
                let Some(index) = page
                    .records
                    .iter()
                    .position(|record| record.user_id == target_id.get().to_string())
                else {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        "That member is no longer on this prisoner page.",
                    )
                    .await?;
                    return Ok(());
                };
                if action == "select_inspect" {
                    let embeds =
                        crate::prisoner_ui::inspection_embeds(http, pool, guild_id, target_id)
                            .await?;
                    let components = crate::prisoner_ui::inspect_components(
                        guild_id, actor_id, page.page, target_id,
                    );
                    edit_deferred_prisoner_panel(
                        http,
                        application_id,
                        interaction,
                        embeds,
                        components,
                        crate::prisoner_ui::terminal_attachment(),
                    )
                    .await?;
                } else {
                    let record = &page.records[index];
                    let (display_name, avatar) =
                        crate::prisoner_ui::profile(http, guild_id, target_id).await;
                    let embeds =
                        crate::prisoner_ui::confirm_release_embeds(record, &display_name, &avatar);
                    let components = crate::prisoner_ui::confirmation_components(
                        guild_id, actor_id, page.page, target_id,
                    );
                    edit_deferred_prisoner_panel(
                        http,
                        application_id,
                        interaction,
                        embeds,
                        components,
                        crate::prisoner_ui::terminal_attachment(),
                    )
                    .await?;
                }
            }
            "release_selected" => {
                let Some(target_id) = parts.get(5).and_then(|value| value.parse::<u64>().ok())
                else {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        "That release selection is invalid.",
                    )
                    .await?;
                    return Ok(());
                };
                show_release_confirmation(
                    http,
                    application_id,
                    interaction,
                    pool,
                    guild_id,
                    actor_id,
                    Id::new(target_id),
                    requested_page,
                )
                .await?;
            }
            "confirm_release" => {
                let Some(target_id) = parts.get(5).and_then(|value| value.parse::<u64>().ok())
                else {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        "That release confirmation is invalid.",
                    )
                    .await?;
                    return Ok(());
                };
                let target_id = Id::<UserMarker>::new(target_id);
                if let Err(error) =
                    ensure_moderator(http, pool, guild_id, actor_id, target_id).await
                {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        &format!("You cannot release this member: {error:#}"),
                    )
                    .await?;
                    return Ok(());
                }
                let config = crate::database::config_repository::get_guild_config(
                    pool,
                    &guild_id.to_string(),
                )
                .await?;
                let Some(role_id) = config
                    .and_then(|config| config.prisoner_role_id)
                    .and_then(|value| value.parse::<u64>().ok())
                else {
                    send_ephemeral_followup(
                        http,
                        application_id,
                        interaction,
                        "The Prisoner role is not configured.",
                    )
                    .await?;
                    return Ok(());
                };
                match crate::services::discord_moderation::release_member(
                    http,
                    pool,
                    guild_id,
                    target_id,
                    actor_id,
                    Id::<RoleMarker>::new(role_id),
                )
                .await
                {
                    Ok(_) => {
                        let page = crate::prisoner_ui::page(
                            http,
                            pool,
                            guild_id,
                            actor_id,
                            requested_page,
                        )
                        .await?;
                        edit_deferred_prisoner_page(http, application_id, interaction, page)
                            .await?;
                    }
                    Err(error) => {
                        send_ephemeral_followup(
                            http,
                            application_id,
                            interaction,
                            &format!("Could not release member: {error:#}"),
                        )
                        .await?;
                    }
                }
            }
            "back" => {
                let page = crate::prisoner_ui::page(http, pool, guild_id, actor_id, requested_page)
                    .await?;
                edit_deferred_prisoner_page(http, application_id, interaction, page).await?;
            }
            _ => {
                send_ephemeral_followup(
                    http,
                    application_id,
                    interaction,
                    "That prisoner control is not available.",
                )
                .await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = action_result {
        send_ephemeral_followup(
            http,
            application_id,
            interaction,
            &format!("Could not update the prisoner panel: {error:#}"),
        )
        .await?;
    }
    Ok(())
}

async fn show_release_confirmation(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    target_id: Id<UserMarker>,
    requested_page: usize,
) -> anyhow::Result<()> {
    let page = crate::prisoner_ui::page(http, pool, guild_id, owner_id, requested_page).await?;
    let Some(record) = page
        .records
        .iter()
        .find(|record| record.user_id == target_id.get().to_string())
    else {
        send_ephemeral_followup(
            http,
            application_id,
            interaction,
            "That member is no longer on this prisoner page.",
        )
        .await?;
        return Ok(());
    };
    let (display_name, avatar) = crate::prisoner_ui::profile(http, guild_id, target_id).await;
    let embeds = crate::prisoner_ui::confirm_release_embeds(record, &display_name, &avatar);
    let components =
        crate::prisoner_ui::confirmation_components(guild_id, owner_id, page.page, target_id);
    edit_deferred_prisoner_panel(
        http,
        application_id,
        interaction,
        embeds,
        components,
        crate::prisoner_ui::terminal_attachment(),
    )
    .await
}

async fn edit_deferred_prisoner_panel(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    embeds: Vec<twilight_model::channel::message::Embed>,
    components: Vec<Component>,
    attachments: Vec<twilight_model::http::attachment::Attachment>,
) -> anyhow::Result<()> {
    ratelimit::guard(
        Lane::Message,
        http.interaction(application_id)
            .update_response(&interaction.token)
            .embeds(Some(&embeds))
            .components(Some(&components))
            .attachments(&attachments)
            .content(None),
    )
    .await?;
    Ok(())
}

async fn quarantine_view(
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
) -> anyhow::Result<(bool, Vec<(&'static str, &'static str, bool)>)> {
    let guild_string = guild_id.to_string();
    let service_enabled = crate::database::config_repository::get_guild_config(pool, &guild_string)
        .await?
        .is_some_and(|config| config.quarantine_enabled);
    let saved =
        crate::database::config_repository::list_quarantine_modules(pool, &guild_string).await?;
    let modules = crate::services::quarantine::MODULES
        .iter()
        .map(|module| {
            let enabled = saved
                .iter()
                .find(|saved| saved.module_key == module.key)
                .is_some_and(|saved| saved.enabled);
            (module.key, module.label, enabled)
        })
        .collect();
    Ok((service_enabled, modules))
}

async fn changelog_page(
    pool: &SqlitePool,
    offset: i64,
) -> anyhow::Result<(twilight_model::channel::message::Embed, Vec<Component>)> {
    let offset = offset.clamp(0, 100_000);
    let entries = crate::database::changelog_repository::recent_entries(pool, 6, offset).await?;
    let has_older = entries.len() > 5;
    let embed = if entries.is_empty() {
        crate::embeds::changelog_empty()
    } else {
        crate::embeds::changelog(&entries[..entries.len().min(5)])
    };
    Ok((
        embed,
        crate::embeds::changelog_components(offset, has_older),
    ))
}

async fn update_component_message(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    embed: twilight_model::channel::message::Embed,
    components: Vec<Component>,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            components: Some(components),
            ..Default::default()
        }),
    };
    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id).create_response(
            interaction.id,
            &interaction.token,
            &response,
        ),
    )
    .await?;
    Ok(())
}

async fn defer_component_update(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::DeferredUpdateMessage,
        data: None,
    };
    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id).create_response(
            interaction.id,
            &interaction.token,
            &response,
        ),
    )
    .await?;
    Ok(())
}

async fn respond_component_ephemeral(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    message: &str,
) -> anyhow::Result<()> {
    respond_component_ephemeral_embed(
        http,
        application_id,
        interaction,
        crate::embeds::response(message),
        crate::embeds::standard_components(),
    )
    .await
}

async fn respond_component_ephemeral_embed(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    embed: twilight_model::channel::message::Embed,
    components: Vec<Component>,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            components: Some(components),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id).create_response(
            interaction.id,
            &interaction.token,
            &response,
        ),
    )
    .await?;
    Ok(())
}

async fn update_deferred_components(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    components: Vec<Component>,
) -> anyhow::Result<()> {
    ratelimit::guard(
        Lane::Message,
        http.interaction(application_id)
            .update_response(&interaction.token)
            .components(Some(&components)),
    )
    .await?;
    Ok(())
}

async fn send_ephemeral_followup(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    message: &str,
) -> anyhow::Result<()> {
    let embed = crate::embeds::response(message);
    ratelimit::guard(
        Lane::Message,
        http.interaction(application_id)
            .create_followup(&interaction.token)
            .embeds(&[embed])
            .components(&crate::embeds::standard_components())
            .flags(MessageFlags::EPHEMERAL),
    )
    .await?;
    Ok(())
}

async fn respond(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    content: &str,
) -> anyhow::Result<()> {
    respond_embed(
        http,
        application_id,
        interaction,
        crate::embeds::response(content),
        crate::embeds::standard_components(),
    )
    .await
}

async fn respond_embed(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    embed: twilight_model::channel::message::Embed,
    components: Vec<Component>,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            components: Some(components),
            ..Default::default()
        }),
    };

    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id).create_response(
            interaction.id,
            &interaction.token,
            &response,
        ),
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
        http.interaction(application_id).create_response(
            interaction.id,
            &interaction.token,
            &response,
        ),
    )
    .await?;

    Ok(())
}

async fn defer_ephemeral_interaction(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
) -> anyhow::Result<()> {
    let response = InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    ratelimit::guard(
        Lane::Interaction,
        http.interaction(application_id).create_response(
            interaction.id,
            &interaction.token,
            &response,
        ),
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
    edit_deferred_response_with_components(
        http,
        application_id,
        interaction,
        content,
        crate::embeds::standard_components(),
    )
    .await
}

async fn edit_deferred_response_with_components(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    content: &str,
    components: Vec<Component>,
) -> anyhow::Result<()> {
    edit_deferred_response_embed(
        http,
        application_id,
        interaction,
        crate::embeds::response(content),
        components,
    )
    .await
}

async fn edit_deferred_response_embed(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    embed: twilight_model::channel::message::Embed,
    components: Vec<Component>,
) -> anyhow::Result<()> {
    ratelimit::guard(
        Lane::Message,
        http.interaction(application_id)
            .update_response(&interaction.token)
            .embeds(Some(std::slice::from_ref(&embed)))
            .components(Some(&components))
            .content(None),
    )
    .await?;

    Ok(())
}

async fn edit_deferred_prisoner_page(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction: &Interaction,
    page: crate::prisoner_ui::PrisonerPage,
) -> anyhow::Result<()> {
    ratelimit::guard(
        Lane::Message,
        http.interaction(application_id)
            .update_response(&interaction.token)
            .embeds(Some(&page.embeds))
            .components(Some(&page.components))
            .attachments(&page.attachments)
            .content(None),
    )
    .await?;
    Ok(())
}

async fn ensure_moderator(
    http: &HttpClient,
    pool: &SqlitePool,
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

    if target_id == guild.owner_id {
        bail!("The server owner cannot be moderated.");
    }

    let config = crate::database::config_repository::get_guild_config(pool, &guild_id.to_string())
        .await?
        .context("Run `/set moderator role` to configure who may jail members")?;
    let moderator_role_id = config.moderator_role_id.as_deref()
        .context("The moderator role is not configured. Ask an administrator to run `/set moderator role`.")?
        .parse::<u64>()
        .context("The configured moderator role ID is invalid")?;
    let moderator_role_id = Id::<RoleMarker>::new(moderator_role_id);

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

    let mut moderator_highest = 0_i64;
    let mut has_moderator_role = false;

    for role_id in &moderator.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            moderator_highest = moderator_highest.max(role.position);
            has_moderator_role |= role.id == moderator_role_id;
        }
    }

    if !has_moderator_role {
        bail!("You need the configured Moderator role to jail or release members.");
    }

    let saved_target_highest = if let Some(record) = crate::database::repository::get_active_jail(
        pool,
        &guild_id.to_string(),
        &target_id.to_string(),
    )
    .await?
    {
        crate::database::repository::get_saved_roles(pool, record.id)
            .await?
            .iter()
            .map(|role| role.role_position)
            .max()
    } else {
        None
    };
    let mut target_highest = saved_target_highest.unwrap_or(0);
    if saved_target_highest.is_none() {
        for role_id in &target.roles {
            if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
                target_highest = target_highest.max(role.position);
            }
        }
    }

    if target_highest >= moderator_highest {
        bail!("You cannot moderate a member with an equal or higher role.");
    }

    Ok(())
}

async fn ensure_admin_or_owner(
    http: &HttpClient,
    guild_id: Id<GuildMarker>,
    member_id: Id<UserMarker>,
) -> anyhow::Result<()> {
    ensure_permission(
        http,
        guild_id,
        member_id,
        Permissions::ADMINISTRATOR,
        "Only the server owner or an administrator can change bot configuration.",
    )
    .await
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
