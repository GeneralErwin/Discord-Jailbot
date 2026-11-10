use std::time::{SystemTime, UNIX_EPOCH};

use twilight_model::{
    channel::message::component::{ActionRow, Button, ButtonStyle, Component},
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
        "you cannot ",
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

pub fn standard_components() -> Vec<Component> {
    action_row(vec![
        button("bot:help", "Commands", ButtonStyle::Primary, None),
        button("bot:changelog", "Changelog", ButtonStyle::Secondary, None),
        button("", "GitHub", ButtonStyle::Link, Some(repository_url())),
    ])
}

pub fn help() -> Embed {
    branded_embed(
        "📘 Bot commands".to_owned(),
        Some("Use the buttons on bot messages to open this guide or the GitHub changelog. Configuration commands require the server owner or an administrator.".to_owned()),
        vec![
            EmbedField { inline: false, name: "Jail moderation".to_owned(), value: "`/jail member [duration] [reason]` - Jail a lower-ranked member.\n`/unjail member` - Release a jailed member.\n`/prisoners` - List active jail records.\n`/jailinfo member` - View jail history.".to_owned() },
            EmbedField { inline: false, name: "Configuration".to_owned(), value: "`/jailsetup` - Create the Prisoner role.\n`/setjail channel` - Set the jail channel and permissions.\n`/set moderator role` - Set who can jail lower-ranked members.\n`/setlogs channel` - Set the audit log channel.\n`/jailconfig` - View jail configuration.\n`/sanitize` - Repair Prisoner permissions in every channel.".to_owned() },
            EmbedField { inline: false, name: "Quarantine".to_owned(), value: "`/quarantine` - Turn quarantine on or off and open its status page.\n`/enable module` and `/disable module` - Change a protection module.\nThe Configure button opens one-click module controls.".to_owned() },
            EmbedField { inline: true, name: "Info".to_owned(), value: "`/help` - Show this guide.\n`/changelog` - View GitHub commits and versions.\n`/ping` - Check bot status.".to_owned() },
        ],
        COLOR_INFO,
        "Rust Jail System",
    )
}

pub fn jail_release_components(guild_id: u64, member_id: u64) -> Vec<Component> {
    action_row(vec![
        button(
            &format!("jail:release:{guild_id}:{member_id}"),
            "Release member",
            ButtonStyle::Danger,
            None,
        ),
        button("bot:help", "Commands", ButtonStyle::Primary, None),
        button("bot:changelog", "Changelog", ButtonStyle::Secondary, None),
        button("", "GitHub", ButtonStyle::Link, Some(repository_url())),
    ])
}

pub fn quarantine_status_components(guild_id: u64) -> Vec<Component> {
    action_row(vec![
        button(
            &format!("quarantine:configure:{guild_id}"),
            "Configure modules",
            ButtonStyle::Primary,
            None,
        ),
        button("bot:help", "Commands", ButtonStyle::Secondary, None),
        button("bot:changelog", "Changelog", ButtonStyle::Secondary, None),
        button("", "GitHub", ButtonStyle::Link, Some(repository_url())),
    ])
}

pub fn quarantine_module_components(
    guild_id: u64,
    modules: &[(&str, &str, bool)],
) -> Vec<Component> {
    let mut rows = Vec::new();
    for group in modules.chunks(5) {
        let buttons = group
            .iter()
            .map(|(key, label, enabled)| {
                let action = if *enabled { "Disable" } else { "Enable" };
                let style = if *enabled {
                    ButtonStyle::Danger
                } else {
                    ButtonStyle::Success
                };
                button(
                    &format!("quarantine:toggle:{guild_id}:{key}"),
                    &format!("{action}: {label}"),
                    style,
                    None,
                )
            })
            .collect();
        rows.extend(action_row(buttons));
    }
    rows.extend(action_row(vec![
        button(
            &format!("quarantine:status:{guild_id}"),
            "Back to status",
            ButtonStyle::Secondary,
            None,
        ),
        button("bot:help", "Commands", ButtonStyle::Secondary, None),
        button("bot:changelog", "Changelog", ButtonStyle::Secondary, None),
        button("", "GitHub", ButtonStyle::Link, Some(repository_url())),
    ]));
    rows
}

pub fn changelog_components(offset: i64, has_older: bool) -> Vec<Component> {
    let mut buttons = Vec::new();
    if offset > 0 {
        buttons.push(button(
            &format!("changelog:page:{}", (offset - 5).max(0)),
            "Newer",
            ButtonStyle::Primary,
            None,
        ));
    }
    if has_older {
        buttons.push(button(
            &format!("changelog:page:{}", offset + 5),
            "Older",
            ButtonStyle::Primary,
            None,
        ));
    }
    buttons.push(button("bot:help", "Commands", ButtonStyle::Secondary, None));
    buttons.push(button(
        "",
        "GitHub",
        ButtonStyle::Link,
        Some(repository_url()),
    ));
    action_row(buttons)
}

pub fn changelog(entries: &[crate::database::changelog_repository::ChangelogEntry]) -> Embed {
    let mut fields = Vec::new();
    for entry in entries.iter().take(5) {
        let date = entry.committed_at.get(..10).unwrap_or(&entry.committed_at);
        let first_line = entry
            .message
            .lines()
            .next()
            .unwrap_or("Commit message unavailable");
        let body = entry.message.lines().skip(1).collect::<Vec<_>>().join("\n");
        let value = format!(
            "{}{}\n[`{}`]({})",
            first_line,
            if body.is_empty() {
                String::new()
            } else {
                format!("\n{body}")
            },
            &entry.sha[..entry.sha.len().min(7)],
            entry.html_url,
        );
        fields.push(EmbedField {
            inline: false,
            name: truncate(
                &format!("v{}.{} | {date}", entry.version_major, entry.version_minor),
                256,
            ),
            value: truncate(&value, 1024),
        });
    }

    branded_embed(
        "📜 GitHub changelog".to_owned(),
        Some("Commit messages are the release descriptions. Regular changes advance the second version number; commits marked `major:`, with a breaking `!:` subject, or containing `BREAKING CHANGE` advance the major version.".to_owned()),
        fields,
        COLOR_INFO,
        "Version history from GitHub",
    )
}

pub fn quarantine_modules_embed(service_enabled: bool, modules: &[(&str, &str, bool)]) -> Embed {
    let fields = modules
        .iter()
        .map(|(key, label, enabled)| {
            let description = crate::services::quarantine::MODULES
                .iter()
                .find(|module| module.key == *key)
                .map(|module| module.description)
                .unwrap_or("Quarantine protection module.");
            let state = if !service_enabled && *enabled {
                "Enabled, but paused while the service is off"
            } else if *enabled {
                "Module enabled"
            } else {
                "Module disabled"
            };
            EmbedField {
                inline: false,
                name: format!("{} {label}", if *enabled { "✅" } else { "❌" }),
                value: format!("{state}\n{description}"),
            }
        })
        .collect();
    branded_embed(
        format!(
            "{} Quarantine modules",
            if service_enabled { "🛡️" } else { "⏸️" }
        ),
        Some(format!(
            "Service is **{}**. Click a module button below to switch it.",
            if service_enabled {
                "enabled"
            } else {
                "disabled"
            }
        )),
        fields,
        if service_enabled {
            COLOR_INFO
        } else {
            COLOR_WARNING
        },
        "Rust Jail System | interactive configuration",
    )
}

pub fn quarantine_status_embed(service_enabled: bool, modules: &[(&str, &str, bool)]) -> Embed {
    let mut embed = quarantine_modules_embed(service_enabled, modules);
    embed.description = Some(format!(
        "Quarantine is **{}**. Open Configure to turn individual protections on or off.",
        if service_enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    embed.title = Some("🛡️ Quarantine status".to_owned());
    embed
}

pub fn changelog_empty() -> Embed {
    branded_embed(
        "📜 GitHub changelog".to_owned(),
        Some("No commits have been synchronized yet. The bot checks GitHub on startup and every five minutes.".to_owned()),
        Vec::new(),
        COLOR_INFO,
        "Version history from GitHub",
    )
}

fn repository_url() -> String {
    let repo = std::env::var("GITHUB_REPOSITORY")
        .unwrap_or_else(|_| "GeneralErwin/Discord-Jailbot".to_owned());
    format!("https://github.com/{repo}")
}

fn action_row(buttons: Vec<Button>) -> Vec<Component> {
    vec![Component::ActionRow(ActionRow {
        components: buttons.into_iter().map(Component::Button).collect(),
        id: None,
    })]
}

fn button(custom_id: &str, label: &str, style: ButtonStyle, url: Option<String>) -> Button {
    Button {
        custom_id: (!custom_id.is_empty()).then(|| custom_id.to_owned()),
        disabled: false,
        emoji: None,
        id: None,
        label: Some(label.to_owned()),
        sku_id: None,
        style,
        url,
    }
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
