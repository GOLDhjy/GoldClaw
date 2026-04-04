use std::{
    env, fs,
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

use chrono::Utc;
use goldclaw_core::{EnvelopeSource, MessageRole, SessionBinding, SessionMessage, SessionSummary};

use crate::{MIGRATIONS, SqliteStore, StoreLayout, current_schema_version};

mod layout {
    use super::*;

    #[test]
    fn backup_path_is_stable() {
        let layout = StoreLayout::from_paths(
            PathBuf::from("db/goldclaw.sqlite3"),
            PathBuf::from("backups"),
        );

        let backup = layout.backup_path(UNIX_EPOCH + Duration::from_secs(42));
        assert_eq!(backup, PathBuf::from("backups/goldclaw-42.sqlite3.bak"));
    }
}

mod migrations {
    use super::*;

    #[test]
    fn schema_version_tracks_latest_migration() {
        assert_eq!(current_schema_version(), 4);
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

fn temp_layout() -> StoreLayout {
    let id = uuid::Uuid::new_v4();
    let database_file = env::temp_dir().join(format!("goldclaw-store-{id}.sqlite3"));
    let backup_dir = env::temp_dir().join(format!("goldclaw-store-backups-{id}"));
    StoreLayout::from_paths(database_file, backup_dir)
}

mod sqlite {
    use super::*;

    #[test]
    fn store_initializes_and_round_trips_runtime_state() {
        let layout = temp_layout();
        let store = SqliteStore::open(layout.clone()).expect("open store");
        assert_eq!(
            store.applied_schema_version().expect("schema version"),
            current_schema_version()
        );

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
