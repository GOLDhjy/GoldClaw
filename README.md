# GoldClaw

GoldClaw 是一个本地优先的 AI assistant daemon。它把模型调用、会话绑定、SQLite 持久化、受限工具调用，以及 Web / TUI / HTTP 接入拆成独立 crate，方便先做出可运行闭环，再逐步替换 provider、policy、tool 和 connector。

当前仓库已经具备一套可本地运行的最小产品：`goldclaw` CLI 负责初始化和守护进程管理，`goldclaw-gateway` 提供 REST + SSE 接口，`goldclaw-web` 和 `goldclaw-tui` 作为两个本地前端，底层会话与记忆由 SQLite 保存。

## 当前能力

- 本地 daemon，默认只绑定 loopback 地址，避免直接暴露到公网。
- Axum gateway，提供 `/healthz`、`/status`、`/sessions`、`/messages`、`/sessions/{id}/events`。
- SQLite 持久化会话、消息、会话绑定和 memory chunks。
- 会话绑定机制：同一个 `ConversationRef` 会稳定复用同一个内部 session。
- 内置 `read_file` 和 `update_soul` 两个工具。
- `read_file` 只能读取配置的 `read_roots` 内文件，单文件上限 64 KiB。
- `update_soul` 使用完整全文写入，用于持久化修改人格、语气和长期对话规则。
- 运行时支持记忆召回：默认走 FTS5；如果 GLM embedding 可用，则额外启用 `sqlite-vec` 向量检索。
- GLM provider 已接入；若未配置 BigModel API key，会自动回退到 `EchoProvider`。
- `doctor` 命令可检查配置、目录、数据库 schema、origin 限制、read roots 和 gateway 存活状态。

## 快速开始

### 1. 环境准备

- 安装较新的 stable Rust toolchain，需支持 Rust 2024 edition。
- 推荐先执行一次完整构建，确保 `goldclaw`、`goldclaw-web`、`goldclaw-tui` 都会被编译出来。

```bash
cargo build
```

### 2. 初始化本地配置

```bash
cargo run --bin goldclaw -- init
```

初始化会创建 `~/.goldclaw/`，其中通常包括：

- `config.toml`：主配置
- `soul.md`：唯一人格源；`init` 会通过问答收集用户习惯与期待后自动生成
- `goldclaw.sqlite3`：SQLite 数据库
- `gateway-state.json`：后台进程状态
- `logs/`、`tmp/`、`backups/`

### 3. 启动服务

```bash
cargo run --bin goldclaw -- start
```

默认端口：

- Gateway: `127.0.0.1:4263`
- Web UI: `127.0.0.1:4264`

如果 `target/debug/goldclaw-web` 已存在，`goldclaw start` 会顺带拉起 Web UI。最稳妥的做法是先执行一次 `cargo build`。

### 4. 使用方式

浏览器：

```text
http://127.0.0.1:4264
```

TUI：

```bash
cargo run --bin goldclaw-tui
```

状态和健康检查：

```bash
cargo run --bin goldclaw -- status
cargo run --bin goldclaw -- doctor
cargo run --bin goldclaw -- doctor --json
```

停止服务：

```bash
cargo run --bin goldclaw -- stop
```

实验性微信 connector：

```bash
# 首次扫码登录，保存 bot token / account id 到 ~/.goldclaw/connectors/weixin/
cargo run --bin goldclaw -- connector weixin login

# 启动微信 connector
cargo run --bin goldclaw -- connector weixin run
```

当前这个微信接入是 spike skeleton：

- 走腾讯 OpenClaw 微信插件同源的二维码登录 + `getupdates` / `sendmessage` 协议
- 当前只处理直聊文本和语音转文字内容
- 收到用户消息后，会把 assistant 的最终文本回复回发到微信
- 还没有做多账号、媒体回发、命令式 connector 管理、doctor 检查和统一 gateway 托管

## 配置说明

默认配置文件位于 `~/.goldclaw/config.toml`。一个最小示例：

```toml
version = 1
profile = "default"

[agent]
name = "GoldClaw"

[gateway]
bind = "127.0.0.1:4263"
allowed_origins = ["http://127.0.0.1", "http://localhost"]

[runtime]
read_roots = ["/absolute/path/to/workspace"]

[provider]
api_key = "YOUR_BIGMODEL_API_KEY"
model = "GLM-5.1"
```

说明：

- `gateway.bind` 必须是 loopback 地址。
- `allowed_origins` 只允许本地来源。
- `runtime.read_roots` 中的目录会限制 `read_file` 工具的读取范围。
- 如果 `runtime.read_roots` 为空，gateway 会回退到启动时的当前工作目录作为读取根目录。
- `~/.goldclaw/soul.md` 是运行时唯一使用的人格 / 风格来源，并且每次模型调用都会重新读取。
- `goldclaw init` 不会让用户直接编辑 `soul.md`；它会先询问称呼、习惯与偏好、对助手的期待，以及默认回复语言，再把这些信息整理进 `soul.md`。
- 如果没有可用的 BigModel API key，运行时仍能启动，但回复会退化为 `echo: <用户输入>`。

### 常用环境变量

GoldClaw 支持通过环境变量覆盖本地配置：

