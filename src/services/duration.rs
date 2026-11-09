use anyhow::{Context, bail};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

pub fn parse_duration(value: &str) -> anyhow::Result<String> {
    let value = value.trim();

    if value.len() < 2 {
        bail!("Invalid duration. Use formats such as 30m, 2h, 3d or 1w.");
    }

    let (number, unit) = value.split_at(value.len() - 1);

    let amount: i64 = number
        .parse()
        .context("Duration must begin with a number")?;

    if amount <= 0 {
        bail!("Duration must be greater than zero");
    }

    let multiplier: i64 = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        _ => bail!("Use m, h, d, or w for the duration unit"),
    };

    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("Duration is too large"))?;

    if seconds > 315_360_000 {
        bail!("Maximum sentence duration is 10 years");
    }

    let release_time = OffsetDateTime::now_utc() + Duration::seconds(seconds);

    Ok(release_time.format(&Rfc3339)?)
}
