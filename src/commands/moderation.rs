use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, RoleMarker, UserMarker},
};

#[derive(CommandModel, CreateCommand)]
#[command(name = "jail", desc = "Jail a member")]
pub struct JailCommand {
    /// The member to jail.
    pub member: Id<UserMarker>,

    /// Sentence duration, such as 30m, 2h, 3d, or 1w. Leave empty for indefinite.
    pub duration: Option<String>,

    /// Reason for the jail.
    pub reason: Option<String>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "unjail", desc = "Release a jailed member")]
pub struct UnjailCommand {
    /// The member to release.
    pub member: Id<UserMarker>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "setlogs", desc = "Set the moderation log channel")]
pub struct SetLogsCommand {
    /// The channel where moderation logs will be sent.
    #[command(channel_types = "guild_text")]
    pub channel: Id<ChannelMarker>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "prisoners", desc = "List currently jailed members")]
pub struct PrisonersCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "prisoner", desc = "Inspect or release jailed members")]
pub enum PrisonerCommand {
    #[command(name = "list")]
    List(PrisonerListCommand),
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "list", desc = "Browse the current prisoner roster")]
pub struct PrisonerListCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "jailinfo", desc = "View a member's jail information")]
pub struct JailInfoCommand {
    /// The member whose jail information you want to view.
    pub member: Id<UserMarker>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "jailconfig", desc = "View this server's jail configuration")]
pub struct JailConfigCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "help", desc = "Show the bot's commands and permissions")]
pub struct HelpCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "changelog", desc = "Show recent bot changes")]
pub struct ChangelogCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "quarantine", desc = "Enable or disable the quarantine service")]
pub struct QuarantineCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "enable", desc = "Enable a quarantine module")]
pub struct EnableModuleCommand {
    /// Module key shown by `/quarantine`.
    pub module: String,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "disable", desc = "Disable a quarantine module")]
pub struct DisableModuleCommand {
    /// Module key shown by `/quarantine`.
    pub module: String,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "sanitize", desc = "Repair jail permissions in every channel")]
pub struct SanitizeCommand;

#[derive(CommandModel, CreateCommand)]
#[command(name = "set", desc = "Configure bot moderation roles")]
pub enum SetCommand {
    #[command(name = "moderator")]
    Moderator(SetModeratorCommands),
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "moderator", desc = "Configure the moderator role")]
pub enum SetModeratorCommands {
    #[command(name = "role")]
    Role(SetModeratorRoleCommand),
}

#[derive(CommandModel, CreateCommand)]
#[command(
    name = "role",
    desc = "Set the role allowed to jail lower-ranked members"
)]
pub struct SetModeratorRoleCommand {
    /// The server's moderator role.
    pub role: Id<RoleMarker>,
}
