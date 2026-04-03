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
