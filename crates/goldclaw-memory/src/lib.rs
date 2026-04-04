use std::{
    path::Path,
    sync::OnceLock,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use goldclaw_core::{GoldClawError, MemoryChunk, MemoryChunkId, MemoryQuery, MemoryStore, Result};
use rusqlite::{Connection, ffi, params};
use sqlite_vec::sqlite3_vec_init;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite-vec unavailable")]
    VectorUnavailable,
    #[error("store lock poisoned")]
    LockPoisoned,
}

impl From<MemoryError> for GoldClawError {
    fn from(e: MemoryError) -> Self {
        GoldClawError::Internal(e.to_string())
    }
}

type MemoryResult<T> = std::result::Result<T, MemoryError>;
const MEMORY_VEC_DIMENSIONS: usize = 2048;

/// Standalone SQLite-backed memory store.
///
/// Opens its own connection to the database file so it can be used independently
/// of `SqliteStore`. The memory tables (`memory_chunks`, `memory_fts`) must
/// already exist — `SqliteStore::open()` applies the required migration.
#[derive(Clone)]
pub struct SqliteMemoryStore {
    inner: Arc<Mutex<Connection>>,
    vector_search_available: bool,
}

impl SqliteMemoryStore {
    pub fn open(database_path: &Path) -> MemoryResult<Self> {
        register_sqlite_vec();
        let conn = Connection::open(database_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let vector_search_available = detect_sqlite_vec(&conn);
        if vector_search_available {
            ensure_vector_schema(&conn)?;
        }
        info!(
            db_path = %database_path.display(),
            vector_search_available,
            "memory store opened"
        );
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
            vector_search_available,
        })
    }

    fn connection(&self) -> MemoryResult<MutexGuard<'_, Connection>> {
        self.inner.lock().map_err(|_| MemoryError::LockPoisoned)
    }

    fn save_chunk_sync(&self, chunk: &MemoryChunk) -> MemoryResult<()> {
        let embedding_blob = chunk.embedding.as_deref().map(vec_to_blob);
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            r#"
INSERT INTO memory_chunks (id, session_id, content, embedding, created_at, metadata_json)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(id) DO NOTHING
"#,
            params![
                chunk.id.to_string(),
                chunk.session_id.map(|id| id.to_string()),
                &chunk.content,
                embedding_blob,
                chunk.created_at.to_rfc3339(),
                serde_json::to_string(&chunk.metadata)?,
            ],
        )?;
        if inserted == 0 {
            return Ok(());
        }

        tx.execute(
            "INSERT INTO memory_fts (content, chunk_id) VALUES (?1, ?2)",
            params![&chunk.content, chunk.id.to_string()],
        )?;
        if self.vector_search_available {
            if let Some(embedding_blob) = embedding_blob {
                tx.execute(
                    "INSERT INTO memory_vec_chunks(id, embedding) VALUES (?1, ?2)",
                    params![chunk.id.to_string(), embedding_blob],
                )?;
            }
        }
        tx.commit()?;
        info!(
            chunk_id = %chunk.id,
            session_id = ?chunk.session_id,
            has_embedding = chunk.embedding.is_some(),
            content_chars = chunk.content.chars().count(),
            "memory chunk saved"
        );
        Ok(())
    }

    fn recall_fts_sync(&self, text: &str, limit: usize) -> MemoryResult<Vec<MemoryChunk>> {
        let conn = self.connection()?;
        let fts_query = sanitize_fts_query(text);
        let mut stmt = conn.prepare(
            r#"
SELECT mc.id, mc.session_id, mc.content, mc.embedding, mc.created_at, mc.metadata_json
FROM memory_fts
JOIN memory_chunks mc ON memory_fts.chunk_id = mc.id
WHERE memory_fts MATCH ?1
ORDER BY rank
LIMIT ?2
"#,
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            parse_memory_chunk_row(row)
        })?;
        let chunks = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(MemoryError::from)?;
        info!(
            query_chars = text.chars().count(),
            limit,
            hits = chunks.len(),
            "memory recall via FTS5"
        );
        Ok(chunks)
    }

    fn recall_vector_sync(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> MemoryResult<Vec<MemoryChunk>> {
        if !self.vector_search_available {
            return Err(MemoryError::VectorUnavailable);
        }

        let query_blob = vec_to_blob(query_embedding);
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"
WITH knn_matches AS (
    SELECT id, distance
    FROM memory_vec_chunks
    WHERE embedding MATCH ?1
    ORDER BY distance
    LIMIT ?2
)
SELECT mc.id, mc.session_id, mc.content, mc.embedding, mc.created_at, mc.metadata_json
FROM knn_matches km
JOIN memory_chunks mc ON mc.id = km.id
ORDER BY km.distance
"#,
        )?;
        let rows = stmt.query_map(params![query_blob, limit as i64], parse_memory_chunk_row)?;
        let chunks = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(MemoryError::from)?;
        info!(
            dims = query_embedding.len(),
            limit,
            hits = chunks.len(),
            "memory recall via sqlite-vec"
        );
        Ok(chunks)
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    async fn save_chunk(&self, chunk: MemoryChunk) -> Result<()> {
        self.save_chunk_sync(&chunk).map_err(GoldClawError::from)
    }

    async fn recall(&self, query: MemoryQuery) -> Result<Vec<MemoryChunk>> {
        if let Some(embedding) = &query.embedding {
            match self.recall_vector_sync(embedding, query.limit) {
                Ok(chunks) => return Ok(chunks),
                Err(e) => warn!("vector recall failed, falling back to FTS5: {e}"),
            }
        }
        self.recall_fts_sync(&query.text, query.limit)
            .map_err(GoldClawError::from)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_memory_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryChunk> {
    let id: MemoryChunkId = Uuid::parse_str(&row.get::<_, String>(0)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let session_id = row
        .get::<_, Option<String>>(1)?
        .and_then(|s| Uuid::parse_str(&s).ok());
    let content: String = row.get(2)?;
    let embedding = row.get::<_, Option<Vec<u8>>>(3)?.map(|b| blob_to_vec(&b));
    let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&row.get::<_, String>(4)?)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let metadata: serde_json::Value =
        serde_json::from_str(&row.get::<_, String>(5)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(MemoryChunk {
        id,
        session_id,
        content,
        embedding,
        created_at,
        metadata,
    })
}

fn register_sqlite_vec() {
    static SQLITE_VEC_REGISTRATION: OnceLock<()> = OnceLock::new();

    SQLITE_VEC_REGISTRATION.get_or_init(|| {
        let rc = unsafe {
            ffi::sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())))
        };

        if rc != ffi::SQLITE_OK {
            warn!(
                rc,
                "failed to register sqlite-vec auto-extension; vector recall will fall back to FTS5"
            );
        }
    });
}

fn detect_sqlite_vec(conn: &Connection) -> bool {
    match conn.query_row("SELECT vec_version()", [], |row| row.get::<_, String>(0)) {
        Ok(version) => {
            info!(version = %version, "sqlite-vec ready");
            true
        }
        Err(error) => {
            warn!("sqlite-vec unavailable, falling back to FTS5: {error}");
            false
        }
    }
}

fn ensure_vector_schema(conn: &Connection) -> MemoryResult<()> {
    conn.execute_batch(&format!(
        r#"
CREATE VIRTUAL TABLE IF NOT EXISTS memory_vec_chunks USING vec0(
    id text primary key,
    embedding float[{MEMORY_VEC_DIMENSIONS}] distance_metric=cosine
);

INSERT INTO memory_vec_chunks(id, embedding)
SELECT mc.id, mc.embedding
FROM memory_chunks mc
WHERE mc.embedding IS NOT NULL
  AND NOT EXISTS (
      SELECT 1
      FROM memory_vec_chunks mv
      WHERE mv.id = mc.id
  );
"#
    ))?;
    info!("memory vec0 schema ready");
    Ok(())
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn sanitize_fts_query(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return String::new();
    }
    words
        .iter()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests;
