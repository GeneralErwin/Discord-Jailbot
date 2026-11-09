use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, UserMarker},
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
#[command(name = "jailinfo", desc = "View a member's jail information")]
pub struct JailInfoCommand {
    /// The member whose jail information you want to view.
    pub member: Id<UserMarker>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "jailconfig", desc = "View this server's jail configuration")]
pub struct JailConfigCommand;
