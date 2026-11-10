use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Context;
use sqlx::SqlitePool;
use twilight_http::Client as HttpClient;
use twilight_model::{
    channel::permission_overwrite::PermissionOverwriteType as ChannelOverwriteType,
    gateway::payload::incoming::MessageCreate,
    guild::{
        Permissions,
        audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType},
    },
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        Id,
        marker::{ChannelMarker, GenericMarker, GuildMarker, RoleMarker, UserMarker},
    },
};

use crate::{
    database::config_repository,
    services::ratelimit::{self, Lane},
};

pub struct ModuleDefinition {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

pub const MODULES: [ModuleDefinition; 7] = [
    ModuleDefinition {
        key: "dangerous_role_add",
        label: "Dangerous role added to a member",
        description: "Undo grants of roles carrying dangerous permissions.",
    },
    ModuleDefinition {
        key: "dangerous_role_remove",
        label: "Dangerous role removed from a member",
        description: "Restore dangerous roles removed from a member.",
    },
    ModuleDefinition {
        key: "role_permission_escalation",
        label: "Dangerous permissions added to a role",
        description: "Restore the role's previous permission set.",
    },
    ModuleDefinition {
        key: "channel_permission_escalation",
        label: "Dangerous permissions added to a channel",
        description: "Restore the previous channel permission overwrite.",
    },
    ModuleDefinition {
        key: "jailed_role_changes",
        label: "Roles changed for a jailed member",
        description: "Quarantine users who alter the roles of a jailed member.",
    },
    ModuleDefinition {
        key: "channel_deletion",
        label: "Channel deleted",
        description: "Quarantine the deleter and log the deleted channel.",
    },
    ModuleDefinition {
        key: "webhook_raid",
        label: "Webhook creation or mass-ping raid",
        description: "Delete webhooks and remove Manage Webhooks permissions.",
    },
];

const WEBHOOK_WINDOW: Duration = Duration::from_secs(30);
const WEBHOOK_CREATE_LIMIT: usize = 3;
const WEBHOOK_CONTAINMENT_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Runtime {
    webhook_creates: HashMap<u64, VecDeque<(Instant, Id<UserMarker>)>>,
    last_webhook_containment: HashMap<u64, Instant>,
}

pub fn module_keys() -> Vec<&'static str> {
    MODULES.iter().map(|module| module.key).collect()
}

pub fn find_module_key(input: &str) -> Option<&'static str> {
    let normalized = input.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    MODULES.iter().find_map(|module| {
        let key = module.key.replace('_', "-");
        let label = module.label.to_ascii_lowercase().replace([' ', '_'], "-");
        (normalized == key || normalized == label).then_some(module.key)
    })
}

