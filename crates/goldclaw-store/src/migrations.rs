#[derive(Clone, Debug)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create_core_tables",
        sql: r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived_at TEXT
);

CREATE TABLE IF NOT EXISTS session_bindings (
    binding_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_instance TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    sender_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    source TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS task_queue (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS connector_states (
    connector_key TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    last_heartbeat_at TEXT,
    detail_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS doctor_snapshots (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    severity TEXT NOT NULL,
    payload_json TEXT NOT NULL
);
"#,
    },
    Migration {
        version: 2,
        name: "add_runtime_indexes",
        sql: r#"
CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
    ON sessions(updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_session_bindings_session_id
    ON session_bindings(session_id);

CREATE INDEX IF NOT EXISTS idx_messages_session_created_at
    ON messages(session_id, created_at);

CREATE INDEX IF NOT EXISTS idx_task_queue_status
    ON task_queue(status, updated_at);
"#,
    },
    Migration {
        version: 3,
        name: "add_message_roles",
        sql: r#"
ALTER TABLE messages
    ADD COLUMN role TEXT NOT NULL DEFAULT 'user';
"#,
    },
];

pub const fn current_schema_version() -> u32 {
    MIGRATIONS[MIGRATIONS.len() - 1].version
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_tracks_latest_migration() {
        assert_eq!(current_schema_version(), 3);
    }

    #[test]
    fn migrations_are_sorted() {
        assert!(
            MIGRATIONS
                .windows(2)
                .all(|pair| pair[0].version < pair[1].version)
        );
    }
}
