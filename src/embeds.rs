use std::time::{SystemTime, UNIX_EPOCH};

use twilight_model::{
    channel::message::{
        Embed,
        embed::{EmbedField, EmbedFooter},
    },
    util::Timestamp,
};

const COLOR_SUCCESS: u32 = 0x57_F2_87;
const COLOR_INFO: u32 = 0x58_65_F2;
const COLOR_WARNING: u32 = 0xFE_E7_5C;
const COLOR_ERROR: u32 = 0xED_42_45;

/// Turn a command's existing message into the standard branded response embed.
pub fn response(message: &str) -> Embed {
    let message = message.trim();
    let lower = message.to_ascii_lowercase();
    let is_error = [
        "could not ",
        "failed ",
        "setup failed",
        "you need ",
        "this command can only",
        "cannot ",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix));

    let (title, description, color) = if is_error {
        (
            "\u{26A0}\u{FE0F} Command failed".to_owned(),
            message.to_owned(),
            COLOR_ERROR,
        )
    } else if let Some((heading, body)) = bold_heading(message) {
        (response_heading(&heading), body, COLOR_INFO)
    } else {
        let (title, description) = if lower.starts_with("pong!") {
            (
                "\u{1F3D3} Pong!",
                "The jail bot is online and responding.".to_owned(),
            )
        } else if lower.contains("jailed successfully") {
            ("\u{1F512} Member jailed", message.to_owned())
        } else if lower.contains("released successfully") {
            ("\u{1F513} Member released", message.to_owned())
        } else if lower.starts_with("prisoner role is ready") {
            ("\u{1F6E1}\u{FE0F} Jail setup complete", message.to_owned())
        } else if lower.starts_with("jail channel configured") {
            ("\u{1F510} Jail channel configured", message.to_owned())
        } else if lower.starts_with("moderation log channel set") {
            ("\u{1F4CB} Log channel configured", message.to_owned())
        } else if lower.starts_with("there are currently no jailed members") {
            (
                "\u{1F4ED} No prisoners",
                "The jail roster is clear.".to_owned(),
            )
        } else if lower.starts_with("no jail history") {
            ("\u{1F4ED} No jail history", message.to_owned())
        } else if lower.starts_with("this server has no jail configuration") {
            ("\u{2699}\u{FE0F} Jail configuration", message.to_owned())
        } else {
            ("\u{2705} Jail system", message.to_owned())
        };

        (title.to_owned(), description, COLOR_SUCCESS)
    };

    branded_embed(
        title,
        (!description.is_empty()).then(|| description),
        Vec::new(),
        color,
        "Rust Jail System",
    )
}

/// Turn the bot's existing audit text into a compact, structured log embed.
pub fn moderation_log(message: &str) -> Embed {
    let message = message.trim();
    let (raw_title, body) = bold_heading(message)
        .unwrap_or_else(|| ("Moderation event".to_owned(), message.to_owned()));
    let lower_title = raw_title.to_ascii_lowercase();

    let (icon, color) = if lower_title.contains("jail state repaired") {
        ("\u{1F6E0}\u{FE0F}", COLOR_INFO)
    } else if lower_title.contains("left the server") {
        ("\u{1F6AA}", COLOR_WARNING)
    } else if lower_title.contains("rejoin") {
        ("\u{21A9}\u{FE0F}", COLOR_SUCCESS)
    } else if lower_title.contains("jailed") {
        ("\u{1F512}", COLOR_ERROR)
    } else if lower_title.contains("release") || lower_title.contains("expired") {
        ("\u{1F513}", COLOR_SUCCESS)
    } else {
        ("\u{1F4CB}", COLOR_INFO)
    };

    let mut fields = Vec::new();
    let mut description_lines = Vec::new();

    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            let known_field = matches!(
                name.to_ascii_lowercase().as_str(),
                "member"
                    | "moderator"
                    | "reason"
                    | "record"
                    | "actor"
                    | "members repaired"
                    | "permission entries updated"
            );

            if known_field && !value.is_empty() && fields.len() < 25 {
                fields.push(EmbedField {
                    inline: matches!(
                        name.to_ascii_lowercase().as_str(),
                        "member" | "moderator" | "actor" | "record"
                    ),
                    name: truncate(name, 256),
                    value: truncate(value, 1024),
                });
                continue;
            }
        }

        description_lines.push(line);
    }

    let description = description_lines.join("\n");
    branded_embed(
        format!("{icon} {raw_title}"),
        (!description.is_empty()).then(|| truncate(&description, 4096)),
        fields,
        color,
        "Moderation audit \u{2022} Rust Jail",
    )
}

fn response_heading(heading: &str) -> String {
    let lower = heading.to_ascii_lowercase();
    let icon = if lower.contains("prisoner") {
        "\u{1F512}"
    } else if lower.contains("history") {
        "\u{1F4DA}"
    } else if lower.contains("configuration") {
        "\u{2699}\u{FE0F}"
    } else {
        "\u{1F4CB}"
    };

    format!("{icon} {heading}")
}

fn bold_heading(message: &str) -> Option<(String, String)> {
    let (first_line, rest) = message.split_once('\n').unwrap_or((message, ""));
    let heading = first_line
        .trim()
        .strip_prefix("**")?
        .strip_suffix("**")?
        .trim();

    if heading.is_empty() {
        return None;
    }

    Some((heading.to_owned(), rest.trim().to_owned()))
}

fn branded_embed(
    title: String,
    description: Option<String>,
    fields: Vec<EmbedField>,
    color: u32,
    footer: &str,
) -> Embed {
    Embed {
        author: None,
        color: Some(color),
        description: description.map(|value| truncate(&value, 4096)),
        fields,
        flags: None,
        footer: Some(EmbedFooter {
            icon_url: None,
            proxy_icon_url: None,
            text: footer.to_owned(),
        }),
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: None,
        timestamp: now_timestamp(),
        title: Some(truncate(&title, 256)),
        url: None,
        video: None,
    }
}

fn now_timestamp() -> Option<Timestamp> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Timestamp::from_secs(i64::try_from(seconds).ok()?).ok()
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    let mut truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('\u{2026}');
    truncated
}
