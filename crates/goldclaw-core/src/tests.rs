use chrono::Utc;
use uuid::Uuid;

use crate::{MemoryChunk, MemoryChunkId};

#[test]
fn memory_chunk_serializes_without_embedding_when_none() {
    let chunk = MemoryChunk {
        id: Uuid::nil(),
        session_id: None,
        content: "User: hi\nAssistant: hello".into(),
        embedding: None,
        created_at: Utc::now(),
        metadata: serde_json::json!({}),
    };

    let json = serde_json::to_value(&chunk).expect("serialize");
    assert!(
        json.get("embedding").is_none(),
        "embedding should be absent when None"
    );
    assert_eq!(json["content"], "User: hi\nAssistant: hello");
}

#[test]
fn memory_chunk_serializes_embedding_when_present() {
    let chunk = MemoryChunk {
        id: Uuid::nil(),
        session_id: None,
        content: "test".into(),
        embedding: Some(vec![0.1, 0.2, 0.3]),
        created_at: Utc::now(),
        metadata: serde_json::json!({}),
    };

    let json = serde_json::to_value(&chunk).expect("serialize");
    let emb = json["embedding"].as_array().expect("embedding array");
    assert_eq!(emb.len(), 3);
}

#[test]
fn memory_chunk_id_is_uuid() {
    let id: MemoryChunkId = Uuid::new_v4();
    assert!(!id.is_nil());
}
