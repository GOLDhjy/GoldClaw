## 1. Core Models and Traits

- [x] 1.1 在 `goldclaw-core/src/models.rs` 添加 `MemoryChunk` 结构体（id, session_id, content, embedding, created_at, metadata）
- [x] 1.2 在 `goldclaw-core/src/lib.rs` 定义 `EmbeddingProvider` trait（embed, dimension, model_name）
- [x] 1.3 为 `MemoryChunk` 和 `EmbeddingProvider` 补充单元测试到 `goldclaw-core/src/tests.rs`

## 2. Soul File Support

- [x] 2.1 在 `goldclaw-config/src/lib.rs` 的 `ProjectPaths` 上添加 `soul_path()` 方法
- [x] 2.2 在 `goldclaw/src/main.rs` 的 `init` 命令中，若 `soul.md` 不存在则生成模板文件
- [x] 2.3 为 `soul_path()` 和模板生成逻辑添加测试

## 3. SQLite Migration and MemoryStore

- [x] 3.1 在 `goldclaw-store/src/migrations.rs` 添加 migration 4：创建 `memory_chunks` 表、`idx_memory_chunks_created_at` 索引和 `memory_fts` FTS5 虚表
- [x] 3.2 在 `goldclaw-store` 中定义 `MemoryStore` trait 和 `MemoryQuery` 结构体
- [x] 3.3 实现 `SqliteMemoryStore::save_chunk()`，同时写入 `memory_chunks` 和 `memory_fts`
- [x] 3.4 实现 `SqliteMemoryStore::recall()` FTS5 路径（`embedding: None` 时使用）
- [x] 3.5 集成 sqlite-vec 扩展加载（启动时尝试加载，失败则 warning 降级）
- [x] 3.6 实现 `SqliteMemoryStore::recall()` sqlite-vec 路径（`embedding: Some(_)` 时使用）
- [x] 3.7 在 `goldclaw-store/src/tests.rs` 添加 migration、save/recall 的单元测试

## 4. GLM EmbeddingProvider

- [x] 4.1 确认 GLM embedding API 端点、请求格式和向量维度
- [x] 4.2 在 `goldclaw-provider-glm/src/lib.rs` 为 `GlmProvider` 实现 `EmbeddingProvider` trait
- [x] 4.3 为 GLM embedding 实现添加单元测试（mock HTTP）

## 5. Runtime Integration

- [x] 5.1 在 `InMemoryRuntime` 中新增 `soul_path: PathBuf`、`Option<Arc<dyn EmbeddingProvider>>`、`Option<Arc<dyn MemoryStore>>` 字段
- [x] 5.2 在 session 创建时读取 `soul.md`，若非空则将内容存为 `role: system` 首条消息
- [x] 5.3 在 `handle_envelope` 中，用户消息到达后先调用 `recall()`（embed if possible），将结果拼入 user message 前缀
- [x] 5.4 在助手回复完成后，将 `"User: ...\nAssistant: ..."` 作为 chunk 保存（embed if possible）
- [x] 5.5 在 `goldclaw-runtime/src/tests.rs` 添加：soul 注入、记忆写入、记忆注入的集成测试