- `GOLDCLAW_PROFILE`
- `GOLDCLAW_GATEWAY_BIND`
- `GOLDCLAW_ALLOWED_ORIGINS`
- `GOLDCLAW_READ_ROOTS`
- `GOLDCLAW_WEB_BIND`

GLM / HTTP 相关环境变量：

- `BIGMODEL_API_KEY`
- `BIGMODEL_MODEL`
- `BIGMODEL_CODING_MODEL`
- `BIGMODEL_BASE_URL`
- `BIGMODEL_CODING_BASE_URL`
- `BIGMODEL_EMBEDDING_BASE_URL`
- `HTTP_PROXY`
- `API_TIMEOUT_MS`

## HTTP API

Gateway 默认地址为 `http://127.0.0.1:4263`。

创建 session：

```bash
curl -s http://127.0.0.1:4263/sessions \
  -H 'content-type: application/json' \
  -d '{"title":"demo"}'
```

发送消息：

```bash
curl -s http://127.0.0.1:4263/messages \
  -H 'content-type: application/json' \
  -d '{
    "session_id": "REPLACE_WITH_SESSION_ID",
    "content": "你好，介绍一下你自己",
    "source": "web"
  }'
```

订阅 SSE 事件流：

```bash
curl -N http://127.0.0.1:4263/sessions/REPLACE_WITH_SESSION_ID/events
```

常见事件名：

- `session_created`
- `message_accepted`
- `tool_started`
- `tool_completed`
- `message_chunk`
- `message_completed`
- `error`

如果你希望把外部线程或会话稳定映射到同一个 GoldClaw session，可以在 `POST /messages` 时带上 `conversation`：

```json
{
  "content": "继续刚才的话题",
  "source": "web",
  "conversation": {
    "source_instance": "local-demo",
    "conversation_id": "thread-42",
    "sender_id": "user-1"
  }
}
```

## 内置工具与记忆

### `read_file`

用户消息以 `read path/to/file` 或 `/read path/to/file` 开头时，运行时会直接触发 `read_file`，而不是走模型推理。

限制：

- 只能读取 `runtime.read_roots` 之下的文件
- 若未配置 `runtime.read_roots`，则读取范围退化为 gateway 进程启动目录
- 默认最大文件大小 64 KiB
- 越界路径会被拒绝

### `update_soul`

运行时会在 system prompt 里明确告诉模型：

- `soul.md` 是当前人格和长期对话风格的唯一来源
- 如果用户要求修改人设、语气、风格、格式习惯或其它长期生效规则，必须调用 `update_soul`

`update_soul` 的参数是完整的新 `soul.md` 全文，工具会把这份全文直接写入文件，并把写入后的全文原样返回。运行时每轮都会把完整当前 soul 注入 `[Active Soul]`，所以模型知道自己在改什么。工具执行后，运行时会立刻重新读取 `soul.md`，因此同一轮后续模型调用就会使用更新后的人设。

### Memory

每轮对话完成后，运行时会把下面这段内容保存为 memory chunk：

```text
User: <用户输入>
Assistant: <助手回复>
```

召回逻辑：

- 默认使用 SQLite FTS5 做全文召回
- 若 GLM embedding 可用，则优先使用 `sqlite-vec` 做向量检索
- 召回内容会以 `[Memory]` block 的形式注入到当前用户消息前

## 工作区结构

### Libraries

- `crates/goldclaw-core`：核心 trait 和共享模型
- `crates/goldclaw-config`：TOML 配置、路径发现、环境变量覆盖
- `crates/goldclaw-store`：SQLite 持久化与 schema migration
- `crates/goldclaw-memory`：memory chunks、FTS5、`sqlite-vec`
- `crates/goldclaw-runtime`：会话生命周期、消息路由、事件广播、工具执行
- `crates/goldclaw-gateway`：本地 HTTP / SSE 网关
- `crates/goldclaw-doctor`：健康检查与诊断报告
- `crates/goldclaw-provider-glm`：BigModel / GLM provider + embedding provider
- `crates/goldclaw-connector-stdin`：stdin connector library
- `crates/goldclaw-connector-weixin`：实验性微信 connector（二维码登录、长轮询收消息、文本回发）

### Apps

- `apps/goldclaw`：CLI，负责 `init` / `doctor` / `start` / `stop` / `restart` / `status`
- `apps/goldclaw-tui`：终端 UI
- `apps/goldclaw-web`：本地 Web UI

## 开发命令

```bash
# Build
cargo build
cargo build --release

# Test
cargo test --all
cargo test -p goldclaw-runtime
cargo test -p goldclaw-runtime session_binding

# Lint / Format
cargo clippy --all
cargo fmt

# CLI
cargo run --bin goldclaw -- --help
cargo run --bin goldclaw -- start
cargo run --bin goldclaw -- stop
cargo run --bin goldclaw -- status
```

## 当前边界

- 这是一个本地单机场景的工程化骨架，不是完整通用助手平台。
- 当前内置工具是 `read_file` 和 `update_soul`。
- Provider 目前聚焦 GLM；未配置 API key 时会回退到 echo stub。
- 安全策略目前是静态 allowlist，后续可替换为更细粒度 policy。

## License

MIT
