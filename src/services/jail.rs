use anyhow::{Context, bail};
use sqlx::SqlitePool;

use crate::database::{
    models::JailRecord,
    repository::{self, NewJail},
};

#[derive(Clone)]
pub struct JailService {
    pool: SqlitePool,
}

impl JailService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Creates a persistent jail record and saves the original state.
    pub async fn create_record(&self, input: NewJail) -> anyhow::Result<i64> {
        if input.guild_id.is_empty() || input.user_id.is_empty() || input.moderator_id.is_empty() {
            bail!("Guild, user, and moderator IDs are required");
        }

        if repository::get_active_jail(&self.pool, &input.guild_id, &input.user_id)
            .await?
            .is_some()
        {
            bail!("This member already has an active jail record");
        }

        let guild_id = input.guild_id.clone();
        let user_id = input.user_id.clone();
        let moderator_id = input.moderator_id.clone();
        let reason = input.reason.clone();

        let record_id = repository::create_jail(&self.pool, input)
            .await
            .context("Failed to create jail record")?;

        if let Err(error) = repository::write_audit_log(
            &self.pool,
            &guild_id,
            Some(&moderator_id),
            Some(&user_id),
            "jail",
            reason.as_deref(),
            "{}",
        )
        .await
        {
            tracing::error!(?error, "Failed to write jail audit log");
        }

        Ok(record_id)
    }

    /// Completes a release after Discord role and permission restoration succeeds.
    pub async fn complete_release(
        &self,
        guild_id: &str,
        user_id: &str,
        released_by: &str,
        expired: bool,
    ) -> anyhow::Result<Option<i64>> {
        let Some(record) = repository::get_active_jail(&self.pool, guild_id, user_id).await? else {
            return Ok(None);
        };

        let changed = repository::release_jail(&self.pool, record.id, released_by, expired).await?;

        if !changed {
            return Ok(None);
        }

        let event_type = if expired { "jail_expired" } else { "unjail" };

        if let Err(error) = repository::write_audit_log(
            &self.pool,
            guild_id,
            Some(released_by),
            Some(user_id),
            event_type,
            None,
            "{}",
        )
        .await
        {
            tracing::error!(?error, "Failed to write release audit log");
        }

        Ok(Some(record.id))
    }

    pub async fn get_prisoner(
        &self,
        guild_id: &str,
        user_id: &str,
    ) -> anyhow::Result<Option<JailRecord>> {
        repository::get_active_jail(&self.pool, guild_id, user_id).await
    }

    pub async fn list_prisoners(&self, guild_id: &str) -> anyhow::Result<Vec<JailRecord>> {
        repository::list_active_jails(&self.pool, guild_id).await
    }

    /// Records a member leaving while their jail record remains active.
    pub async fn member_left(&self, guild_id: &str, user_id: &str) -> anyhow::Result<bool> {
        let changed = repository::mark_left_server(&self.pool, guild_id, user_id).await?;

        if changed {
            if let Err(error) = repository::write_audit_log(
                &self.pool,
                guild_id,
                None,
                Some(user_id),
                "prisoner_left",
                None,
                "{}",
            )
            .await
            {
                tracing::error!(?error, "Failed to write departure audit log");
            }
        }

        Ok(changed)
    }

    /// Records a prisoner returning to the server.
    pub async fn member_rejoined(&self, guild_id: &str, user_id: &str) -> anyhow::Result<bool> {
        let changed = repository::mark_rejoined(&self.pool, guild_id, user_id).await?;

        if changed {
            if let Err(error) = repository::write_audit_log(
                &self.pool,
                guild_id,
                None,
                Some(user_id),
                "prisoner_rejoined",
                None,
                "{}",
            )
            .await
            {
                tracing::error!(?error, "Failed to write rejoin audit log");
            }
        }

        Ok(changed)
    }
}
impl JailService {
    pub async fn list_due(&self) -> anyhow::Result<Vec<crate::database::models::JailRecord>> {
        crate::database::repository::list_due_jails(&self.pool).await
    }

    pub async fn get_history(
        &self,
        guild_id: &str,
        user_id: &str,
    ) -> anyhow::Result<Vec<crate::database::models::JailRecord>> {
        crate::database::repository::get_jail_history(&self.pool, guild_id, user_id).await
    }
}
