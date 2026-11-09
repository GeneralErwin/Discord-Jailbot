use anyhow::Context;
use sqlx::SqlitePool;

use super::models::GuildConfig;

/// Retrieves a server's configuration.
pub async fn get_guild_config(
    pool: &SqlitePool,
    guild_id: &str,
) -> anyhow::Result<Option<GuildConfig>> {
    let config = sqlx::query_as::<_, GuildConfig>("SELECT * FROM guild_configs WHERE guild_id = ?")
        .bind(guild_id)
        .fetch_optional(pool)
        .await
        .context("Failed to retrieve guild configuration")?;

    Ok(config)
}

/// Creates a default configuration if one doesn't exist.
pub async fn ensure_guild_config(pool: &SqlitePool, guild_id: &str) -> anyhow::Result<GuildConfig> {
    sqlx::query(
        "INSERT OR IGNORE INTO guild_configs (guild_id)
         VALUES (?)",
    )
    .bind(guild_id)
    .execute(pool)
    .await
    .context("Failed to create guild configuration")?;

    get_guild_config(pool, guild_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Guild configuration was not created"))
}

/// Sets the Prisoner role for a server.
pub async fn set_prisoner_role(
    pool: &SqlitePool,
    guild_id: &str,
    role_id: Option<&str>,
) -> anyhow::Result<()> {
    ensure_guild_config(pool, guild_id).await?;

    sqlx::query(
        "UPDATE guild_configs
         SET prisoner_role_id = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE guild_id = ?",
    )
    .bind(role_id)
    .bind(guild_id)
    .execute(pool)
    .await
    .context("Failed to update Prisoner role")?;

    Ok(())
}

/// Sets the jail channel for a server.
pub async fn set_jail_channel(
    pool: &SqlitePool,
    guild_id: &str,
    channel_id: Option<&str>,
) -> anyhow::Result<()> {
    ensure_guild_config(pool, guild_id).await?;

    sqlx::query(
        "UPDATE guild_configs
         SET jail_channel_id = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE guild_id = ?",
    )
    .bind(channel_id)
    .bind(guild_id)
    .execute(pool)
    .await
    .context("Failed to update jail channel")?;

    Ok(())
}

/// Sets the logs channel for a server.
pub async fn set_logs_channel(
    pool: &SqlitePool,
    guild_id: &str,
    channel_id: Option<&str>,
) -> anyhow::Result<()> {
    ensure_guild_config(pool, guild_id).await?;

    sqlx::query(
        "UPDATE guild_configs
         SET logs_channel_id = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE guild_id = ?",
    )
    .bind(channel_id)
    .bind(guild_id)
    .execute(pool)
    .await
    .context("Failed to update logs channel")?;

    Ok(())
}

/// Marks initial server setup as complete or incomplete.
pub async fn set_setup_complete(
    pool: &SqlitePool,
    guild_id: &str,
    complete: bool,
) -> anyhow::Result<()> {
    ensure_guild_config(pool, guild_id).await?;

    sqlx::query(
        "UPDATE guild_configs
         SET setup_complete = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE guild_id = ?",
    )
    .bind(complete)
    .bind(guild_id)
    .execute(pool)
    .await
    .context("Failed to update setup status")?;

    Ok(())
}

/// Enables or disables automatic prisoner releases.
pub async fn set_auto_release(
    pool: &SqlitePool,
    guild_id: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    ensure_guild_config(pool, guild_id).await?;

    sqlx::query(
        "UPDATE guild_configs
         SET auto_release_enabled = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE guild_id = ?",
    )
    .bind(enabled)
    .bind(guild_id)
    .execute(pool)
    .await
    .context("Failed to update automatic release setting")?;

    Ok(())
}
pub async fn list_guild_configs(pool: &SqlitePool) -> anyhow::Result<Vec<GuildConfig>> {
    let configs = sqlx::query_as::<_, GuildConfig>("SELECT * FROM guild_configs ORDER BY guild_id")
        .fetch_all(pool)
        .await
        .context("Failed to retrieve guild configurations")?;

    Ok(configs)
}
