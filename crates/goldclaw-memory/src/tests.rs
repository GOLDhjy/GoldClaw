use std::{env, fs};

use chrono::Utc;
use goldclaw_core::{MemoryChunk, MemoryQuery, MemoryStore};
use uuid::Uuid;

use crate::SqliteMemoryStore;

fn sparse_embedding(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0_f32; 2048];
    embedding[index] = 1.0;
    embedding
}

fn temp_db() -> (std::path::PathBuf, SqliteMemoryStore) {
    let id = Uuid::new_v4();
    let path = env::temp_dir().join(format!("goldclaw-memory-{id}.sqlite3"));
    // Bootstrap the required tables (normally done by SqliteStore migrations).
    let conn = rusqlite::Connection::open(&path).expect("open");
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS memory_chunks (
    id            TEXT PRIMARY KEY,
    session_id    TEXT,
    content       TEXT NOT NULL,
    embedding     BLOB,
    created_at    TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_memory_chunks_created_at ON memory_chunks(created_at DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(content, chunk_id UNINDEXED);
"#,
    )
    .expect("bootstrap tables");
    drop(conn);
    let store = SqliteMemoryStore::open(&path).expect("open store");
    (path, store)
}

#[tokio::test]
async fn save_and_recall_fts5() {
    let (path, store) = temp_db();

    let chunk = MemoryChunk {
        id: Uuid::new_v4(),
        session_id: None,
        content: "User: 你好\nAssistant: 你好！有什么可以帮你的？".into(),
        embedding: None,
        created_at: Utc::now(),
        metadata: serde_json::json!({}),
    };

    store.save_chunk(chunk.clone()).await.expect("save");

    let results = store
        .recall(MemoryQuery { text: "你好".into(), embedding: None, limit: 5 })
        .await
        .expect("recall");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, chunk.id);

    let _ = fs::remove_file(path);
}

#[tokio::test]
async fn save_and_recall_vector() {
    let (path, store) = temp_db();

    let primary = MemoryChunk {
        id: Uuid::new_v4(),
        session_id: None,
        content: "User: alpha\nAssistant: primary".into(),
        embedding: Some(sparse_embedding(0)),
        created_at: Utc::now(),
        metadata: serde_json::json!({}),
    };
    let secondary = MemoryChunk {
        id: Uuid::new_v4(),
        session_id: None,
        content: "User: beta\nAssistant: secondary".into(),
        embedding: Some(sparse_embedding(1)),
        created_at: Utc::now(),
        metadata: serde_json::json!({}),
    };

    store.save_chunk(primary.clone()).await.expect("save primary");
    store.save_chunk(secondary.clone()).await.expect("save secondary");

    let results = store
        .recall(MemoryQuery {
            text: "no keyword overlap".into(),
            embedding: Some(sparse_embedding(0)),
            limit: 5,
        })
        .await
        .expect("recall");

    assert!(!results.is_empty());
    assert_eq!(results[0].id, primary.id);

    let _ = fs::remove_file(path);
}

#[tokio::test]
async fn open_backfills_vec_index_from_existing_chunks() {
    let id = Uuid::new_v4();
    let path = env::temp_dir().join(format!("goldclaw-memory-backfill-{id}.sqlite3"));
    let conn = rusqlite::Connection::open(&path).expect("open");
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS memory_chunks (
    id            TEXT PRIMARY KEY,
    session_id    TEXT,
    content       TEXT NOT NULL,
    embedding     BLOB,
    created_at    TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_memory_chunks_created_at ON memory_chunks(created_at DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(content, chunk_id UNINDEXED);
"#,
    )
    .expect("bootstrap tables");

    let chunk_id = Uuid::new_v4();
    let embedding_blob: Vec<u8> = sparse_embedding(0)
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    conn.execute(
        r#"
INSERT INTO memory_chunks (id, session_id, content, embedding, created_at, metadata_json)
VALUES (?1, NULL, ?2, ?3, ?4, '{}')
"#,
        rusqlite::params![
            chunk_id.to_string(),
            "User: migrated\nAssistant: existing row",
            embedding_blob,
            Utc::now().to_rfc3339(),
        ],
    )
    .expect("insert chunk");
    conn.execute(
        "INSERT INTO memory_fts (content, chunk_id) VALUES (?1, ?2)",
        rusqlite::params!["User: migrated\nAssistant: existing row", chunk_id.to_string()],
    )
    .expect("insert fts");
    drop(conn);

    let store = SqliteMemoryStore::open(&path).expect("open store");
    let results = store
        .recall(MemoryQuery {
            text: "no keyword overlap".into(),
            embedding: Some(sparse_embedding(0)),
            limit: 5,
        })
        .await
        .expect("recall");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, chunk_id);

    let _ = fs::remove_file(path);
}

#[tokio::test]
async fn recall_empty_returns_empty() {
    let (path, store) = temp_db();

    let results = store
        .recall(MemoryQuery { text: "nothing".into(), embedding: None, limit: 5 })
        .await
        .expect("recall");

    assert!(results.is_empty());

    let _ = fs::remove_file(path);
}

#[test]
fn sqlite_vec_extension_is_available_on_open_connection() {
    let (path, store) = temp_db();
    assert!(
        store.vector_search_available,
        "sqlite-vec should be enabled for vector search"
    );

    let conn = store.connection().expect("connection");
    let version: String = conn
        .query_row("SELECT vec_version()", [], |row| row.get(0))
        .expect("vec_version");
    assert!(!version.trim().is_empty());

    drop(conn);
    let _ = fs::remove_file(path);
}
