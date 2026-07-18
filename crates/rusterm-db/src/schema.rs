pub const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS connections (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    config TEXT NOT NULL,
    group_name TEXT,
    tags TEXT,
    onekey INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS history (
    id TEXT PRIMARY KEY,
    command TEXT NOT NULL,
    session_id TEXT NOT NULL,
    cwd TEXT,
    hostname TEXT,
    exit_code INTEGER,
    duration_ms INTEGER,
    created_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(
    command,
    cwd,
    hostname,
    content='history',
    content_rowid='rowid'
);

CREATE TABLE IF NOT EXISTS session_log (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    data BLOB NOT NULL,
    created_at TEXT NOT NULL
);

-- Hosts that have already been "auto-configured to the left side" after a
-- successful SSH login. Recording the final success here lets subsequent
-- SSH logins to the same host skip the configuration step entirely
-- (idempotency: avoid duplicate configuration). Intermediate debug
-- steps are NOT recorded — only the final success is.
CREATE TABLE IF NOT EXISTS configured_hosts (
    host TEXT PRIMARY KEY,
    configured_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_history_session ON history(session_id);
CREATE INDEX IF NOT EXISTS idx_history_created ON history(created_at);
CREATE INDEX IF NOT EXISTS idx_history_command_created ON history(command, created_at);
CREATE INDEX IF NOT EXISTS idx_session_log_session ON session_log(session_id);

CREATE TRIGGER IF NOT EXISTS history_ai AFTER INSERT ON history BEGIN
    INSERT INTO history_fts(rowid, command, cwd, hostname)
    VALUES (new.rowid, new.command, new.cwd, new.hostname);
END;

CREATE TRIGGER IF NOT EXISTS history_ad AFTER DELETE ON history BEGIN
    INSERT INTO history_fts(history_fts, rowid, command, cwd, hostname)
    VALUES ('delete', old.rowid, old.command, old.cwd, old.hostname);
END;

CREATE TRIGGER IF NOT EXISTS history_au AFTER UPDATE ON history BEGIN
    INSERT INTO history_fts(history_fts, rowid, command, cwd, hostname)
    VALUES ('delete', old.rowid, old.command, old.cwd, old.hostname);
    INSERT INTO history_fts(rowid, command, cwd, hostname)
    VALUES (new.rowid, new.command, new.cwd, new.hostname);
END;
"#;
