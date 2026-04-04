## ADDED Requirements

### Requirement: MemoryChunk model
`goldclaw-core` SHALL define a `MemoryChunk` struct with fields: `id: String`, `session_id: Option<SessionId>`, `content: String`, `embedding: Option<Vec<f32>>`, `created_at: DateTime<Utc>`, `metadata: serde_json::Value`.

#### Scenario: MemoryChunk serialization
- **WHEN** a `MemoryChunk` is serialized to JSON
- **THEN** all fields are present and `embedding` is omitted when `None`

---

### Requirement: EmbeddingProvider trait
`goldclaw-core` SHALL define an `EmbeddingProvider` trait with methods `embed(&self, text: &str) -> Result<Vec<f32>, GoldClawError>`, `dimension(&self) -> usize`, and `model_name(&self) -> &str`.

#### Scenario: Embed returns vector of correct dimension
- **WHEN** `embed()` is called with a non-empty string
- **THEN** the returned `Vec<f32>` has length equal to `dimension()`

---

### Requirement: SQLite schema for memory chunks
`goldclaw-store` SHALL add migration version 4 creating:
- Table `memory_chunks` with columns `id`, `session_id`, `content`, `embedding` (BLOB, nullable), `created_at`, `metadata_json`
- Index `idx_memory_chunks_created_at` on `(created_at DESC)`
- FTS5 virtual table `memory_fts` with columns `content` and unindexed `chunk_id`

#### Scenario: Migration applied cleanly
- **WHEN** migration 4 is applied to a database at schema version 3
- **THEN** `memory_chunks` and `memory_fts` tables exist and `schema_migrations` records version 4

---

### Requirement: MemoryStore trait
`goldclaw-store` SHALL define a `MemoryStore` trait with:
- `save_chunk(&self, chunk: MemoryChunk) -> Result<(), StoreError>`
- `recall(&self, query: MemoryQuery) -> Result<Vec<MemoryChunk>, StoreError>`

where `MemoryQuery` contains `text: String`, `embedding: Option<Vec<f32>>`, `limit: usize`.

#### Scenario: Save and recall roundtrip (FTS5)
- **WHEN** a chunk is saved and `recall()` is called with matching keywords and no embedding
- **THEN** the saved chunk appears in results

#### Scenario: Save and recall roundtrip (vector)
- **WHEN** a chunk with embedding is saved and `recall()` is called with a similar embedding vector
- **THEN** the saved chunk appears in results ordered by cosine similarity

---

### Requirement: FTS5 fallback when no embedding provided
`SqliteMemoryStore::recall()` SHALL use FTS5 BM25 keyword search when `query.embedding` is `None`.

#### Scenario: FTS5 used on text-only query
- **WHEN** `recall()` is called with `embedding: None`
- **THEN** results are returned using FTS5 keyword matching on `content`

---

### Requirement: sqlite-vec vector search when embedding provided
`SqliteMemoryStore::recall()` SHALL use sqlite-vec cosine similarity search when `query.embedding` is `Some(_)` and the extension is loaded.

#### Scenario: Vector search used when embedding available
- **WHEN** `recall()` is called with `embedding: Some(vec)`
- **THEN** results are ordered by cosine similarity to `vec`

#### Scenario: Fallback to FTS5 when sqlite-vec unavailable
- **WHEN** sqlite-vec extension fails to load at startup
- **THEN** `recall()` falls back to FTS5 and a warning is logged

---

### Requirement: GLM EmbeddingProvider implementation
`goldclaw-provider-glm` SHALL implement `EmbeddingProvider` for `GlmProvider` by calling the GLM embedding API endpoint with the input text and returning the resulting float vector.

#### Scenario: Successful embedding call
- **WHEN** `GlmProvider::embed()` is called with valid input and GLM API is reachable
- **THEN** a `Vec<f32>` of the configured dimension is returned

#### Scenario: API error propagates
- **WHEN** the GLM embedding API returns an error
- **THEN** `embed()` returns `Err(GoldClawError)` with a descriptive message

---

### Requirement: Memory written after each completed turn
After `InMemoryRuntime` receives a completed assistant response for a user message, it SHALL create a `MemoryChunk` with `content = "User: {user_msg}\nAssistant: {assistant_reply}"`, optionally embed it via `EmbeddingProvider`, and save it via `MemoryStore`.

#### Scenario: Memory saved after assistant reply
- **WHEN** a user message is processed and the assistant reply is complete
- **THEN** a new `memory_chunks` row exists containing both the user message and assistant reply

#### Scenario: Memory saved without embedding when EmbeddingProvider not configured
- **WHEN** no `EmbeddingProvider` is registered and a turn completes
- **THEN** the chunk is saved with `embedding = NULL`

---

### Requirement: Memory recalled and injected before each user message
Before invoking the `Provider`, `InMemoryRuntime` SHALL call `MemoryStore::recall()` using the current user message text (and its embedding if available), retrieve up to 5 chunks, and prepend them to the user message content in the format:

```
[Memory]
- {chunk1.content}
- {chunk2.content}
...

[User Input]
{original user message}
```

#### Scenario: Relevant memories injected into user message
- **WHEN** `recall()` returns one or more chunks for the current user input
- **THEN** the message sent to the Provider has the memory block prepended before the original content

#### Scenario: No injection when no memories found
- **WHEN** `recall()` returns an empty list
- **THEN** the message sent to the Provider contains only the original user content, with no `[Memory]` block

#### Scenario: No injection when MemoryStore not configured
- **WHEN** no `MemoryStore` is registered in the runtime
- **THEN** the message is sent to the Provider unchanged
