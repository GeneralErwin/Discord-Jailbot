use anyhow::Context;
use sqlx::SqlitePool;
use twilight_http::Client as HttpClient;
use twilight_model::{
    channel::message::{
        Component, Embed,
        component::{ActionRow, Button, ButtonStyle, SelectMenu, SelectMenuOption, SelectMenuType},
        embed::{EmbedField, EmbedFooter, EmbedImage, EmbedThumbnail},
    },
    guild::Member,
    http::attachment::Attachment,
    id::{
        Id,
        marker::{GuildMarker, UserMarker},
    },
};

use crate::{
    database::{
        models::{JailRecord, SavedRole},
        repository,
    },
    services::ratelimit::{self, Lane},
};

pub const PAGE_SIZE: i64 = 4;
const TERMINAL_IMAGE: &[u8] = include_bytes!("../assets/prisoner_terminal.png");
const IMAGE_FILENAME: &str = "prisoner_terminal.png";
const COLOR_TERMINAL: u32 = 0x1D_5B_A8;
const COLOR_RECORD: u32 = 0x29_7A_4C;

pub struct PrisonerPage {
    pub records: Vec<JailRecord>,
    pub names: Vec<String>,
    pub embeds: Vec<Embed>,
    pub components: Vec<Component>,
    pub attachments: Vec<Attachment>,
    pub page: usize,
    pub page_count: usize,
    pub total: i64,
}

pub async fn page(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    requested_page: usize,
) -> anyhow::Result<PrisonerPage> {
    let guild_key = guild_id.to_string();
    let total = repository::count_active_jails(pool, &guild_key).await?;
    let page_count = usize::try_from((total + PAGE_SIZE - 1) / PAGE_SIZE)
        .unwrap_or(0)
        .max(1);
    let page = requested_page.clamp(1, page_count);
    let offset = i64::try_from(page - 1)
        .unwrap_or(i64::MAX)
        .saturating_mul(PAGE_SIZE);
    let records = repository::list_active_jails_page(pool, &guild_key, PAGE_SIZE, offset).await?;

    let mut names = Vec::with_capacity(records.len());
    let mut cards = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let user_id = parse_user_id(&record.user_id)?;
        let member = load_member(http, guild_id, user_id).await;
        let display_name = member
            .as_ref()
            .map(|member| {
                member
                    .nick
                    .as_deref()
                    .unwrap_or(&member.user.name)
                    .to_owned()
            })
            .unwrap_or_else(|| format!("Unknown member {}", record.user_id));
        let avatar = member
            .as_ref()
            .map(|member| avatar_url(user_id, member.user.avatar))
            .unwrap_or_else(|| default_avatar_url(user_id));
        names.push(display_name.clone());
        cards.push(roster_card(record, &display_name, &avatar, page, index + 1));
    }

    let mut embeds = vec![terminal_embed(
        "XP Jail Terminal | Active custody",
        &format!(
            "**{total}** active prisoner{} | Page **{page} of {page_count}**\nEach record includes the member profile, reason, arrest time, expiry, and moderator.",
            if total == 1 { "" } else { "s" }
        ),
    )];
    if total == 0 {
        embeds[0].description =
            Some("The custody roster is clear. No members are currently jailed.".to_owned());
    }
    embeds.extend(cards);

    let components = roster_components(guild_id, owner_id, page, page_count, &records, &names);
    let attachments = terminal_roster_attachment(total, page, page_count, &records, &names);
    Ok(PrisonerPage {
        records,
        names,
        embeds,
        components,
        attachments,
        page,
        page_count,
        total,
    })
}

pub fn selection_embed(action: &str, page: usize, page_count: usize, total: i64) -> Embed {
    terminal_embed(
        if action == "inspect" {
            "XP Jail Terminal | Inspect record"
        } else {
            "XP Jail Terminal | Release member"
        },
        &format!(
            "Choose a prisoner from page **{page} of {page_count}**.\n{total} active record{} in this server.",
            if total == 1 { "" } else { "s" }
        ),
    )
}

