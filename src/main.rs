mod commands;
mod database;
mod discord;
mod services;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let started = std::time::Instant::now();

    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting Rust Jail System...");

    let pool = database::connect().await?;

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Database initialized successfully"
    );

    let token =
        std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN is missing from the environment")?;

    let _jail_service = services::JailService::new(pool.clone());

    discord::run(token, pool, started).await?;

    Ok(())
}
