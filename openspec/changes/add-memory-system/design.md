## Context

GoldClaw 当前的对话流程：用户发消息 → InMemoryRuntime 加载当前会话历史 → 调用 Provider → 返回回复。会话历史仅限当前 session，跨会话信息完全不保留。

助手没有任何跨会话记忆，也没有稳定的角色性格。需要在不破坏现有架构的前提下，在 session 创建时注入 soul，在每轮消息前注入相关历史记忆。

## Goals / Non-Goals

**Goals:**

- Soul file 作为 system message 注入，在 session 创建时读取一次
- 每轮对话结束后，将 `User: ...\nAssistant: ...` 存为一条 memory chunk
- 每条用户消息前，检索 Top-K 相关 chunk，拼入 user message 前缀
- 支持 FTS5（默认）和 sqlite-vec（有 embedding 配置时）双轨检索
- GLM provider 实现 `EmbeddingProvider` trait

**Non-Goals:**

- 不做 per-source 记忆隔离，记忆全局共享
- 不做记忆的 Web/TUI 管理界面（v1）
- 不支持非 GLM 的 embedding provider（v1）
- 不做记忆衰减、遗忘、去重（v1）

## Decisions

### 1. Soul 作为 system message，session 创建时读取一次

**Decision:** `InMemoryRuntime` 在创建新 session 时读取 `soul.md`，将其内容作为 `role: system` 的首条消息存入 session 的消息历史。后续对话复用该消息，不重复读取文件。

**Rationale:** system message 天然是 Provider 协议里的角色设定位置，符合所有主流模型 API 的约定。读一次存入 session 历史，保证了同一 session 内的一致性，也避免每轮都做文件 IO。

**Alternatives considered:**
- 每轮都读文件并注入：文件 IO 开销低，但语义上 system message 应该是会话级别的，不是消息级别的。

---

### 2. 情景记忆注入位置：user message 前缀

**Decision:** 检索到的相关记忆拼入当前 user message 的前缀，格式如下：

```
[Memory]
- User: 上次问题\n  Assistant: 上次回答
- ...

[User Input]
原始用户消息
```

**Rationale:** 将记忆和用户输入合并在同一条 user message 里，避免在消息历史中插入额外的 role，保持消息结构干净。放在 user message 里也符合"用户带着上下文来提问"的语义。

**Alternatives considered:**
- 注入为独立 system message：污染消息历史，每轮都有新的 system message 不符合协议语义。
- 注入为 assistant message：语义错误，记忆不是 assistant 说的话。

---

### 3. 双轨检索：FTS5 降级 + sqlite-vec 向量搜索

**Decision:** `SqliteMemoryStore` 在 `recall()` 时判断查询是否携带 embedding 向量：有向量则用 sqlite-vec 做余弦相似度检索，无向量则用 FTS5 BM25 关键词检索。两者共用同一个 `memory_chunks` 主表。

**Rationale:** sqlite-vec 是 SQLite 扩展，需要运行时加载 `.dylib`/.so，有一定部署成本。FTS5 内置于 SQLite，零依赖。双轨设计让系统在没有配置 embedding 时仍然可用，不强依赖外部模型。

**Alternatives considered:**
- 只做 FTS5：无法真正语义检索，"喜欢 Rust"搜不到"偏好这门系统语言"。
- 只做 sqlite-vec：强依赖 embedding model，配置未完成时记忆功能完全不可用。

---

### 4. EmbeddingProvider 作为独立 trait

**Decision:** 在 `goldclaw-core` 中定义 `EmbeddingProvider` trait，与 `Provider`（文本生成）分离。`GlmProvider` 同时实现两个 trait。`InMemoryRuntime` 持有 `Option<Arc<dyn EmbeddingProvider>>`。

**Rationale:** 文本生成和向量化是两个不同的能力，未来可以用不同的模型分别承担（如本地模型做 embedding，云端模型做生成）。分离 trait 使依赖关系更清晰。

**Alternatives considered:**
- 在 `Provider` trait 上加 `embed()` 方法：破坏现有 Provider 实现，且语义混杂。

---

### 5. memory_chunks 存储格式

**Decision:** `content` 字段存储格式化的对话文本 `"User: {msg}\nAssistant: {reply}"`，`embedding` 字段存储 f32 little-endian 字节序的向量 BLOB，NULL 表示未向量化。

**Rationale:** 文本格式便于 FTS5 索引和人工查阅。向量存 BLOB 不依赖 sqlite-vec 就能存储，只在检索时才需要扩展。

## Risks / Trade-offs

- [sqlite-vec 扩展加载失败] → 运行时检测加载结果，失败时自动降级为 FTS5，记录 warning 日志
- [GLM embedding API 不可用] → `EmbeddingProvider::embed()` 返回 Error，runtime 降级为无向量 recall（FTS5）
- [soul.md 不存在] → session 创建时文件不存在则不注入 system message，不报错
- [记忆量增大导致检索变慢] → FTS5 和 sqlite-vec 均有索引，Top-K 限制为 5，初期不做分页
- [注入记忆导致 context 超长] → v1 不做 token 计数截断，依赖 Top-K=5 的自然限制；v2 加 token budget 控制

## Migration Plan

1. 新增 migration 4，添加 `memory_chunks` 表和 `memory_fts` FTS5 虚表
2. `soul_path()` 加入 `ProjectPaths`，`init` 命令生成 soul.md 模板（不影响已有用户）
3. `EmbeddingProvider` trait 和 `MemoryChunk` 模型加入 `goldclaw-core`（无破坏性变更）
4. `SqliteMemoryStore` 实现加入 `goldclaw-store`（独立模块，不改现有代码）
5. `GlmProvider` 实现 `EmbeddingProvider`（新增 impl，不改现有方法）
6. `InMemoryRuntime` 集成：新增可选字段 + 修改 `handle_envelope` 和 session 创建逻辑

回滚：migration 可向前兼容（只加表不改表），runtime 的记忆逻辑均有 `Option` 保护，关闭 embedding 配置即可停用 Layer 3。

## Open Questions

- sqlite-vec 的 Rust 绑定用哪个 crate？（`sqlite-vec` 官方 crate 还是通过 `rusqlite` 加载扩展）
- GLM embedding API 的端点和维度是多少？需要确认后才能定 migration 里的向量维度
- FTS5 是否需要中文分词插件（jieba）？v1 先不加，用空格分词观察效果
