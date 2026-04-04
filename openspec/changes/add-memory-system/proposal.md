## Why

GoldClaw 目前只有会话内短期记忆（messages 表），每次对话结束后上下文完全丢失，无法记住用户偏好、过往事件或助手性格设定。作为个人 AI 助手，记忆能力是让用户感受到"它认识我"的核心体验。

## What Changes

- 新增 `soul.md` 文件，存储助手角色性格与用户偏好，作为每次会话的 system message
- 新增情景记忆（Episodic Memory）：每轮对话结束后将 user+assistant 内容向量化存入 SQLite
- 每条用户消息发出前，用消息内容检索 Top-K 相关历史记忆，拼入 user message 前缀注入 Provider
- 新增 `EmbeddingProvider` trait，GLM provider 实现该 trait 接入 GLM embedding API
- 检索支持双轨：有 embedding 配置时用 sqlite-vec 向量搜索，无配置时降级为 FTS5 关键词搜索
- 记忆全局共享，不做来源隔离

## Capabilities

### New Capabilities

- `soul-file`: 管理 `~/.goldclaw/soul.md` 的生成、读取，并在会话创建时注入为 system message
- `episodic-memory`: 情景记忆的写入（每轮对话后）与检索（每条用户消息前），注入 user message

### Modified Capabilities

## Impact

- `goldclaw-core`: 新增 `EmbeddingProvider` trait 和 `MemoryChunk` 模型
- `goldclaw-config`: `ProjectPaths` 新增 `soul_path()`，`init` 生成 soul.md 模板
- `goldclaw-store`: 新增 migration（`memory_chunks` 表、`memory_fts` FTS5 虚表），实现 `MemoryStore` trait
- `goldclaw-runtime`: `InMemoryRuntime` 集成 soul 注入 + 记忆检索 + 记忆写入
- `goldclaw-provider-glm`: 实现 `EmbeddingProvider`，接入 GLM embedding API
- 依赖新增：`sqlite-vec`（可选，向量路径）
