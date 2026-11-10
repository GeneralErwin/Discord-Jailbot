ALTER TABLE guild_configs
    ADD COLUMN moderator_role_id TEXT;

ALTER TABLE guild_configs
    ADD COLUMN quarantine_enabled INTEGER NOT NULL DEFAULT 0;

CREATE TABLE quarantine_modules (
    guild_id TEXT NOT NULL,
    module_key TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (guild_id, module_key),
    FOREIGN KEY (guild_id) REFERENCES guild_configs(guild_id) ON DELETE CASCADE
);
