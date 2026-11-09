use crate::database::repository;
use anyhow::{Context, bail};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use twilight_http::Client as HttpClient;
use twilight_model::{
    channel::permission_overwrite::PermissionOverwriteType as ChannelPermissionOverwriteType,
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    },
};

use crate::database::repository::{NewJail, OverwriteSnapshotInput, SavedRoleInput};
use crate::services::JailService;
use crate::services::ratelimit::{self, Lane};

const ABSENT: &str = "ABSENT";

/// Upper bound for a single reconciliation pass. Remaining repairs are picked up
/// by the next sweep, so one large server cannot monopolise the event loop.
const MAX_WRITES_PER_PASS: usize = 120;
const MAX_PASS_DURATION: Duration = Duration::from_secs(10);

fn budget_exhausted(writes: usize, started: Instant) -> bool {
    writes >= MAX_WRITES_PER_PASS || started.elapsed() >= MAX_PASS_DURATION
}

pub async fn jail_member(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    target_id: Id<UserMarker>,
    moderator_id: Id<UserMarker>,
    prisoner_role_id: Id<RoleMarker>,
    jail_channel_id: Id<ChannelMarker>,
    reason: Option<String>,
    release_at: Option<String>,
) -> anyhow::Result<i64> {
    // Fetch guild and verify target.
    let guild = http.guild(guild_id).await?.model().await?;

    if guild.owner_id == target_id {
        bail!("The server owner cannot be jailed.");
    }

    let bot_user = http.current_user().await?.model().await?;

    if bot_user.id == target_id {
        bail!("The bot cannot jail itself.");
    }

    let target = http
        .guild_member(guild_id, target_id)
        .await?
        .model()
        .await
        .context("Could not retrieve the target member")?;

    let bot_member = http
        .guild_member(guild_id, bot_user.id)
        .await?
        .model()
        .await
        .context("Could not retrieve the bot's server membership")?;

    let roles = http.roles(guild_id).await?.models().await?;

    let active = crate::database::repository::get_active_jail(
        pool,
        &guild_id.to_string(),
        &target_id.to_string(),
    )
    .await?;

    if active.is_some() {
        bail!("This member is already jailed.");
    }

    // Find the @everyone role.
    let everyone_id: Id<RoleMarker> = guild_id.cast();

    let everyone = roles
        .iter()
        .find(|role| role.id == everyone_id)
        .context("Could not find the @everyone role")?;

    if everyone.permissions.contains(Permissions::ADMINISTRATOR) {
        bail!("The @everyone role has Administrator. The bot cannot safely restrict members.");
    }

    // Calculate the bot's highest role position and permissions.
    let mut bot_highest_position = 0_i64;
    let mut bot_permissions = everyone.permissions;

    for role_id in &bot_member.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            bot_highest_position = bot_highest_position.max(role.position);
            bot_permissions |= role.permissions;
        }
    }

    if !bot_permissions.contains(Permissions::ADMINISTRATOR)
        && !bot_permissions.contains(Permissions::MANAGE_ROLES)
    {
        bail!("The bot needs Manage Roles permission.");
    }

    if !bot_permissions.contains(Permissions::ADMINISTRATOR)
        && !bot_permissions.contains(Permissions::MANAGE_CHANNELS)
    {
        bail!("The bot needs Manage Channels permission to change channel overwrites.");
    }

    let prisoner_role = roles
        .iter()
        .find(|role| role.id == prisoner_role_id)
        .context("The configured Prisoner role no longer exists")?;

    if prisoner_role
        .permissions
        .contains(Permissions::ADMINISTRATOR)
    {
        bail!("The Prisoner role must not have Administrator permission.");
    }

    if prisoner_role.position >= bot_highest_position {
        bail!("Move the bot's highest role above the Prisoner role.");
    }

    // Check target roles and prepare snapshots.
    let mut saved_roles = Vec::new();

    for role_id in &target.roles {
        if *role_id == prisoner_role_id {
            continue;
        }

        let role = roles
            .iter()
            .find(|role| role.id == *role_id)
            .context("A member role could not be found")?;

        if role.managed && role.permissions.contains(Permissions::ADMINISTRATOR) {
            bail!("The member has a managed Administrator role. They cannot be safely jailed.");
        }

        if !role.managed && role.position >= bot_highest_position {
            bail!("The bot's highest role must be above every removable role held by the target.");
        }

        saved_roles.push(SavedRoleInput {
            role_id: role.id.to_string(),
            role_position: role.position,
            restorable: !role.managed,
        });
    }

    // Fetch channels and snapshot existing member overwrites.
    let channels = http.guild_channels(guild_id).await?.models().await?;

    if !channels.iter().any(|channel| channel.id == jail_channel_id) {
        bail!("The configured jail channel no longer exists.");
    }

    let mut overwrite_snapshots = Vec::new();

    for channel in &channels {
        let existing = channel
            .permission_overwrites
            .as_ref()
            .and_then(|overwrites| {
                overwrites.iter().find(|overwrite| {
                    overwrite.id == target_id.cast()
                        && overwrite.kind
                            == twilight_model::channel::permission_overwrite::PermissionOverwriteType::Member
                })
            });

        let (allow_bits, deny_bits) = match existing {
            Some(overwrite) => (
                overwrite.allow.bits().to_string(),
                overwrite.deny.bits().to_string(),
            ),
            None => (ABSENT.to_string(), ABSENT.to_string()),
        };

        overwrite_snapshots.push(OverwriteSnapshotInput {
            channel_id: channel.id.to_string(),
            allow_bits,
            deny_bits,
        });
    }

    // Create the database record BEFORE changing Discord.
    let log_reason = reason.as_deref().unwrap_or("No reason provided").to_owned();

    let jail = NewJail {
        guild_id: guild_id.to_string(),
        user_id: target_id.to_string(),
        moderator_id: moderator_id.to_string(),
        reason,
        release_at,
        original_nickname: target.nick.clone(),
        roles: saved_roles,
        overwrites: overwrite_snapshots,
    };

    let record_id = JailService::new(pool.clone()).create_record(jail).await?;

    // Remove manageable roles.
    for role_id in &target.roles {
        if *role_id == prisoner_role_id {
            continue;
        }

        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            if !role.managed {
                ratelimit::guard(
                    Lane::Member,
                    http.remove_guild_member_role(guild_id, target_id, role.id),
                )
                .await?;
            }
        }
    }

    // Assign the Prisoner role.
    if !target.roles.contains(&prisoner_role_id) {
        ratelimit::guard(
            Lane::Member,
            http.add_guild_member_role(guild_id, target_id, prisoner_role_id),
        )
        .await?;
    }

    // Mark the member's nickname.
    let original_name = target.nick.as_deref().unwrap_or(target.user.name.as_str());

    let jailed_nickname: String = format!("[JAILED] {original_name}")
        .chars()
        .take(32)
        .collect();

    ratelimit::guard(
        Lane::Member,
        http.update_guild_member(guild_id, target_id)
            .nick(Some(&jailed_nickname)),
    )
    .await?;

    // Apply member-specific restrictions in every channel.
    for channel in &channels {
        let (allow, deny) = if channel.id == jail_channel_id {
            (
                Permissions::VIEW_CHANNEL
                    | Permissions::SEND_MESSAGES
                    | Permissions::READ_MESSAGE_HISTORY,
                Permissions::ATTACH_FILES
                    | Permissions::EMBED_LINKS
                    | Permissions::CREATE_PUBLIC_THREADS
                    | Permissions::CREATE_PRIVATE_THREADS
                    | Permissions::SEND_MESSAGES_IN_THREADS
                    | Permissions::SEND_VOICE_MESSAGES
                    | Permissions::SEND_POLLS,
            )
        } else {
            (Permissions::empty(), Permissions::VIEW_CHANNEL)
        };

        let overwrite = PermissionOverwrite {
            allow: Some(allow),
            deny: Some(deny),
            id: target_id.cast(),
            kind: PermissionOverwriteType::Member,
        };

        ratelimit::guard(
            Lane::Overwrite,
            http.update_channel_permission(channel.id, &overwrite),
        )
        .await?;
    }

    tracing::info!(
        guild_id = %guild_id,
        user_id = %target_id,
        record_id,
        "Member jailed successfully"
    );

    if let Err(error) = send_moderation_log(
        http,
        pool,
        guild_id,
        &format!("**Member jailed**\nMember: <@{target_id}>\nModerator: <@{moderator_id}>\nReason: {log_reason}\nRecord: `{record_id}`"),
    )
    .await
    {
        tracing::warn!(?error, "Could not send jail log message");
    }

    Ok(record_id)
}
pub async fn release_member(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    target_id: Id<UserMarker>,
    moderator_id: Id<UserMarker>,
    prisoner_role_id: Id<RoleMarker>,
) -> anyhow::Result<i64> {
    let moderator_id_string = moderator_id.to_string();

    release_member_internal(
        http,
        pool,
        guild_id,
        target_id,
        &moderator_id_string,
        prisoner_role_id,
        false,
    )
    .await
}