pub async fn inspection_embeds(
    http: &HttpClient,
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
) -> anyhow::Result<Vec<Embed>> {
    let guild_key = guild_id.to_string();
    let user_key = user_id.to_string();
    let record = repository::get_active_jail(pool, &guild_key, &user_key)
        .await?
        .context("That member is no longer in the active prisoner list")?;
    let saved_roles = repository::get_saved_roles(pool, record.id).await?;
    let overwrites = repository::get_overwrite_snapshots(pool, record.id).await?;
    let history = repository::get_jail_history(pool, &guild_key, &user_key).await?;
    let roles = match ratelimit::guard(Lane::Read, http.roles(guild_id)).await {
        Ok(response) => response.models().await.unwrap_or_default(),
        Err(error) => {
            tracing::debug!(
                ?error,
                "Could not load role names for a prisoner inspection"
            );
            Vec::new()
        }
    };
    let member = load_member(http, guild_id, user_id).await;
    let display_name = member
        .as_ref()
        .map(|member| {
            member
                .nick
                .as_deref()
                .unwrap_or(&member.user.name)
                .to_owned()
        })
        .unwrap_or_else(|| format!("Unknown member {user_id}"));
    let avatar = member
        .as_ref()
        .map(|member| avatar_url(user_id, member.user.avatar))
        .unwrap_or_else(|| default_avatar_url(user_id));
    let actor_id = parse_user_id(&record.moderator_id).ok();
    let actor_name = if let Some(actor_id) = actor_id {
        load_member(http, guild_id, actor_id)
            .await
            .map(|member| member.user.name)
            .unwrap_or_else(|| format!("Unknown moderator {actor_id}"))
    } else {
        format!("Unknown moderator {}", record.moderator_id)
    };

    let current_roles = member
        .as_ref()
        .map(|member| role_labels(&member.roles, &roles))
        .unwrap_or_else(|| "Member is not currently in the server".to_owned());
    let original_roles = role_snapshot_labels(&saved_roles, &roles);
    let history_text = history
        .iter()
        .take(8)
        .map(|item| {
            format!(
                "`#{}` {} | {} | {}",
                item.id,
                item.status,
                format_timestamp(&item.jailed_at),
                item.reason.as_deref().unwrap_or("No reason provided")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let overwrite_text = if overwrites.is_empty() {
        "No member-specific channel overwrites were saved for this sentence.".to_owned()
    } else {
        overwrites
            .iter()
            .map(|entry| {
                format!(
                    "Channel `{}` | allow `{}` | deny `{}`",
                    entry.channel_id, entry.allow_bits, entry.deny_bits
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut detail = Embed {
        author: None,
        color: Some(COLOR_RECORD),
        description: Some(format!(
            "Full active record for **{display_name}**. Member ID: `{user_id}` | guild `{guild_id}` | record `#{}`",
            record.id,
        )),
        fields: vec![
            EmbedField {
                inline: false,
                name: "Arrest details".to_owned(),
                value: format!(
                    "**Reason:** {}\n**Arrested:** {}\n**Expires:** {}\n**Moderator:** {actor_name} (`{}`)",
                    truncate(
                        record.reason.as_deref().unwrap_or("No reason provided"),
                        800
                    ),
                    format_timestamp(&record.jailed_at),
                    record
                        .release_at
                        .as_deref()
                        .map(format_timestamp)
                        .unwrap_or_else(|| "Indefinite".to_owned()),
                    record.moderator_id,
                ),
            },
            EmbedField {
                inline: true,
                name: "Current custody".to_owned(),
                value: format!(
                    "**Username:** {}\n**Status:** {}\n**In server:** {}\n**Current nickname:** {}\n**Current roles:** {}",
                    member
                        .as_ref()
                        .map(|member| member.user.name.as_str())
                        .unwrap_or("Unavailable"),
                    record.status,
                    if record.left_server { "No" } else { "Yes" },
                    member
                        .as_ref()
                        .and_then(|member| member.nick.as_deref())
                        .unwrap_or("None"),
                    truncate(&current_roles, 900),
                ),
            },
            EmbedField {
                inline: true,
                name: "Saved state".to_owned(),
                value: format!(
                    "**Original nickname:** {}\n**Original roles:** {}",
                    record.original_nickname.as_deref().unwrap_or("None"),
                    truncate(&original_roles, 900),
                ),
            },
            EmbedField {
                inline: false,
                name: "Sentence activity".to_owned(),
                value: format!(
                    "**Left server:** {}\n**Left at:** {}\n**Last rejoined:** {}\n**Rejoin count:** {}\n**Released at:** {}\n**Released by:** {}",
                    record.left_server,
                    record
                        .left_at
                        .as_deref()
                        .map(format_timestamp)
                        .unwrap_or_else(|| "Never".to_owned()),
                    record
                        .last_rejoined_at
                        .as_deref()
                        .map(format_timestamp)
                        .unwrap_or_else(|| "Never".to_owned()),
                    record.rejoin_count,
                    record
                        .released_at
                        .as_deref()
                        .map(format_timestamp)
                        .unwrap_or_else(|| "Not released".to_owned()),
                    record.released_by.as_deref().unwrap_or("Not released"),
                ),
            },
            EmbedField {
                inline: false,
                name: format!("Saved channel overwrites ({})", overwrites.len()),
                value: truncate(&overwrite_text, 1024),
            },
            EmbedField {
                inline: false,
                name: format!("Jail history ({})", history.len()),
                value: if history_text.is_empty() {
                    "No jail history found.".to_owned()
                } else {
                    truncate(&history_text, 1024)
                },
            },
        ],
        flags: None,
        footer: Some(EmbedFooter {
            icon_url: None,
            proxy_icon_url: None,
            text: format!("XP Jail Terminal | record #{}", record.id),
        }),
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: Some(EmbedThumbnail {
            height: None,
            proxy_url: None,
            url: avatar,
            width: None,
        }),
        timestamp: None,
        title: Some(format!("Prisoner inspection | {display_name}")),
        url: None,
        video: None,
    };
    if history.len() > 8 {
        if let Some(field) = detail
            .fields
            .iter_mut()
            .find(|field| field.name.starts_with("Jail history"))
        {
            field
                .value
                .push_str(&format!("\n…and {} older records.", history.len() - 8));
        }
    }
    let mut banner = terminal_embed(
        "XP Jail Terminal | Record inspection",
        "Member details, current custody state, saved restoration data, and recent jail history.",
    );
    banner.thumbnail = None;
    detail.fields.shrink_to_fit();
    Ok(vec![banner, detail])
}

pub async fn profile(
    http: &HttpClient,
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
) -> (String, String) {
    let member = load_member(http, guild_id, user_id).await;
    let display_name = member
        .as_ref()
        .map(|member| {
            member
                .nick
                .as_deref()
                .unwrap_or(&member.user.name)
                .to_owned()
        })
        .unwrap_or_else(|| format!("Unknown member {user_id}"));
    let avatar = member
        .as_ref()
        .map(|member| avatar_url(user_id, member.user.avatar))
        .unwrap_or_else(|| default_avatar_url(user_id));
    (display_name, avatar)
}

pub fn inspect_components(
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    page: usize,
    user_id: Id<UserMarker>,
) -> Vec<Component> {
    row(vec![
        button(
            &format!(
                "prisoner:release_selected:{}:{}:{}:{}",
                guild_id, owner_id, page, user_id
            ),
            "Release this member",
            ButtonStyle::Danger,
            false,
        ),
        button(
            &format!("prisoner:back:{}:{}:{}", guild_id, owner_id, page),
            "Back to roster",
            ButtonStyle::Secondary,
            false,
        ),
    ])
}

pub fn confirmation_components(
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    page: usize,
    user_id: Id<UserMarker>,
) -> Vec<Component> {
    row(vec![
        button(
            &format!(
                "prisoner:confirm_release:{}:{}:{}:{}",
                guild_id, owner_id, page, user_id
            ),
            "Yes, release",
            ButtonStyle::Danger,
            false,
        ),
        button(
            &format!("prisoner:back:{}:{}:{}", guild_id, owner_id, page),
            "Cancel",
            ButtonStyle::Secondary,
            false,
        ),
    ])
}

pub fn selection_components(
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    page: usize,
    action: &str,
    records: &[JailRecord],
    names: &[String],
) -> Vec<Component> {
    let options: Vec<SelectMenuOption> = records
        .iter()
        .zip(names)
        .map(|(record, name)| SelectMenuOption {
            default: false,
            description: Some(truncate(
                &format!(
                    "{} | {}",
                    record.reason.as_deref().unwrap_or("No reason provided"),
                    short_date(&record.jailed_at)
                ),
                100,
            )),
            emoji: None,
            label: truncate(name, 100),
            value: record.user_id.clone(),
        })
        .collect();
    let custom_id = format!("prisoner:select_{action}:{guild_id}:{owner_id}:{page}");
    let select = Component::SelectMenu(SelectMenu {
        channel_types: None,
        custom_id,
        default_values: None,
        disabled: options.is_empty(),
        id: None,
        kind: SelectMenuType::Text,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(options),
        placeholder: Some(
            if action == "inspect" {
                "Choose a prisoner to inspect"
            } else {
                "Choose a prisoner to release"
            }
            .to_owned(),
        ),
        required: None,
    });
    vec![
        Component::ActionRow(ActionRow {
            components: vec![select],
            id: None,
        }),
        Component::ActionRow(ActionRow {
            components: vec![Component::Button(button(
                &format!("prisoner:back:{guild_id}:{owner_id}:{page}"),
                "Back to roster",
                ButtonStyle::Secondary,
                false,
            ))],
            id: None,
        }),
    ]
}

pub fn confirm_release_embeds(record: &JailRecord, display_name: &str, avatar: &str) -> Vec<Embed> {
    let mut banner = terminal_embed(
        "XP Jail Terminal | Confirm release",
        "Review the selected prisoner before restoring their saved member state.",
    );
    banner.thumbnail = None;
    let mut card = Embed {
        author: None,
        color: Some(COLOR_RECORD),
        description: Some(format!(
            "Release **{display_name}** (`{}`)? Their original nickname and restorable roles will be restored.",
            record.user_id
        )),
        fields: vec![
            EmbedField {
                inline: false,
                name: "Reason".to_owned(),
                value: truncate(
                    record.reason.as_deref().unwrap_or("No reason provided"),
                    1024,
                ),
            },
            EmbedField {
                inline: true,
                name: "Arrested".to_owned(),
                value: format_timestamp(&record.jailed_at),
            },
            EmbedField {
                inline: true,
                name: "Expiry".to_owned(),
                value: record
                    .release_at
                    .as_deref()
                    .map(format_timestamp)
                    .unwrap_or_else(|| "Indefinite".to_owned()),
            },
        ],
        flags: None,
        footer: Some(EmbedFooter {
            icon_url: None,
            proxy_icon_url: None,
            text: format!("Record #{} | confirm to proceed", record.id),
        }),
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: Some(EmbedThumbnail {
            height: None,
            proxy_url: None,
            url: avatar.to_owned(),
            width: None,
        }),
        timestamp: None,
        title: Some("Are you sure?".to_owned()),
        url: None,
        video: None,
    };
    card.fields.shrink_to_fit();
    vec![banner, card]
}

fn terminal_roster_attachment(
    total: i64,
    page: usize,
    page_count: usize,
    records: &[JailRecord],
    names: &[String],
) -> Vec<Attachment> {
    let mut canvas = Raster::new(900, 270, rgb(23, 55, 94));
    canvas.frame(8, 6, 884, 258, rgb(214, 211, 206));
    for y in 10..40 {
        let step = (y - 10) as u16;
        canvas.fill(
            12,
            y,
            876,
            1,
            rgb(
                (34 * step / 30) as u8,
                (72 + 116 * step / 30) as u8,
                (190 + 61 * step / 30) as u8,
            ),
        );
    }
    canvas.frame(21, 16, 20, 18, rgb(198, 230, 245));
    canvas.fill(24, 19, 14, 10, rgb(18, 59, 96));
    canvas.fill(28, 31, 6, 2, rgb(77, 97, 120));
    canvas.text(49, 20, "PRISONER CONTROL CENTER", rgb(255, 255, 255), 2);
    canvas.fill(12, 40, 876, 23, rgb(236, 233, 216));
    canvas.text(24, 48, "FILE   VIEW   CUSTODY   HELP", rgb(37, 54, 74), 1);

    canvas.frame(22, 72, 183, 155, rgb(232, 231, 226));
    canvas.fill(25, 75, 177, 20, rgb(36, 94, 219));
    canvas.text(33, 82, "SYSTEM STATUS", rgb(255, 255, 255), 1);
    canvas.frame(35, 106, 155, 55, rgb(16, 26, 32));
    canvas.text(46, 115, "SECURE", rgb(100, 245, 140), 2);
    canvas.text(47, 143, "ACCESS: STAFF", rgb(176, 243, 193), 1);
    canvas.fill(36, 173, 12, 12, rgb(67, 200, 103));
    canvas.text(55, 176, &format!("ACTIVE: {total}"), rgb(37, 54, 74), 1);
    canvas.fill(36, 196, 12, 12, rgb(68, 169, 242));
    canvas.text(
        55,
        199,
        &format!("PAGE {page}/{page_count}"),
        rgb(37, 54, 74),
        1,
    );

    canvas.frame(215, 72, 661, 155, rgb(16, 27, 32));
    canvas.fill(220, 77, 651, 23, rgb(24, 53, 68));
    canvas.text(233, 85, "ACTIVE DETENTION ROSTER", rgb(136, 255, 170), 1);
    canvas.text(251, 111, "MEMBER", rgb(136, 255, 170), 1);
    canvas.text(409, 111, "OFFENSE", rgb(136, 255, 170), 1);
    canvas.text(625, 111, "ARRESTED", rgb(136, 255, 170), 1);
    canvas.text(764, 111, "EXPIRY", rgb(136, 255, 170), 1);
    canvas.fill(229, 123, 633, 1, rgb(58, 102, 82));
    if records.is_empty() {
        canvas.text(
            246,
            157,
            "NO ACTIVE PRISONER RECORDS",
            rgb(102, 201, 137),
            1,
        );
    } else {
        for (index, record) in records.iter().enumerate() {
            let y = 130 + index * 23;
            canvas.avatar_icon(233, y + 1);
            canvas.fill(230, y + 19, 632, 1, rgb(38, 61, 54));
            canvas.text(
                251,
                y + 4,
                &ascii_label(
                    names.get(index).map(String::as_str).unwrap_or("UNKNOWN"),
                    25,
                ),
                rgb(160, 242, 180),
                1,
            );
            canvas.text(
                409,
                y + 4,
                &ascii_label(record.reason.as_deref().unwrap_or("NO REASON"), 34),
                rgb(119, 216, 152),
                1,
            );
            canvas.text(
                625,
                y + 4,
                &short_date(&record.jailed_at),
                rgb(119, 216, 152),
                1,
            );
            canvas.text(
                764,
                y + 4,
                &record
                    .release_at
                    .as_deref()
                    .map(short_date)
                    .unwrap_or_else(|| "INDEFINITE".to_owned()),
                rgb(119, 216, 152),
                1,
            );
        }
    }
    canvas.fill(12, 232, 876, 26, rgb(236, 233, 216));
    canvas.text(
        23,
        242,
        "ACTIVE CUSTODY | SELECT A MEMBER TO INSPECT",
        rgb(38, 56, 77),
        1,
    );
    canvas.text(758, 242, "CONNECTED", rgb(22, 122, 59), 1);
    let mut attachment = Attachment::from_bytes(IMAGE_FILENAME.to_owned(), canvas.png(), 0);
    attachment.description(format!(
        "Active prisoner terminal roster, page {page} of {page_count}, {total} total records"
    ));
    vec![attachment]
}

pub fn terminal_attachment() -> Vec<Attachment> {
    let mut attachment =
        Attachment::from_bytes(IMAGE_FILENAME.to_owned(), TERMINAL_IMAGE.to_vec(), 0);
    attachment.description("Retro Windows XP-inspired prisoner roster terminal".to_owned());
    vec![attachment]
}

pub fn terminal_embed(title: &str, description: &str) -> Embed {
    Embed {
        author: None,
        color: Some(COLOR_TERMINAL),
        description: Some(truncate(description, 4096)),
        fields: Vec::new(),
        flags: None,
        footer: Some(EmbedFooter {
            icon_url: None,
            proxy_icon_url: None,
            text: "PRISONER DATABASE | AUTHORIZED STAFF VIEW".to_owned(),
        }),
        image: Some(EmbedImage {
            height: Some(270),
            proxy_url: None,
            url: format!("attachment://{IMAGE_FILENAME}"),
            width: Some(900),
            content_type: None,
            placeholder: None,
            placeholder_version: None,
            flags: None,
        }),
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: None,
        timestamp: None,
        title: Some(truncate(title, 256)),
        url: None,
        video: None,
    }
}

fn roster_components(
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    page: usize,
    page_count: usize,
    records: &[JailRecord],
    names: &[String],
) -> Vec<Component> {
    let inspect_disabled = records.is_empty();
    let action_row = row(vec![
        button(
            &format!("prisoner:inspect:{guild_id}:{owner_id}:{page}"),
            "Inspect",
            ButtonStyle::Primary,
            inspect_disabled,
        ),
        button(
            &format!("prisoner:release:{guild_id}:{owner_id}:{page}"),
            "Release",
            ButtonStyle::Danger,
            inspect_disabled,
        ),
    ]);
    let page_select = page_select_row(guild_id, owner_id, page, page_count);
    let navigation = row(vec![
        button(
            &format!(
                "prisoner:page:{guild_id}:{owner_id}:{}",
                page.saturating_sub(1).max(1)
            ),
            "Previous page",
            ButtonStyle::Secondary,
            page <= 1,
        ),
        button(
            &format!(
                "prisoner:page:{guild_id}:{owner_id}:{}",
                (page + 1).min(page_count)
            ),
            "Next page",
            ButtonStyle::Secondary,
            page >= page_count,
        ),
    ]);
    let mut components = action_row;
    components.extend(page_select);
    components.extend(navigation);
    let _ = names;
    components
}

fn page_select_row(
    guild_id: Id<GuildMarker>,
    owner_id: Id<UserMarker>,
    page: usize,
    page_count: usize,
) -> Vec<Component> {
    let start = if page_count <= 25 {
        1
    } else {
        page.saturating_sub(12).max(1).min(page_count - 24)
    };
    let end = page_count.min(start + 24);
    let options = (start..=end)
        .map(|value| SelectMenuOption {
            default: false,
            description: Some(format!("Jump to page {value}")),
            emoji: None,
            label: format!("Page {value}"),
            value: value.to_string(),
        })
        .collect();
    vec![Component::ActionRow(ActionRow {
        components: vec![Component::SelectMenu(SelectMenu {
            channel_types: None,
            custom_id: format!("prisoner:choose_page:{guild_id}:{owner_id}"),
            default_values: None,
            disabled: page_count <= 1,
            id: None,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(options),
            placeholder: Some(format!("Page {page} of {page_count}")),
            required: None,
        })],
        id: None,
    })]
}

fn roster_card(
    record: &JailRecord,
    display_name: &str,
    avatar: &str,
    page: usize,
    index: usize,
) -> Embed {
    Embed {
        author: None,
        color: Some(COLOR_RECORD),
        description: Some(format!(
            "Member ID: `{}` | Record `#{}`",
            record.user_id, record.id
        )),
        fields: vec![
            EmbedField {
                inline: false,
                name: "Reason for arrest".to_owned(),
                value: truncate(
                    record.reason.as_deref().unwrap_or("No reason provided"),
                    1024,
                ),
            },
            EmbedField {
                inline: true,
                name: "Arrested".to_owned(),
                value: format_timestamp(&record.jailed_at),
            },
            EmbedField {
                inline: true,
                name: "Expiry".to_owned(),
                value: record
                    .release_at
                    .as_deref()
                    .map(format_timestamp)
                    .unwrap_or_else(|| "Indefinite".to_owned()),
            },
            EmbedField {
                inline: false,
                name: "Moderator".to_owned(),
                value: format!("<@{}>", record.moderator_id),
            },
        ],
        flags: None,
        footer: Some(EmbedFooter {
            icon_url: None,
            proxy_icon_url: None,
            text: format!("Page {page} | prisoner {index} of page"),
        }),
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: Some(EmbedThumbnail {
            height: None,
            proxy_url: None,
            url: avatar.to_owned(),
            width: None,
        }),
        timestamp: None,
        title: Some(truncate(display_name, 256)),
        url: None,
        video: None,
    }
}

fn row(buttons: Vec<Button>) -> Vec<Component> {
    vec![Component::ActionRow(ActionRow {
        components: buttons.into_iter().map(Component::Button).collect(),
        id: None,
    })]
}

fn button(custom_id: &str, label: &str, style: ButtonStyle, disabled: bool) -> Button {
    Button {
        custom_id: Some(custom_id.to_owned()),
        disabled,
        emoji: None,
        id: None,
        label: Some(label.to_owned()),
        sku_id: None,
        style,
        url: None,
    }
}

async fn load_member(
    http: &HttpClient,
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
) -> Option<Member> {
    match ratelimit::guard(Lane::Read, http.guild_member(guild_id, user_id)).await {
        Ok(response) => match response.model().await {
            Ok(member) => Some(member),
            Err(error) => {
                tracing::debug!(?error, %user_id, "Could not load prisoner member profile");
                None
            }
        },
        Err(error) => {
            tracing::debug!(?error, %user_id, "Could not load prisoner member profile");
            None
        }
    }
}

fn role_labels(
    role_ids: &[Id<twilight_model::id::marker::RoleMarker>],
    roles: &[twilight_model::guild::Role],
) -> String {
    if role_ids.is_empty() {
        return "None".to_owned();
    }
    role_ids
        .iter()
        .map(|id| {
            roles
                .iter()
                .find(|role| role.id == *id)
                .map(|role| format!("{} (`{id}`)", role.name))
                .unwrap_or_else(|| format!("Unknown role `{id}`"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn role_snapshot_labels(saved: &[SavedRole], roles: &[twilight_model::guild::Role]) -> String {
    if saved.is_empty() {
        return "No restorable roles were saved".to_owned();
    }
    saved
        .iter()
        .map(|saved| {
            let label = roles
                .iter()
                .find(|role| role.id.get().to_string() == saved.role_id)
                .map(|role| role.name.as_str())
                .unwrap_or("Unknown role");
            format!(
                "{label} (`{}`, position {}, {})",
                saved.role_id,
                saved.role_position,
                if saved.restorable {
                    "restorable"
                } else {
                    "not restorable"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_user_id(raw: &str) -> anyhow::Result<Id<UserMarker>> {
    Ok(Id::new(
        raw.parse::<u64>().context("Invalid prisoner user ID")?,
    ))
}

fn avatar_url(user_id: Id<UserMarker>, hash: Option<twilight_model::util::ImageHash>) -> String {
    match hash {
        Some(hash) => format!("https://cdn.discordapp.com/avatars/{user_id}/{hash}.png?size=128"),
        None => default_avatar_url(user_id),
    }
}

fn default_avatar_url(user_id: Id<UserMarker>) -> String {
    let index = (user_id.get() >> 22) % 6;
    format!("https://cdn.discordapp.com/embed/avatars/{index}.png")
}

fn format_timestamp(raw: &str) -> String {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map(|timestamp| {
            format!(
                "<t:{}:F> (<t:{}:R>)",
                timestamp.unix_timestamp(),
                timestamp.unix_timestamp()
            )
        })
        .unwrap_or_else(|_| format!("`{raw}`"))
}

fn short_date(raw: &str) -> String {
    raw.get(..10).unwrap_or(raw).to_owned()
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut value: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    value.push('…');
    value
}

fn ascii_label(value: &str, limit: usize) -> String {
    truncate(
        &value
            .chars()
            .map(|character| {
                if character.is_ascii() {
                    character.to_ascii_uppercase()
                } else {
                    '?'
                }
            })
            .collect::<String>()
            .replace(['\n', '\r'], " "),
        limit,
    )
}

const fn rgb(red: u8, green: u8, blue: u8) -> [u8; 3] {
    [red, green, blue]
}

struct Raster {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Raster {
    fn new(width: usize, height: usize, fill: [u8; 3]) -> Self {
        Self {
            width,
            height,
            pixels: fill.repeat(width * height),
        }
    }

    fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, color: [u8; 3]) {
        for row in y.min(self.height)..y.saturating_add(height).min(self.height) {
            for column in x.min(self.width)..x.saturating_add(width).min(self.width) {
                let offset = (row * self.width + column) * 3;
                self.pixels[offset..offset + 3].copy_from_slice(&color);
            }
        }
    }

    fn frame(&mut self, x: usize, y: usize, width: usize, height: usize, color: [u8; 3]) {
        self.fill(x, y, width, height, color);
        self.fill(x, y, width, 2, rgb(255, 255, 255));
        self.fill(x, y, 2, height, rgb(255, 255, 255));
        self.fill(x, y + height - 2, width, 2, rgb(59, 82, 104));
        self.fill(x + width - 2, y, 2, height, rgb(59, 82, 104));
    }

    fn avatar_icon(&mut self, x: usize, y: usize) {
        self.fill(x, y, 13, 13, rgb(200, 218, 230));
        self.fill(x + 4, y + 2, 5, 5, rgb(63, 107, 141));
        self.fill(x + 2, y + 8, 9, 4, rgb(63, 107, 141));
    }

    fn text(&mut self, x: usize, y: usize, value: &str, color: [u8; 3], scale: usize) {
        let mut cursor = x;
        for character in value.chars() {
            if character == ' ' {
                cursor += 4 * scale;
                continue;
            }
            for (row, mask) in glyph(character).iter().enumerate() {
                for column in 0..5 {
                    if mask & (1 << (4 - column)) != 0 {
                        self.fill(
                            cursor + column * scale,
                            y + row * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
            cursor += 6 * scale;
        }
    }

    fn png(&self) -> Vec<u8> {
        let row_len = self.width * 3;
        let mut raw = Vec::with_capacity((row_len + 1) * self.height);
        for row in 0..self.height {
            raw.push(0);
            let start = row * row_len;
            raw.extend_from_slice(&self.pixels[start..start + row_len]);
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::with_capacity(13);
        header.extend_from_slice(&(self.width as u32).to_be_bytes());
        header.extend_from_slice(&(self.height as u32).to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        png_chunk(&mut png, b"IHDR", &header);
        png_chunk(&mut png, b"IDAT", &zlib_uncompressed(&raw));
        png_chunk(&mut png, b"IEND", &[]);
        png
    }
}

fn glyph(character: char) -> [u8; 7] {
    match character.to_ascii_uppercase() {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [15, 16, 16, 16, 16, 16, 15],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [15, 16, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [31, 4, 4, 4, 4, 4, 31],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        ':' => [0, 4, 4, 0, 4, 4, 0],
        '/' => [1, 2, 2, 4, 8, 8, 16],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 6, 6],
        '|' => [4, 4, 4, 4, 4, 4, 4],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}

fn zlib_uncompressed(data: &[u8]) -> Vec<u8> {
    let mut output = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < data.len() {
        let length = (data.len() - offset).min(65_535);
        let final_block = offset + length == data.len();
        output.push(u8::from(final_block));
        let length = length as u16;
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(&(!length).to_le_bytes());
        output.extend_from_slice(&data[offset..offset + usize::from(length)]);
        offset += usize::from(length);
    }
    let (mut first, mut second) = (1_u32, 0_u32);
    for byte in data {
        first = (first + u32::from(*byte)) % 65_521;
        second = (second + first) % 65_521;
    }
    output.extend_from_slice(&((second << 16) | first).to_be_bytes());
    output
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    output.extend_from_slice(&(!crc).to_be_bytes());
}
