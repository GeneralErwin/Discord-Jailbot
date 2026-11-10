use anyhow::Context;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChangelogEntry {
    pub sha: String,
    pub version_major: i64,
    pub version_minor: i64,
    pub message: String,
    pub html_url: String,
    pub committed_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChangelogState {
    pub last_sha: String,
    pub version_major: i64,
    pub version_minor: i64,
}

pub struct NewChangelogEntry<'a> {
    pub sha: &'a str,
    pub version_major: i64,
    pub version_minor: i64,
    pub message: &'a str,
    pub html_url: &'a str,
    pub committed_at: &'a str,
}

pub async fn get_state(pool: &SqlitePool) -> anyhow::Result<Option<ChangelogState>> {
    sqlx::query_as::<_, ChangelogState>(
        "SELECT last_sha, version_major, version_minor
         FROM github_changelog_state WHERE singleton = 1",
    )
    .fetch_optional(pool)
    .await
    .context("Failed to retrieve GitHub changelog state")
}

pub async fn recent_entries(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<ChangelogEntry>> {
    sqlx::query_as::<_, ChangelogEntry>(
        "SELECT sha, version_major, version_minor, message, html_url, committed_at
         FROM github_changelog_entries ORDER BY id DESC LIMIT ? OFFSET ?",
    )
    .bind(limit.clamp(1, 20))
    .bind(offset.max(0))
    .fetch_all(pool)
    .await
    .context("Failed to read GitHub changelog entries")
}

pub async fn save_sync(
    pool: &SqlitePool,
    entries: &[NewChangelogEntry<'_>],
    state: &ChangelogState,
) -> anyhow::Result<()> {
    let mut transaction = pool
        .begin()
        .await
        .context("Failed to begin changelog transaction")?;

    for entry in entries {
        sqlx::query(
            "INSERT OR IGNORE INTO github_changelog_entries
             (sha, version_major, version_minor, message, html_url, committed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(entry.sha)
        .bind(entry.version_major)
        .bind(entry.version_minor)
        .bind(entry.message)
        .bind(entry.html_url)
        .bind(entry.committed_at)
        .execute(&mut *transaction)
        .await
        .context("Failed to save a GitHub commit changelog entry")?;
    }

    sqlx::query(
        "INSERT INTO github_changelog_state (singleton, last_sha, version_major, version_minor)
         VALUES (1, ?, ?, ?)
         ON CONFLICT(singleton) DO UPDATE SET
             last_sha = excluded.last_sha,
             version_major = excluded.version_major,
             version_minor = excluded.version_minor",
    )
    .bind(&state.last_sha)
    .bind(state.version_major)
    .bind(state.version_minor)
    .execute(&mut *transaction)
    .await
    .context("Failed to save GitHub changelog sync state")?;

    transaction
        .commit()
        .await
        .context("Failed to commit GitHub changelog sync")
}