pub async fn expire_member(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    target_id: Id<UserMarker>,
    prisoner_role_id: Id<RoleMarker>,
) -> anyhow::Result<i64> {
    release_member_internal(
        http,
        pool,
        guild_id,
        target_id,
        "SYSTEM",
        prisoner_role_id,
        true,
    )
    .await
}

async fn release_member_internal(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    target_id: Id<UserMarker>,
    released_by: &str,
    prisoner_role_id: Id<RoleMarker>,
    expired: bool,
) -> anyhow::Result<i64> {
    let guild_id_string = guild_id.to_string();
    let target_id_string = target_id.to_string();

    // Find the active jail record.
    let record = repository::get_active_jail(pool, &guild_id_string, &target_id_string)
        .await?
        .context("This member is not currently jailed")?;

    // Retrieve the member and bot.
    let target = http
        .guild_member(guild_id, target_id)
        .await?
        .model()
        .await
        .context("Could not retrieve the jailed member")?;

    let bot_user = http.current_user().await?.model().await?;

    let bot_member = http
        .guild_member(guild_id, bot_user.id)
        .await?
        .model()
        .await?;

    let roles = http.roles(guild_id).await?.models().await?;

    // Calculate the bot's highest role.
    let mut bot_highest_position = 0_i64;

    for role_id in &bot_member.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            bot_highest_position = bot_highest_position.max(role.position);
        }
    }

    // Load saved roles.
    let saved_roles = repository::get_saved_roles(pool, record.id).await?;

    let mut roles_to_restore = Vec::new();

    // Check hierarchy before making changes.
    for saved in &saved_roles {
        if !saved.restorable {
            continue;
        }

        let role_id = Id::<RoleMarker>::new(
            saved
                .role_id
                .parse::<u64>()
                .context("Invalid saved role ID")?,
        );

        let Some(role) = roles.iter().find(|role| role.id == role_id) else {
            // The role was deleted from Discord.
            continue;
        };

        if role.managed {
            continue;
        }

        if role.position >= bot_highest_position {
            bail!(
                "Cannot restore role {}: move the bot's highest role above it.",
                role.name
            );
        }

        roles_to_restore.push(role_id);
    }

    // Restore original channel permission overwrites.
    let snapshots = repository::get_overwrite_snapshots(pool, record.id).await?;

    let channels = http.guild_channels(guild_id).await?.models().await?;

    for snapshot in &snapshots {
        let channel_id = Id::<ChannelMarker>::new(
            snapshot
                .channel_id
                .parse::<u64>()
                .context("Invalid saved channel ID")?,
        );

        // Ignore channels that no longer exist.
        if !channels.iter().any(|channel| channel.id == channel_id) {
            continue;
        }

        if snapshot.allow_bits == "ABSENT" && snapshot.deny_bits == "ABSENT" {
            ratelimit::guard(
                Lane::Overwrite,
                http.delete_channel_permission(channel_id).member(target_id),
            )
            .await?;
            continue;
        }

        let allow_bits = snapshot.allow_bits.parse::<u64>()?;
        let deny_bits = snapshot.deny_bits.parse::<u64>()?;

        let overwrite = PermissionOverwrite {
            allow: Some(Permissions::from_bits_retain(allow_bits)),
            deny: Some(Permissions::from_bits_retain(deny_bits)),
            id: target_id.cast(),
            kind: PermissionOverwriteType::Member,
        };

        ratelimit::guard(
            Lane::Overwrite,
            http.update_channel_permission(channel_id, &overwrite),
        )
        .await?;
    }

    // Restore the original nickname.
    ratelimit::guard(
        Lane::Member,
        http.update_guild_member(guild_id, target_id)
            .nick(record.original_nickname.as_deref()),
    )
    .await?;

    // Restore the saved roles.
    for role_id in roles_to_restore {
        if !target.roles.contains(&role_id) {
            ratelimit::guard(
                Lane::Member,
                http.add_guild_member_role(guild_id, target_id, role_id),
            )
            .await?;
        }
    }

    // Remove the Prisoner role last.
    if target.roles.contains(&prisoner_role_id) {
        ratelimit::guard(
            Lane::Member,
            http.remove_guild_member_role(guild_id, target_id, prisoner_role_id),
        )
        .await?;
    }

    // Only mark the record released after Discord restoration succeeds.
    let released = JailService::new(pool.clone())
        .complete_release(&guild_id_string, &target_id_string, released_by, expired)
        .await?;

    released.context("Discord restoration completed, but the database release update failed")?;

    tracing::info!(
        guild_id = %guild_id,
        user_id = %target_id,
        record_id = record.id,
        expired,
        "Member release completed successfully"
    );

    let action = if expired {
        "sentence expired"
    } else {
        "member released"
    };
    if let Err(error) = send_moderation_log(
        http,
        pool,
        guild_id,
        &format!(
            "**Member {action}**\nMember: <@{target_id}>\nRecord: `{}`\nActor: `{released_by}`",
            record.id
        ),
    )
    .await
    {
        tracing::warn!(?error, "Could not send release log message");
    }

    Ok(record.id)
}