pub async fn handle_audit_entry(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    entry: &AuditLogEntry,
    runtime: &mut Runtime,
) -> anyhow::Result<()> {
    let guild_string = guild_id.to_string();
    let Some(config) = config_repository::get_guild_config(pool, &guild_string).await? else {
        return Ok(());
    };
    if !config.quarantine_enabled {
        return Ok(());
    }

    let Some(actor_id) = entry.user_id else {
        return Ok(());
    };
    let bot_id = ratelimit::guard(Lane::Read, http.current_user())
        .await?
        .model()
        .await?
        .id;
    if actor_id == bot_id {
        return Ok(());
    }

    let mut quarantined_reason: Option<String> = None;
    let mut recovery = Vec::new();

    match entry.action_type {
        AuditLogEventType::MemberRoleUpdate => {
            let Some(target) = entry.target_id.map(|id| Id::<UserMarker>::new(id.get())) else {
                return Ok(());
            };
            let roles = ratelimit::guard(Lane::Read, http.roles(guild_id))
                .await?
                .models()
                .await?;
            let target_is_jailed = crate::database::repository::get_active_jail(
                pool,
                &guild_string,
                &target.to_string(),
            )
            .await?
            .is_some();

            if module_is_enabled(pool, &guild_string, "dangerous_role_add").await? {
                for change in &entry.changes {
                    if let AuditLogChange::RoleAdded { new, .. } = change {
                        for added in new {
                            let Some(role) = roles.iter().find(|role| role.id == added.id) else {
                                continue;
                            };
                            if role.permissions.intersects(dangerous_permissions()) {
                                quarantined_reason = Some(
                                    "gave a member a role with dangerous permissions".to_owned(),
                                );
                                match ratelimit::guard(
                                    Lane::Member,
                                    http.remove_guild_member_role(guild_id, target, role.id),
                                )
                                .await {
                                    Ok(_) => recovery.push(format!("Removed dangerous role `{}` from <@{target}>.", role.name)),
                                    Err(error) => recovery.push(format!("Could not remove dangerous role `{}` from <@{target}>: {error}", role.name)),
                                }
                            }
                        }
                    }
                }
            }

            if module_is_enabled(pool, &guild_string, "dangerous_role_remove").await? {
                for change in &entry.changes {
                    if let AuditLogChange::RoleRemoved { new, .. } = change {
                        for removed in new {
                            let Some(role) = roles.iter().find(|role| role.id == removed.id) else {
                                continue;
                            };
                            if role.permissions.intersects(dangerous_permissions()) {
                                quarantined_reason =
                                    Some("removed a dangerous role from a member".to_owned());
                                if target_is_jailed {
                                    recovery.push(format!("Left removed role `{}` off jailed member <@{target}>; jail reconciliation will keep the member restricted.", role.name));
                                } else {
                                    match ratelimit::guard(
                                        Lane::Member,
                                        http.add_guild_member_role(guild_id, target, role.id),
                                    ).await {
                                        Ok(_) => recovery.push(format!("Restored dangerous role `{}` to <@{target}>.", role.name)),
                                        Err(error) => recovery.push(format!("Could not restore dangerous role `{}` to <@{target}>: {error}", role.name)),
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if module_is_enabled(pool, &guild_string, "jailed_role_changes").await?
                && crate::database::repository::get_active_jail(
                    pool,
                    &guild_string,
                    &target.to_string(),
                )
                .await?
                .is_some()
            {
                quarantined_reason = Some("changed the roles of a jailed member".to_owned());
                recovery.push(format!("Queued jail state repair for <@{target}>."));
            }
        }

        AuditLogEventType::RoleCreate | AuditLogEventType::RoleUpdate => {
            if module_is_enabled(pool, &guild_string, "role_permission_escalation").await? {
                let Some(raw_role_id) = entry.target_id else {
                    return Ok(());
                };
                let role_id = Id::<RoleMarker>::new(raw_role_id.get());
                for change in &entry.changes {
                    if let AuditLogChange::Permissions { new, old } = change {
                        let new_permissions = new.unwrap_or(Permissions::empty());
                        let old_permissions = old.unwrap_or(Permissions::empty());
                        let granted = new_permissions & !old_permissions & dangerous_permissions();
                        if !granted.is_empty() {
                            quarantined_reason =
                                Some("added dangerous permissions to a role".to_owned());
                            match ratelimit::guard(
                                Lane::Member,
                                http.update_role(guild_id, role_id)
                                    .permissions(old_permissions),
                            )
                            .await
                            {
                                Ok(_) => recovery.push(format!(
                                    "Reverted dangerous permissions added to role <@&{role_id}>."
                                )),
                                Err(error) => recovery.push(format!(
                                    "Could not restore permissions on role <@&{role_id}>: {error}"
                                )),
                            }
                        }
                    }
                }
            }
        }

        AuditLogEventType::ChannelOverwriteCreate | AuditLogEventType::ChannelOverwriteUpdate => {
            if module_is_enabled(pool, &guild_string, "channel_permission_escalation").await? {
                let Some(channel_raw) = entry.target_id else {
                    return Ok(());
                };
                let channel_id = Id::<ChannelMarker>::new(channel_raw.get());
                let Some(options) = entry.options.as_ref() else {
                    return Ok(());
                };
                let Some(subject_raw) = options.id else {
                    return Ok(());
                };
                let mut old_allow = None;
                let mut old_deny = None;
                let mut new_allow = None;
                let mut new_deny = None;

                for change in &entry.changes {
                    match change {
                        AuditLogChange::Allow { new, old } => {
                            new_allow = *new;
                            old_allow = *old;
                        }
                        AuditLogChange::Deny { new, old } => {
                            new_deny = *new;
                            old_deny = *old;
                        }
                        _ => {}
                    }
                }
                let gained_allow = new_allow.unwrap_or(Permissions::empty())
                    & !old_allow.unwrap_or(Permissions::empty());
                let removed_deny = old_deny.unwrap_or(Permissions::empty())
                    & !new_deny.unwrap_or(Permissions::empty());
                if (gained_allow | removed_deny).intersects(dangerous_permissions()) {
                    quarantined_reason =
                        Some("granted dangerous permissions in a channel".to_owned());
                    let rollback = restore_channel_overwrite(
                        http,
                        channel_id,
                        subject_raw,
                        options.kind.as_deref().unwrap_or_default(),
                        old_allow,
                        old_deny,
                    )
                    .await;
                    match rollback {
                        Ok(()) => recovery.push(format!(
                            "Restored the previous permissions for <#{channel_id}>."
                        )),
                        Err(error) => recovery.push(format!(
                            "Could not restore permissions for <#{channel_id}>: {error:#}"
                        )),
                    }
                }
            }
        }

        AuditLogEventType::ChannelDelete => {
            if module_is_enabled(pool, &guild_string, "channel_deletion").await? {
                if let Some(channel_id) = entry.target_id {
                    recovery.push(format!("Channel `{channel_id}` was deleted. Discord does not allow the bot to restore its messages or original channel ID."));
                    quarantined_reason = Some("deleted a channel".to_owned());
                }
            }
        }

        AuditLogEventType::WebhookCreate => {
            if module_is_enabled(pool, &guild_string, "webhook_raid").await? {
                let now = Instant::now();
                let should_contain = {
                    let events = runtime.webhook_creates.entry(guild_id.get()).or_default();
                    while events
                        .front()
                        .is_some_and(|(time, _)| now.duration_since(*time) > WEBHOOK_WINDOW)
                    {
                        events.pop_front();
                    }
                    events.push_back((now, actor_id));
                    events.len() >= WEBHOOK_CREATE_LIMIT
                } && webhook_cooldown_ready(runtime, guild_id, now);
                if should_contain {
                    quarantined_reason = Some("created a burst of webhooks".to_owned());
                    match contain_webhook_raid(http, guild_id).await {
                        Ok(()) => recovery.push("Deleted all guild webhooks and removed Manage Webhooks permissions after a webhook creation burst.".to_owned()),
                        Err(error) => recovery.push(format!("Webhook containment was incomplete: {error:#}")),
                    }
                    runtime.last_webhook_containment.insert(guild_id.get(), now);
                    if let Some(events) = runtime.webhook_creates.get_mut(&guild_id.get()) {
                        events.clear();
                    }
                }
            }
        }

        _ => {}
    }

    if let Some(reason) = quarantined_reason {
        let quarantine_result = quarantine_actor(http, pool, guild_id, actor_id, &reason).await;
        let actor_action = match quarantine_result {
            Ok(Some(record_id)) => {
                format!("Actor <@{actor_id}> was jailed (record `{record_id}`) for {reason}.")
            }
            Ok(None) => format!("Actor <@{actor_id}> was already jailed."),
            Err(error) => format!("Could not jail actor <@{actor_id}> for {reason}: {error:#}"),
        };
        recovery.push(actor_action);
        let message = format!(
            "**Quarantine response**\nActor: <@{actor_id}>\n{}",
            recovery.join("\n")
        );
        if let Err(error) =
            crate::services::discord_moderation::send_moderation_log(http, pool, guild_id, &message)
                .await
        {
            tracing::warn!(?error, "Could not send quarantine audit log");
        }
    }

    Ok(())
}

pub async fn handle_webhook_message(
    http: &HttpClient,
    pool: &SqlitePool,
    message: &MessageCreate,
    runtime: &mut Runtime,
) -> anyhow::Result<()> {
    let Some(guild_id) = message.guild_id else {
        return Ok(());
    };
    if message.webhook_id.is_none()
        || !(message.mention_everyone || message.mentions.len() + message.mention_roles.len() >= 5)
    {
        return Ok(());
    }
    let guild_string = guild_id.to_string();
    let Some(config) = config_repository::get_guild_config(pool, &guild_string).await? else {
        return Ok(());
    };
    if !config.quarantine_enabled || !module_is_enabled(pool, &guild_string, "webhook_raid").await?
    {
        return Ok(());
    }

    let now = Instant::now();
    if !webhook_cooldown_ready(runtime, guild_id, now) {
        return Ok(());
    }
    runtime.last_webhook_containment.insert(guild_id.get(), now);
    let containment = contain_webhook_raid(http, guild_id).await;

    let recent_actor = runtime
        .webhook_creates
        .get(&guild_id.get())
        .and_then(|events| {
            events
                .iter()
                .rev()
                .find(|(time, _)| now.duration_since(*time) <= WEBHOOK_WINDOW)
                .map(|(_, actor)| *actor)
        });
    let mut detail = match containment {
        Ok(()) => "A webhook sent a mass-mention message. All guild webhooks were deleted and Manage Webhooks permissions removed.".to_owned(),
        Err(error) => format!("A webhook sent a mass-mention message, but containment was incomplete: {error:#}"),
    };
    if let Some(actor) = recent_actor {
        match quarantine_actor(
            http,
            pool,
            guild_id,
            actor,
            "created a webhook that mass-pinged the server",
        )
        .await
        {
            Ok(Some(record_id)) => detail.push_str(&format!(
                "\nRecent creator <@{actor}> was jailed (record `{record_id}`)."
            )),
            Ok(None) => {
                detail.push_str(&format!("\nRecent creator <@{actor}> was already jailed."))
            }
            Err(error) => detail.push_str(&format!(
                "\nCould not jail recent creator <@{actor}>: {error:#}"
            )),
        }
    }
    if let Err(error) = crate::services::discord_moderation::send_moderation_log(
        http,
        pool,
        guild_id,
        &format!("**Webhook raid contained**\n{detail}"),
    )
    .await
    {
        tracing::warn!(?error, "Could not send webhook raid audit log");
    }
    Ok(())
}

async fn restore_channel_overwrite(
    http: &HttpClient,
    channel_id: Id<ChannelMarker>,
    subject_id: Id<GenericMarker>,
    subject_kind: &str,
    old_allow: Option<Permissions>,
    old_deny: Option<Permissions>,
) -> anyhow::Result<()> {
    let delete_previous = old_allow.is_none() && old_deny.is_none();
    match subject_kind {
        "role" | "0" => {
            let role_id = Id::<RoleMarker>::new(subject_id.get());
            if delete_previous {
                ratelimit::guard(
                    Lane::Overwrite,
                    http.delete_channel_permission(channel_id).role(role_id),
                )
                .await?;
                return Ok(());
            }
            let channel = ratelimit::guard(Lane::Read, http.channel(channel_id))
                .await?
                .model()
                .await?;
            let existing = channel
                .permission_overwrites
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|overwrite| {
                    overwrite.kind == ChannelOverwriteType::Role && overwrite.id == role_id.cast()
                });
            let allow = old_allow.unwrap_or_else(|| {
                existing
                    .map(|overwrite| overwrite.allow)
                    .unwrap_or(Permissions::empty())
            });
            let deny = old_deny.unwrap_or_else(|| {
                existing
                    .map(|overwrite| overwrite.deny)
                    .unwrap_or(Permissions::empty())
            });
            let overwrite = PermissionOverwrite {
                allow: Some(allow),
                deny: Some(deny),
                id: role_id.cast(),
                kind: PermissionOverwriteType::Role,
            };
            ratelimit::guard(
                Lane::Overwrite,
                http.update_channel_permission(channel_id, &overwrite),
            )
            .await?;
        }
        "member" | "1" => {
            let user_id = Id::<UserMarker>::new(subject_id.get());
            if delete_previous {
                ratelimit::guard(
                    Lane::Overwrite,
                    http.delete_channel_permission(channel_id).member(user_id),
                )
                .await?;
                return Ok(());
            }
            let channel = ratelimit::guard(Lane::Read, http.channel(channel_id))
                .await?
                .model()
                .await?;
            let existing = channel
                .permission_overwrites
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|overwrite| {
                    overwrite.kind == ChannelOverwriteType::Member && overwrite.id == user_id.cast()
                });
            let allow = old_allow.unwrap_or_else(|| {
                existing
                    .map(|overwrite| overwrite.allow)
                    .unwrap_or(Permissions::empty())
            });
            let deny = old_deny.unwrap_or_else(|| {
                existing
                    .map(|overwrite| overwrite.deny)
                    .unwrap_or(Permissions::empty())
            });
            let overwrite = PermissionOverwrite {
                allow: Some(allow),
                deny: Some(deny),
                id: user_id.cast(),
                kind: PermissionOverwriteType::Member,
            };
            ratelimit::guard(
                Lane::Overwrite,
                http.update_channel_permission(channel_id, &overwrite),
            )
            .await?;
        }
        _ => anyhow::bail!(
            "The audit log did not identify whether the overwrite belongs to a role or member"
        ),
    }
    Ok(())
}

async fn module_is_enabled(pool: &SqlitePool, guild_id: &str, key: &str) -> anyhow::Result<bool> {
    config_repository::quarantine_module_enabled(pool, guild_id, key).await
}

fn dangerous_permissions() -> Permissions {
    Permissions::ADMINISTRATOR
        | Permissions::MANAGE_GUILD
        | Permissions::MANAGE_ROLES
        | Permissions::MANAGE_CHANNELS
        | Permissions::MANAGE_WEBHOOKS
        | Permissions::BAN_MEMBERS
        | Permissions::KICK_MEMBERS
        | Permissions::MODERATE_MEMBERS
        | Permissions::VIEW_AUDIT_LOG
}

async fn quarantine_actor(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
    reason: &str,
) -> anyhow::Result<Option<i64>> {
    let config = config_repository::get_guild_config(pool, &guild_id.to_string())
        .await?
        .context("Server configuration is missing")?;
    let prisoner_role_id = config
        .prisoner_role_id
        .as_deref()
        .context("Run `/jailsetup` first")?
        .parse::<u64>()?;
    let jail_channel_id = config
        .jail_channel_id
        .as_deref()
        .context("Run `/setjail` first")?
        .parse::<u64>()?;
    let reason = format!("Quarantine: {reason}");
    match crate::services::discord_moderation::jail_member(
        http,
        pool,
        guild_id,
        actor_id,
        actor_id,
        Id::<RoleMarker>::new(prisoner_role_id),
        Id::<ChannelMarker>::new(jail_channel_id),
        Some(reason),
        None,
    )
    .await
    {
        Ok(record_id) => Ok(Some(record_id)),
        Err(error) if error.to_string().contains("already jailed") => Ok(None),
        Err(error) => Err(error),
    }
}

fn webhook_cooldown_ready(runtime: &Runtime, guild_id: Id<GuildMarker>, now: Instant) -> bool {
    runtime
        .last_webhook_containment
        .get(&guild_id.get())
        .is_none_or(|last| now.duration_since(*last) >= WEBHOOK_CONTAINMENT_COOLDOWN)
}

async fn contain_webhook_raid(http: &HttpClient, guild_id: Id<GuildMarker>) -> anyhow::Result<()> {
    let webhooks = ratelimit::guard(Lane::Read, http.guild_webhooks(guild_id))
        .await?
        .models()
        .await?;
    for webhook in webhooks {
        ratelimit::guard(Lane::Member, http.delete_webhook(webhook.id)).await?;
    }

    let roles = ratelimit::guard(Lane::Read, http.roles(guild_id))
        .await?
        .models()
        .await?;
    for role in roles {
        if role.permissions.contains(Permissions::MANAGE_WEBHOOKS) {
            ratelimit::guard(
                Lane::Member,
                http.update_role(guild_id, role.id)
                    .permissions(role.permissions & !Permissions::MANAGE_WEBHOOKS),
            )
            .await?;
        }
    }

    let channels = ratelimit::guard(Lane::Read, http.guild_channels(guild_id))
        .await?
        .models()
        .await?;
    for channel in channels {
        for current in channel.permission_overwrites.as_deref().unwrap_or_default() {
            if !current.allow.contains(Permissions::MANAGE_WEBHOOKS) {
                continue;
            }
            let allow = current.allow & !Permissions::MANAGE_WEBHOOKS;
            let overwrite = match current.kind {
                ChannelOverwriteType::Role => PermissionOverwrite {
                    allow: Some(allow),
                    deny: Some(current.deny),
                    id: current.id,
                    kind: PermissionOverwriteType::Role,
                },
                ChannelOverwriteType::Member => PermissionOverwrite {
                    allow: Some(allow),
                    deny: Some(current.deny),
                    id: current.id,
                    kind: PermissionOverwriteType::Member,
                },
                _ => continue,
            };
            ratelimit::guard(
                Lane::Overwrite,
                http.update_channel_permission(channel.id, &overwrite),
            )
            .await?;
        }
    }
    Ok(())
}
