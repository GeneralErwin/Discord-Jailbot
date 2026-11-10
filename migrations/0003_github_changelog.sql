CREATE TABLE github_changelog_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sha TEXT NOT NULL UNIQUE,
    version_major INTEGER NOT NULL,
    version_minor INTEGER NOT NULL,
    message TEXT NOT NULL,
    html_url TEXT NOT NULL,
    committed_at TEXT NOT NULL
);

CREATE TABLE github_changelog_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_sha TEXT NOT NULL,
    version_major INTEGER NOT NULL,
    version_minor INTEGER NOT NULL
);
