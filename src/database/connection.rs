use std::time::Duration;

use anyhow::Context;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

pub async fn connect() -> anyhow::Result<SqlitePool> {
    std::fs::create_dir_all("data").context("Failed to create database directory")?;

    let options = SqliteConnectOptions::new()
        .filename("data/jail.sqlite3")
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5))
        .statement_cache_capacity(128);

    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .context("Failed to connect to SQLite")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("Failed to apply database migrations")?;

    Ok(pool)
}