/// Sends a moderation message to the configured log channel.
/// A missing log channel is not an error; logging is simply skipped.
pub async fn send_moderation_log(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    message: &str,
) -> anyhow::Result<()> {
    let Some(config) =
        crate::database::config_repository::get_guild_config(pool, &guild_id.to_string()).await?
    else {
        return Ok(());
    };

    let Some(channel_id) = config.logs_channel_id else {
        return Ok(());
    };

    let channel_id = Id::<ChannelMarker>::new(
        channel_id
            .parse::<u64>()
            .context("Invalid configured log channel ID")?,
    );

    ratelimit::guard(Lane::Message, http.create_message(channel_id).content(message)).await?;

    Ok(())
}

/// Expires a prisoner who has already left the server.
/// Their roles and nickname no longer exist in the guild, but channel overwrites
/// must still be restored before the database record is closed.
pub async fn expire_absent_member(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    target_id: Id<UserMarker>,
) -> anyhow::Result<Option<i64>> {
    let guild_string = guild_id.to_string();
    let user_string = target_id.to_string();

    let Some(record) = repository::get_active_jail(pool, &guild_string, &user_string).await? else {
        return Ok(None);
    };

    let snapshots = repository::get_overwrite_snapshots(pool, record.id).await?;
    let channels = http.guild_channels(guild_id).await?.models().await?;

    for snapshot in snapshots {
        let channel_id = Id::<ChannelMarker>::new(
            snapshot
                .channel_id
                .parse::<u64>()
                .context("Invalid saved channel ID")?,
        );

        if !channels.iter().any(|channel| channel.id == channel_id) {
            continue;
        }

        if snapshot.allow_bits == "ABSENT" && snapshot.deny_bits == "ABSENT" {
            ratelimit::guard(
                Lane::Overwrite,
                http.delete_channel_permission(channel_id).member(target_id),
            )
            .await?;
        } else {
            let allow = Permissions::from_bits_retain(snapshot.allow_bits.parse::<u64>()?);
            let deny = Permissions::from_bits_retain(snapshot.deny_bits.parse::<u64>()?);

            let overwrite = PermissionOverwrite {
                allow: Some(allow),
                deny: Some(deny),
                id: target_id.cast(),
                kind: PermissionOverwriteType::Member,
            };

            ratelimit::guard(
                Lane::Overwrite,
                http.update_channel_permission(channel_id, &overwrite),
            )
            .await?;
        }
    }

    let released = JailService::new(pool.clone())
        .complete_release(&guild_string, &user_string, "SYSTEM", true)
        .await?;

    if let Some(record_id) = released {
        if let Err(error) = send_moderation_log(
            http,
            pool,
            guild_id,
            &format!("**Sentence expired while member was absent**\nMember: <@{target_id}>\nRecord: `{record_id}`"),
        )
        .await
        {
            tracing::warn!(?error, "Could not send absent-member expiry log");
        }
    }

    Ok(released)
}

