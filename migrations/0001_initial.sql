
-- ==========================================
-- RUST JAIL SYSTEM - INITIAL DATABASE
-- Multi-server SQLite architecture
-- ==========================================

-- 1. Server configuration
CREATE TABLE guild_configs (
    guild_id TEXT PRIMARY KEY,

    prisoner_role_id TEXT,
    jail_channel_id TEXT,
    logs_channel_id TEXT,

    setup_complete INTEGER NOT NULL DEFAULT 0,
    auto_release_enabled INTEGER NOT NULL DEFAULT 1,

    permissions_last_reconciled_at TEXT,

    created_at TEXT NOT NULL DEFAULT
        (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    updated_at TEXT NOT NULL DEFAULT
        (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- 2. Jail records
CREATE TABLE jail_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    guild_id TEXT NOT NULL,
    user_id TEXT NOT NULL,

    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'released', 'expired')),

    reason TEXT,
    moderator_id TEXT NOT NULL,

    jailed_at TEXT NOT NULL DEFAULT
        (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    release_at TEXT,
    released_at TEXT,
    released_by TEXT,

    original_nickname TEXT,

    left_server INTEGER NOT NULL DEFAULT 0,
    left_at TEXT,
    last_rejoined_at TEXT,
    rejoin_count INTEGER NOT NULL DEFAULT 0
);

-- Only one active sentence per user per server.
CREATE UNIQUE INDEX idx_one_active_jail
ON jail_records(guild_id, user_id)
WHERE status = 'active';

CREATE INDEX idx_jail_guild_status
ON jail_records(guild_id, status);

CREATE INDEX idx_jail_release
ON jail_records(status, release_at);

-- 3. Original member roles
CREATE TABLE saved_roles (
    jail_record_id INTEGER NOT NULL,
    role_id TEXT NOT NULL,

    role_position INTEGER NOT NULL DEFAULT 0,

    -- Managed roles cannot necessarily be restored.
    restorable INTEGER NOT NULL DEFAULT 1,

    PRIMARY KEY (jail_record_id, role_id),

    FOREIGN KEY (jail_record_id)
        REFERENCES jail_records(id)
        ON DELETE CASCADE
);

-- 4. Original member-specific channel permissions
CREATE TABLE member_overwrite_snapshots (
    jail_record_id INTEGER NOT NULL,
    channel_id TEXT NOT NULL,

    allow_bits TEXT NOT NULL,
    deny_bits TEXT NOT NULL,

    PRIMARY KEY (jail_record_id, channel_id),

    FOREIGN KEY (jail_record_id)
        REFERENCES jail_records(id)
        ON DELETE CASCADE
);

-- 5. Audit and moderation logs
CREATE TABLE audit_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    guild_id TEXT NOT NULL,

    actor_id TEXT,
    target_user_id TEXT,

    event_type TEXT NOT NULL,
    reason TEXT,

    details_json TEXT NOT NULL DEFAULT '{}',

    created_at TEXT NOT NULL DEFAULT
        (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_audit_guild
ON audit_logs(guild_id, created_at);

CREATE INDEX idx_audit_target
ON audit_logs(guild_id, target_user_id);
