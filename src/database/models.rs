use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct GuildConfig {
    pub guild_id: String,
    pub prisoner_role_id: Option<String>,
    pub jail_channel_id: Option<String>,
    pub logs_channel_id: Option<String>,
    pub setup_complete: bool,
    pub auto_release_enabled: bool,
    pub permissions_last_reconciled_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct JailRecord {
    pub id: i64,
    pub guild_id: String,
    pub user_id: String,
    pub status: String,
    pub reason: Option<String>,
    pub moderator_id: String,
    pub jailed_at: String,
    pub release_at: Option<String>,
    pub released_at: Option<String>,
    pub released_by: Option<String>,
    pub original_nickname: Option<String>,
    pub left_server: bool,
    pub left_at: Option<String>,
    pub last_rejoined_at: Option<String>,
    pub rejoin_count: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct SavedRole {
    pub jail_record_id: i64,
    pub role_id: String,
    pub role_position: i64,
    pub restorable: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuditLog {
    pub id: i64,
    pub guild_id: String,
    pub actor_id: Option<String>,
    pub target_user_id: Option<String>,
    pub event_type: String,
    pub reason: Option<String>,
    pub details_json: String,
    pub created_at: String,
}
