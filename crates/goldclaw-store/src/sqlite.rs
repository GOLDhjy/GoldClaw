use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use goldclaw_core::{
    EnvelopeSource, GoldClawError, MessageRole, SessionBinding, SessionId, SessionMessage,
    SessionSummary,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;

use crate::{MIGRATIONS, StoreLayout, current_schema_version};

pub type StoreResult<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid stored data: {0}")]
    InvalidData(String),
    #[error("internal store lock poisoned")]
    LockPoisoned,
}

#[derive(Clone, Debug, Default)]
pub struct StoreSnapshot {
    pub sessions: Vec<SessionSummary>,
    pub bindings: Vec<SessionBinding>,
    pub messages: Vec<SessionMessage>,
}

#[derive(Clone, Debug)]
pub struct StoreInspection {
    pub database_exists: bool,
    pub applied_schema_version: u32,
    pub target_schema_version: u32,
}

impl StoreInspection {
    pub fn has_pending_migrations(&self) -> bool {
        self.database_exists && self.applied_schema_version < self.target_schema_version
    }
}

#[derive(Clone)]
pub struct SqliteStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    layout: StoreLayout,
    connection: Mutex<Connection>,
}

impl SqliteStore {
    pub fn inspect(layout: &StoreLayout) -> StoreResult<StoreInspection> {
        let database_file = layout.paths().database_file.clone();
        if !database_file.exists() {
            return Ok(StoreInspection {
                database_exists: false,
                applied_schema_version: 0,
                target_schema_version: current_schema_version(),
            });
        }

        let connection =
            Connection::open_with_flags(&database_file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let applied_schema_version = read_schema_version(&connection)?;
        Ok(StoreInspection {
            database_exists: true,
            applied_schema_version,
            target_schema_version: current_schema_version(),
        })
    }

    pub fn open(layout: StoreLayout) -> StoreResult<Self> {
        layout.ensure_parent_dirs()?;
        let database_file = layout.paths().database_file.clone();
        let database_exists = database_file.exists();
        let connection = Connection::open(&database_file)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let store = Self {
            inner: Arc::new(StoreInner {
                layout,
                connection: Mutex::new(connection),
            }),
        };
        store.initialize(database_exists)?;
        Ok(store)
    }

    pub fn layout(&self) -> &StoreLayout {
        &self.inner.layout
    }

    pub fn applied_schema_version(&self) -> StoreResult<u32> {
        let conn = self.connection()?;
        read_schema_version(&conn)
    }

    pub fn has_pending_migrations(&self) -> StoreResult<bool> {
        Ok(self.applied_schema_version()? < current_schema_version())
    }

    pub fn list_sessions(&self) -> StoreResult<Vec<SessionSummary>> {
        let conn = self.connection()?;
        let mut statement = conn.prepare(
            r#"
SELECT id, title, created_at, updated_at
FROM sessions
WHERE archived_at IS NULL
ORDER BY updated_at DESC
"#,
        )?;

        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: parse_uuid(row.get::<_, String>(0)?)?,
                title: row.get(1)?,
                created_at: parse_datetime(row.get::<_, String>(2)?)?,
                updated_at: parse_datetime(row.get::<_, String>(3)?)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_session(&self, session_id: SessionId) -> StoreResult<Option<SessionSummary>> {
        let conn = self.connection()?;
        conn.query_row(
            r#"
SELECT id, title, created_at, updated_at
FROM sessions
WHERE id = ?1 AND archived_at IS NULL
"#,
            params![session_id.to_string()],
            |row| {
                Ok(SessionSummary {
                    id: parse_uuid(row.get::<_, String>(0)?)?,
                    title: row.get(1)?,
                    created_at: parse_datetime(row.get::<_, String>(2)?)?,
                    updated_at: parse_datetime(row.get::<_, String>(3)?)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn upsert_session(&self, session: &SessionSummary) -> StoreResult<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"
INSERT INTO sessions (id, title, created_at, updated_at, archived_at)
VALUES (?1, ?2, ?3, ?4, NULL)
ON CONFLICT(id) DO UPDATE SET
    title = excluded.title,
    updated_at = excluded.updated_at,
    archived_at = NULL
"#,
            params![
                session.id.to_string(),
                &session.title,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_session_binding(&self, binding: &SessionBinding) -> StoreResult<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"
INSERT INTO session_bindings (
    binding_key,
    session_id,
    source_kind,
    source_instance,
    conversation_id,
    sender_id,
    created_at,
    updated_at
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(binding_key) DO UPDATE SET
    session_id = excluded.session_id,
    sender_id = excluded.sender_id,
    updated_at = excluded.updated_at
"#,
            params![
                binding.binding_key(),
                binding.session_id.to_string(),
                binding.source.key(),
                &binding.source_instance,
                &binding.conversation_id,
                &binding.sender_id,
                binding.created_at.to_rfc3339(),
                binding.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn resolve_binding(&self, binding_key: &str) -> StoreResult<Option<SessionBinding>> {
        let conn = self.connection()?;
        conn.query_row(
            r#"
SELECT session_id, source_kind, source_instance, conversation_id, sender_id, created_at, updated_at
FROM session_bindings
WHERE binding_key = ?1
"#,
            params![binding_key],
            |row| {
                let source_raw: String = row.get(1)?;
                let source = EnvelopeSource::from_key(&source_raw).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(GoldClawError::InvalidInput(format!(
                            "unknown source `{source_raw}`"
                        ))),
                    )
                })?;

                Ok(SessionBinding {
                    session_id: parse_uuid(row.get::<_, String>(0)?)?,
                    source,
                    source_instance: row.get(2)?,
                    conversation_id: row.get(3)?,
                    sender_id: row.get(4)?,
                    created_at: parse_datetime(row.get::<_, String>(5)?)?,
                    updated_at: parse_datetime(row.get::<_, String>(6)?)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn list_bindings(&self) -> StoreResult<Vec<SessionBinding>> {
        let conn = self.connection()?;
        let mut statement = conn.prepare(
            r#"
SELECT session_id, source_kind, source_instance, conversation_id, sender_id, created_at, updated_at
FROM session_bindings
ORDER BY updated_at DESC
"#,
        )?;

        let rows = statement.query_map([], |row| {
            let source_raw: String = row.get(1)?;
            let source = EnvelopeSource::from_key(&source_raw).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(GoldClawError::InvalidInput(format!(
                        "unknown source `{source_raw}`"
                    ))),
                )
            })?;

            Ok(SessionBinding {
                session_id: parse_uuid(row.get::<_, String>(0)?)?,
                source,
                source_instance: row.get(2)?,
                conversation_id: row.get(3)?,
                sender_id: row.get(4)?,
                created_at: parse_datetime(row.get::<_, String>(5)?)?,
                updated_at: parse_datetime(row.get::<_, String>(6)?)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn append_message(&self, message: &SessionMessage) -> StoreResult<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"
INSERT INTO messages (id, session_id, role, source, content, created_at, metadata_json)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#,
            params![
                message.id.to_string(),
                message.session_id.to_string(),
                message.role.as_str(),
                message.source.key(),
                &message.content,
                message.created_at.to_rfc3339(),
                serde_json::to_string(&message.metadata)?,
            ],
        )?;
        Ok(())
    }

    pub fn list_messages(&self) -> StoreResult<Vec<SessionMessage>> {
        let conn = self.connection()?;
        let mut statement = conn.prepare(
            r#"
SELECT id, session_id, role, source, content, created_at, metadata_json
FROM messages
ORDER BY created_at ASC
"#,
        )?;

        let rows = statement.query_map([], |row| {
            let role_raw: String = row.get(2)?;
            let source_raw: String = row.get(3)?;
            let role = MessageRole::parse(&role_raw).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(GoldClawError::InvalidInput(format!(
                        "unknown message role `{role_raw}`"
                    ))),
                )
            })?;
            let source = EnvelopeSource::from_key(&source_raw).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(GoldClawError::InvalidInput(format!(
                        "unknown source `{source_raw}`"
                    ))),
                )
            })?;

            Ok(SessionMessage {
                id: parse_uuid(row.get::<_, String>(0)?)?,
                session_id: parse_uuid(row.get::<_, String>(1)?)?,
                role,
                source,
                content: row.get(4)?,
                created_at: parse_datetime(row.get::<_, String>(5)?)?,
                metadata: serde_json::from_str(&row.get::<_, String>(6)?)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_snapshot(&self) -> StoreResult<StoreSnapshot> {
        Ok(StoreSnapshot {
            sessions: self.list_sessions()?,
            bindings: self.list_bindings()?,
            messages: self.list_messages()?,
        })
    }

    fn initialize(&self, database_exists: bool) -> StoreResult<()> {
        let current_version = self.applied_schema_version()?;
        if database_exists
            && current_version < current_schema_version()
            && file_has_contents(&self.inner.layout.paths().database_file)?
        {
            self.backup_database()?;
        }
        self.apply_migrations(current_version)?;
        Ok(())
    }

    fn apply_migrations(&self, current_version: u32) -> StoreResult<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version > current_version)
        {
            tx.execute_batch(migration.sql)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
                params![migration.version, migration.name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn backup_database(&self) -> StoreResult<PathBuf> {
        let backup_path = self.inner.layout.backup_path(SystemTime::now());
        fs::copy(&self.inner.layout.paths().database_file, &backup_path)?;
        Ok(backup_path)
    }

    fn connection(&self) -> StoreResult<MutexGuard<'_, Connection>> {
        self.inner
            .connection
            .lock()
            .map_err(|_| StoreError::LockPoisoned)
    }
}

fn read_schema_version(conn: &Connection) -> StoreResult<u32> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(1) FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
        [],
        |row| row.get(0),
    )?;

    if exists == 0 {
        return Ok(0);
    }

    let version = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    Ok(version)
}

fn file_has_contents(path: &PathBuf) -> StoreResult<bool> {
    Ok(fs::metadata(path).map(|metadata| metadata.len() > 0)?)
}

fn parse_uuid(value: String) -> rusqlite::Result<uuid::Uuid> {
    uuid::Uuid::parse_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn parse_datetime(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use goldclaw_core::{EnvelopeSource, MessageRole};
    use std::{env, time::UNIX_EPOCH};

    fn temp_database_file() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock went backwards")
            .as_nanos();
        env::temp_dir().join(format!("goldclaw-store-{unique}.sqlite3"))
    }

    fn temp_layout() -> StoreLayout {
        let database_file = temp_database_file();
        let backup_dir = database_file
            .parent()
            .expect("temp dir")
            .join("goldclaw-store-backups");

        StoreLayout::from_paths(database_file, backup_dir)
    }

    #[test]
    fn store_initializes_and_round_trips_runtime_state() {
        let layout = temp_layout();
        let store = SqliteStore::open(layout.clone()).expect("open store");
        assert_eq!(store.applied_schema_version().expect("schema version"), 3);

        let now = Utc::now();
        let session = SessionSummary {
            id: uuid::Uuid::new_v4(),
            title: "Feishu DM".into(),
            created_at: now,
            updated_at: now,
        };
        store.upsert_session(&session).expect("persist session");

        let binding = SessionBinding {
            session_id: session.id,
            source: EnvelopeSource::Connector("feishu".into()),
            source_instance: "bot-main".into(),
            conversation_id: "dm:user_123".into(),
            sender_id: Some("user_123".into()),
            created_at: now,
            updated_at: now,
        };
        store
            .upsert_session_binding(&binding)
            .expect("persist binding");

        let message = SessionMessage {
            id: uuid::Uuid::new_v4(),
            session_id: session.id,
            role: MessageRole::User,
            source: EnvelopeSource::Connector("feishu".into()),
            content: "hello".into(),
            metadata: serde_json::json!({ "external_message_id": "msg-1" }),
            created_at: now,
        };
        store.append_message(&message).expect("persist message");

        let snapshot = store.load_snapshot().expect("load snapshot");
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.bindings.len(), 1);
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.bindings[0].binding_key(), binding.binding_key());

        let _ = fs::remove_file(layout.paths().database_file.clone());
        let _ = fs::remove_dir_all(layout.paths().backup_dir.clone());
    }
}