/// Reconciles one server against all active jail records.
/// This repairs incomplete operations after restarts, API failures, channel changes,
/// and members returning to the server.
pub async fn reconcile_guild(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
) -> anyhow::Result<usize> {
    let guild_id_string = guild_id.to_string();

    let Some(config) =
        crate::database::config_repository::get_guild_config(pool, &guild_id_string).await?
    else {
        return Ok(0);
    };

    if !config.setup_complete {
        return Ok(0);
    }

    let prisoner_role_id = config
        .prisoner_role_id
        .as_deref()
        .context("Prisoner role is not configured")?
        .parse::<u64>()
        .context("Invalid saved Prisoner role ID")?;
    let prisoner_role_id = Id::<RoleMarker>::new(prisoner_role_id);

    let jail_channel_id = config
        .jail_channel_id
        .as_deref()
        .context("Jail channel is not configured")?
        .parse::<u64>()
        .context("Invalid saved jail channel ID")?;
    let jail_channel_id = Id::<ChannelMarker>::new(jail_channel_id);

    let roles = http.roles(guild_id).await?.models().await?;
    let prisoner_role = roles
        .iter()
        .find(|role| role.id == prisoner_role_id)
        .context("The configured Prisoner role no longer exists")?;

    if prisoner_role
        .permissions
        .contains(Permissions::ADMINISTRATOR)
    {
        bail!("The Prisoner role must not have Administrator permission");
    }

    let bot_user = http.current_user().await?.model().await?;
    let bot_member = http
        .guild_member(guild_id, bot_user.id)
        .await?
        .model()
        .await?;

    let everyone_id: Id<RoleMarker> = guild_id.cast();
    let everyone = roles
        .iter()
        .find(|role| role.id == everyone_id)
        .context("Could not find the @everyone role")?;

    if everyone.permissions.contains(Permissions::ADMINISTRATOR) {
        bail!(
            "The @everyone role has Administrator permission; members cannot be safely restricted"
        );
    }

    let mut bot_permissions = everyone.permissions;
    let mut bot_highest_position = 0_i64;

    for role_id in &bot_member.roles {
        if let Some(role) = roles.iter().find(|role| role.id == *role_id) {
            bot_permissions |= role.permissions;
            bot_highest_position = bot_highest_position.max(role.position);
        }
    }

    if !bot_permissions.contains(Permissions::ADMINISTRATOR)
        && (!bot_permissions.contains(Permissions::MANAGE_ROLES)
            || !bot_permissions.contains(Permissions::MANAGE_CHANNELS))
    {
        bail!(
            "The bot needs both Manage Roles and Manage Channels permissions to reconcile jail records"
        );
    }

    if prisoner_role.position >= bot_highest_position {
        bail!("Move the bot's highest role above the Prisoner role");
    }

    let records = repository::list_active_jails(pool, &guild_id_string).await?;
    let channels = ratelimit::guard(Lane::Read, http.guild_channels(guild_id))
        .await?
        .models()
        .await?;
    let service = JailService::new(pool.clone());

    // Which channels already have a saved snapshot per record. Loading this once
    // keeps a converged sweep free of database writes.
    let mut saved_channels: HashMap<i64, HashSet<String>> = HashMap::with_capacity(records.len());

    for record in &records {
        let snapshots = repository::get_overwrite_snapshots(pool, record.id).await?;

        saved_channels.insert(
            record.id,
            snapshots
                .into_iter()
                .map(|snapshot| snapshot.channel_id)
                .collect(),
        );
    }

    // First restore the member-side restrictions and remove roles that may have
    // been re-added while the bot was offline.
    let mut members_repaired = 0_usize;

    for record in &records {
        let target_id = Id::<UserMarker>::new(
            record
                .user_id
                .parse::<u64>()
                .context("Invalid jailed user ID")?,
        );

        let target = match ratelimit::guard(Lane::Read, http.guild_member(guild_id, target_id)).await
        {
            Ok(response) => match response.model().await {
                Ok(member) => Some(member),
                Err(error) => {
                    tracing::warn!(?error, user_id = %target_id, "Could not read jailed member during reconciliation");
                    None
                }
            },
            Err(error) => {
                // A member may have left while the bot was offline. Keep the
                // active record and retry on the next reconciliation.
                tracing::debug!(?error, user_id = %target_id, "Jailed member is not currently retrievable");
                None
            }
        };

        let Some(target) = target else {
            continue;
        };

        let mut member_changed = false;

        for role_id in &target.roles {
            if *role_id == prisoner_role_id {
                continue;
            }

            let Some(role) = roles.iter().find(|role| role.id == *role_id) else {
                continue;
            };

            if role.managed {
                if role.permissions.contains(Permissions::ADMINISTRATOR) {
                    bail!(
                        "Cannot safely restrict <@{}>: they have a managed Administrator role",
                        target_id
                    );
                }
                continue;
            }

            if role.position >= bot_highest_position {
                bail!(
                    "Cannot remove role `{}` from <@{}>: move the bot's highest role above it",
                    role.name,
                    target_id
                );
            }

            ratelimit::guard(
                Lane::Member,
                http.remove_guild_member_role(guild_id, target_id, role.id),
            )
            .await
            .with_context(|| {
                format!("Failed to remove role `{}` from jailed member", role.name)
            })?;
            member_changed = true;
        }

        if !target.roles.contains(&prisoner_role_id) {
            ratelimit::guard(
                Lane::Member,
                http.add_guild_member_role(guild_id, target_id, prisoner_role_id),
            )
            .await
            .context("Failed to reapply Prisoner role during reconciliation")?;
            member_changed = true;
        }

        let base_name = record
            .original_nickname
            .as_deref()
            .unwrap_or(target.user.name.as_str());

        let jailed_nickname: String = format!("[JAILED] {base_name}").chars().take(32).collect();

        if target.nick.as_deref() != Some(jailed_nickname.as_str()) {
            ratelimit::guard(
                Lane::Member,
                http.update_guild_member(guild_id, target_id)
                    .nick(Some(&jailed_nickname)),
            )
            .await
            .context("Failed to restore jailed nickname during reconciliation")?;
            member_changed = true;
        }

        if member_changed {
            members_repaired += 1;
        }

        if record.left_server {
            if service
                .member_rejoined(&guild_id_string, &record.user_id)
                .await?
            {
                if let Err(error) = send_moderation_log(
                    http,
                    pool,
                    guild_id,
                    &format!("**Prisoner rejoined**\nMember: <@{target_id}>\nRestrictions reapplied. Record: `{}`", record.id),
                )
                .await
                {
                    tracing::warn!(?error, "Could not send rejoin log message");
                }
            }
        }
    }

    // Enforce the Prisoner role and member-specific restrictions in every
    // existing channel. Snapshots are inserted only once, before the overwrite
    // is changed, so later releases can restore the original state.
    let pass_started = Instant::now();
    let mut updated = 0_usize;
    let mut writes = 0_usize;

    for channel in &channels {
        if budget_exhausted(writes, pass_started) {
            break;
        }

        let channel_id_string = channel.id.to_string();
        let (role_allow, role_deny) = if channel.id == jail_channel_id {
            (
                Permissions::VIEW_CHANNEL
                    | Permissions::SEND_MESSAGES
                    | Permissions::READ_MESSAGE_HISTORY,
                Permissions::ATTACH_FILES
                    | Permissions::EMBED_LINKS
                    | Permissions::CREATE_PUBLIC_THREADS
                    | Permissions::CREATE_PRIVATE_THREADS
                    | Permissions::SEND_MESSAGES_IN_THREADS
                    | Permissions::SEND_VOICE_MESSAGES
                    | Permissions::SEND_POLLS,
            )
        } else {
            (Permissions::empty(), Permissions::VIEW_CHANNEL)
        };

        let current_role_overwrite =
            channel
                .permission_overwrites
                .as_ref()
                .and_then(|overwrites| {
                    overwrites.iter().find(|overwrite| {
                        overwrite.id == prisoner_role_id.cast()
                            && overwrite.kind == ChannelPermissionOverwriteType::Role
                    })
                });

        let role_matches = current_role_overwrite
            .is_some_and(|overwrite| overwrite.allow == role_allow && overwrite.deny == role_deny);

        if !role_matches {
            let overwrite = PermissionOverwrite {
                allow: Some(role_allow),
                deny: Some(role_deny),
                id: prisoner_role_id.cast(),
                kind: PermissionOverwriteType::Role,
            };

            ratelimit::guard(
                Lane::Overwrite,
                http.update_channel_permission(channel.id, &overwrite),
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to repair Prisoner role permissions in channel {}",
                    channel.id
                )
            })?;
            updated += 1;
            writes += 1;
        }

        for record in &records {
            if budget_exhausted(writes, pass_started) {
                break;
            }

            let target_id = Id::<UserMarker>::new(
                record
                    .user_id
                    .parse::<u64>()
                    .context("Invalid jailed user ID")?,
            );

            let current_member_overwrite =
                channel
                    .permission_overwrites
                    .as_ref()
                    .and_then(|overwrites| {
                        overwrites.iter().find(|overwrite| {
                            overwrite.id == target_id.cast()
                                && overwrite.kind == ChannelPermissionOverwriteType::Member
                        })
                    });

            let snapshot_bits = current_member_overwrite
                .map(|overwrite| (overwrite.allow.bits(), overwrite.deny.bits()));

            // A snapshot is written once, before the overwrite is first changed.
            // Channels that already have one are skipped entirely.
            let known_snapshot = saved_channels
                .get(&record.id)
                .is_some_and(|channels| channels.contains(&channel_id_string));

            if !known_snapshot {
                let (allow_bits, deny_bits) = match snapshot_bits {
                    Some((allow, deny)) => (Some(allow), Some(deny)),
                    None => (None, None),
                };

                sqlx::query(
                    "INSERT OR IGNORE INTO member_overwrite_snapshots
                     (jail_record_id, channel_id, allow_bits, deny_bits)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(record.id)
                .bind(&channel_id_string)
                .bind(
                    allow_bits
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| ABSENT.to_owned()),
                )
                .bind(
                    deny_bits
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| ABSENT.to_owned()),
                )
                .execute(pool)
                .await
                .context("Failed to save a channel overwrite before reconciliation")?;

                saved_channels
                    .entry(record.id)
                    .or_default()
                    .insert(channel_id_string.clone());
            }

            let (member_allow, member_deny) = if channel.id == jail_channel_id {
                (
                    Permissions::VIEW_CHANNEL
                        | Permissions::SEND_MESSAGES
                        | Permissions::READ_MESSAGE_HISTORY,
                    Permissions::ATTACH_FILES
                        | Permissions::EMBED_LINKS
                        | Permissions::CREATE_PUBLIC_THREADS
                        | Permissions::CREATE_PRIVATE_THREADS
                        | Permissions::SEND_MESSAGES_IN_THREADS
                        | Permissions::SEND_VOICE_MESSAGES
                        | Permissions::SEND_POLLS,
                )
            } else {
                (Permissions::empty(), Permissions::VIEW_CHANNEL)
            };

            let member_matches = current_member_overwrite.is_some_and(|overwrite| {
                overwrite.allow == member_allow && overwrite.deny == member_deny
            });

            if !member_matches {
                let overwrite = PermissionOverwrite {
                    allow: Some(member_allow),
                    deny: Some(member_deny),
                    id: target_id.cast(),
                    kind: PermissionOverwriteType::Member,
                };

                ratelimit::guard(
                    Lane::Overwrite,
                    http.update_channel_permission(channel.id, &overwrite),
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed to repair jailed member permissions in channel {}",
                        channel.id
                    )
                })?;
                updated += 1;
                writes += 1;
            }
        }
    }

    if updated > 0 || members_repaired > 0 {
        if let Err(error) = send_moderation_log(
            http,
            pool,
            guild_id,
            &format!(
                "**Jail state repaired**\nMembers repaired: `{members_repaired}`\nPermission entries updated: `{updated}`"
            ),
        )
        .await
        {
            tracing::warn!(?error, "Could not send reconciliation log message");
        }
    }

    Ok(updated)
}
