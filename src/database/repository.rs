use anyhow::Context;
use sqlx::{FromRow, SqlitePool};

use super::models::{JailRecord, SavedRole};

#[derive(Debug, Clone)]
pub struct SavedRoleInput {
    pub role_id: String,
    pub role_position: i64,
    pub restorable: bool,
}

#[derive(Debug, Clone)]
pub struct OverwriteSnapshotInput {
    pub channel_id: String,
    pub allow_bits: String,
    pub deny_bits: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct OverwriteSnapshot {
    pub jail_record_id: i64,
    pub channel_id: String,
    pub allow_bits: String,
    pub deny_bits: String,
}

#[derive(Debug, Clone)]
pub struct NewJail {
    pub guild_id: String,
    pub user_id: String,
    pub moderator_id: String,
    pub reason: Option<String>,
    pub release_at: Option<String>,
    pub original_nickname: Option<String>,
    pub roles: Vec<SavedRoleInput>,
    pub overwrites: Vec<OverwriteSnapshotInput>,
}

/// Creates a jail record and saves the member's original roles
/// and channel permission overwrites in one transaction.
pub async fn create_jail(pool: &SqlitePool, input: NewJail) -> anyhow::Result<i64> {
    let mut tx = pool.begin().await?;

    let result = sqlx::query(
        "INSERT INTO jail_records
        (guild_id, user_id, moderator_id, reason,
         release_at, original_nickname)
        VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&input.guild_id)
    .bind(&input.user_id)
    .bind(&input.moderator_id)
    .bind(input.reason.as_deref())
    .bind(input.release_at.as_deref())
    .bind(input.original_nickname.as_deref())
    .execute(&mut *tx)
    .await
    .context("Failed to create jail record")?;

    let record_id = result.last_insert_rowid();

    for role in &input.roles {
        sqlx::query(
            "INSERT INTO saved_roles
            (jail_record_id, role_id, role_position, restorable)
            VALUES (?, ?, ?, ?)",
        )
        .bind(record_id)
        .bind(&role.role_id)
        .bind(role.role_position)
        .bind(role.restorable)
        .execute(&mut *tx)
        .await
        .context("Failed to save member role")?;
    }

    for overwrite in &input.overwrites {
        sqlx::query(
            "INSERT INTO member_overwrite_snapshots
            (jail_record_id, channel_id, allow_bits, deny_bits)
            VALUES (?, ?, ?, ?)",
        )
        .bind(record_id)
        .bind(&overwrite.channel_id)
        .bind(&overwrite.allow_bits)
        .bind(&overwrite.deny_bits)
        .execute(&mut *tx)
        .await
        .context("Failed to save channel permission overwrite")?;
    }

    tx.commit().await?;

    Ok(record_id)
}

/// Retrieves an active jail for a particular member.
pub async fn get_active_jail(
    pool: &SqlitePool,
    guild_id: &str,
    user_id: &str,
) -> anyhow::Result<Option<JailRecord>> {
    let record = sqlx::query_as::<_, JailRecord>(
        "SELECT * FROM jail_records
         WHERE guild_id = ? AND user_id = ? AND status = 'active'
         LIMIT 1",
    )
    .bind(guild_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(record)
}

/// Retrieves all active prisoners in a server.
pub async fn list_active_jails(
    pool: &SqlitePool,
    guild_id: &str,
) -> anyhow::Result<Vec<JailRecord>> {
    let records = sqlx::query_as::<_, JailRecord>(
        "SELECT * FROM jail_records
         WHERE guild_id = ? AND status = 'active'
         ORDER BY jailed_at ASC",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;

    Ok(records)
}

/// Retrieves the original roles saved when a member was jailed.
pub async fn get_saved_roles(pool: &SqlitePool, record_id: i64) -> anyhow::Result<Vec<SavedRole>> {
    let roles = sqlx::query_as::<_, SavedRole>(
        "SELECT * FROM saved_roles
         WHERE jail_record_id = ?
         ORDER BY role_position DESC",
    )
    .bind(record_id)
    .fetch_all(pool)
    .await?;

    Ok(roles)
}

/// Retrieves the original channel permission overwrites.
pub async fn get_overwrite_snapshots(
    pool: &SqlitePool,
    record_id: i64,
) -> anyhow::Result<Vec<OverwriteSnapshot>> {
    let snapshots = sqlx::query_as::<_, OverwriteSnapshot>(
        "SELECT jail_record_id, channel_id, allow_bits, deny_bits
         FROM member_overwrite_snapshots
         WHERE jail_record_id = ?",
    )
    .bind(record_id)
    .fetch_all(pool)
    .await?;

    Ok(snapshots)
}

/// Releases a member or marks their sentence as expired.
/// Returns false if the record was already inactive.
pub async fn release_jail(
    pool: &SqlitePool,
    record_id: i64,
    released_by: &str,
    expired: bool,
) -> anyhow::Result<bool> {
    let status = if expired { "expired" } else { "released" };

    let result = sqlx::query(
        "UPDATE jail_records
         SET status = ?,
             released_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
             released_by = ?
         WHERE id = ? AND status = 'active'",
    )
    .bind(status)
    .bind(released_by)
    .bind(record_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Marks an active prisoner as having left the server.
pub async fn mark_left_server(
    pool: &SqlitePool,
    guild_id: &str,
    user_id: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE jail_records
         SET left_server = 1,
             left_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE guild_id = ? AND user_id = ? AND status = 'active'",
    )
    .bind(guild_id)
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Records a prisoner's return to the server.
pub async fn mark_rejoined(
    pool: &SqlitePool,
    guild_id: &str,
    user_id: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE jail_records
         SET left_server = 0,
             left_at = NULL,
             last_rejoined_at =
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
             rejoin_count = rejoin_count + 1
         WHERE guild_id = ? AND user_id = ? AND status = 'active'",
    )
    .bind(guild_id)
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Writes an entry to the audit log.
pub async fn write_audit_log(
    pool: &SqlitePool,
    guild_id: &str,
    actor_id: Option<&str>,
    target_user_id: Option<&str>,
    event_type: &str,
    reason: Option<&str>,
    details_json: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO audit_logs
        (guild_id, actor_id, target_user_id,
         event_type, reason, details_json)
        VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(guild_id)
    .bind(actor_id)
    .bind(target_user_id)
    .bind(event_type)
    .bind(reason)
    .bind(details_json)
    .execute(pool)
    .await
    .context("Failed to write audit log")?;

    Ok(())
}
pub async fn list_due_jails(pool: &SqlitePool) -> anyhow::Result<Vec<JailRecord>> {
    let records = sqlx::query_as::<_, JailRecord>(
        "SELECT * FROM jail_records
         WHERE status = 'active'
           AND release_at IS NOT NULL
           AND datetime(release_at) <= datetime('now')
         ORDER BY release_at ASC",
    )
    .fetch_all(pool)
    .await
    .context("Failed to retrieve expired jail records")?;

    Ok(records)
}

pub async fn get_jail_history(
    pool: &SqlitePool,
    guild_id: &str,
    user_id: &str,
) -> anyhow::Result<Vec<JailRecord>> {
    let records = sqlx::query_as::<_, JailRecord>(
        "SELECT * FROM jail_records
         WHERE guild_id = ? AND user_id = ?
         ORDER BY jailed_at DESC",
    )
    .bind(guild_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .context("Failed to retrieve jail history")?;

    Ok(records)
}
