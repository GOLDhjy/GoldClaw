## Context

目标产品是一个面向个人使用的 Rust AI 助手，既要支持本地命令行/TUI/Web 的一致体验，也要支持飞书、企业微信、微信等外部渠道接入。该系统需要具备三个明显区别于普通命令行 agent 的能力：一是长期驻留并在后台服务多个入口；二是首次启动即可完成清晰引导，而不是依赖手动拼装环境变量；三是当模型、凭据、端口、Webhook 或渠道异常时，能够通过 `doctor` 快速定位问题。

系统默认假设为单用户、本机优先部署，但应保留后续扩展到远程访问和更多连接器的结构余量。v1 优先保证本地可靠性、可调试性和多界面一致性，而不是追求复杂的自治工作流或多租户能力。

## Goals / Non-Goals

**Goals:**

- 用 Rust workspace 建立清晰的模块边界，避免运行时、UI、渠道和运维能力混杂。
- 提供一个常驻的 GoldClaw gateway 服务作为唯一控制面和消息交换中枢。
- 让 CLI、TUI、Web 共享统一会话模型、事件流和配置来源。
- 提供可交互的 `init` 向导和可脚本化的 `doctor` 诊断结果。
- 提供飞书、企业微信、微信的统一连接器抽象和参考实现。
- 让系统在 Windows/macOS/Linux 上都能以本地优先的方式工作。

**Non-Goals:**

- v1 不做多租户 SaaS 控制台或团队级权限模型。
- v1 不做完整移动端 App。
- v1 不做插件市场或任意第三方动态加载系统。
- v1 不追求复杂的分布式集群部署。
- v1 不承诺支持所有微信生态形态，先以受支持、可合规接入的接口形态为主。

## Decisions

### 1. 采用 gateway-first 架构，由 GoldClaw gateway 服务作为唯一长生命周期进程

**Decision:** 运行时能力不直接嵌入每个前端，而是由 GoldClaw gateway 服务统一承载。CLI/TUI/Web/渠道连接器都通过本地控制面访问守护进程。

**Rationale:** 这样可以把会话状态、模型连接、任务队列、连接器状态、日志和诊断集中管理，避免每个界面各自维护一份运行时。后台服务也更容易做自动重连、消息排队和健康检查。

**Alternatives considered:**

- 每个客户端内嵌运行时：实现简单，但会导致状态割裂、渠道复用困难、后台运行能力弱。
- 完全远程化服务：便于统一部署，但与“个人助手、本地优先、离线可控”的目标不符。

### 2. 使用 Rust workspace 拆分核心 crate 与应用入口

**Decision:** 推荐如下工作区布局：

- `crates/goldclaw-core`: 领域模型、事件、错误、trait 定义
- `crates/goldclaw-config`: 配置加载、profile、schema、密钥引用
- `crates/goldclaw-store`: SQLite 持久化、迁移、仓储接口
- `crates/goldclaw-runtime`: 会话编排、模型路由、工具执行、记忆接口
- `crates/goldclaw-gateway`: HTTP/WebSocket、IPC、订阅和生命周期管理
- `crates/goldclaw-doctor`: 检查项框架、修复建议、诊断输出
- `crates/channel-feishu`
- `crates/channel-wecom`
- `crates/channel-wechat`
- `apps/goldclaw`
- `apps/goldclaw-tui`
- `apps/goldclaw-web`

**Rationale:** 这种拆分可以把协议、状态、运行时和表现层解耦，方便并行开发，也便于后期新增渠道或替换 UI。

**Alternatives considered:**

- 单 crate + feature flag：前期快，但后期边界模糊，测试和依赖污染严重。
- 每个界面一个完整独立仓库：会降低复用效率，带来版本和协议漂移。

### 3. 控制面分为本地 HTTP/WebSocket 数据面 + OS 原生 IPC 生命周期面

**Decision:** Web 与桌面本地 UI 使用 loopback HTTP/WebSocket 访问守护进程；服务安装、启动、停止、状态、升级等高权限操作通过 Unix Domain Socket / Windows Named Pipe 抽象的 IPC 层完成。

**Rationale:** 浏览器天然适合 HTTP/WebSocket，而服务生命周期操作更适合受控 IPC。两者分离可以同时满足浏览器兼容性、事件流传输和本地安全控制。

**Alternatives considered:**

- 全部使用 gRPC：浏览器兼容性和本地调试成本偏高。
- 全部使用 HTTP：生命周期与权限控制不够清晰。
- 全部使用 stdio：不利于后台常驻和多客户端并发访问。

### 4. 使用分层配置 + OS keyring + SQLite 本地状态库

**Decision:** 配置采用 `config.toml` + profile 覆盖 + 环境变量覆写的三层模型；敏感凭据默认保存在 OS keyring，配置文件只保存引用；本地状态使用 SQLite。

**Rationale:** 该组合适合单机长期运行服务，既便于备份和迁移，也便于 `doctor` 做静态检查和一致性校验。SQLite 能承载会话索引、任务队列、事件日志和连接器状态。

**Alternatives considered:**

- 纯环境变量：首次配置和多 profile 管理体验差。
- 纯 YAML/JSON 文件：适合简单工具，不适合事件日志和可恢复状态。
- 外部数据库：会显著提高个人助手部署成本。

### 5. 采用统一消息信封与事件总线

**Decision:** 所有入口消息进入系统后都转换成统一 `Envelope`，所有运行时输出统一转换成 `AssistantEvent` 并通过事件总线广播。

**Rationale:** 这样可以让 CLI/TUI/Web/渠道连接器复用同一套消息生命周期，包括输入、思考、工具调用、响应流、失败、重试、审计等。

**Alternatives considered:**

- 每个渠道维护独立消息模型：适配快，但核心逻辑会被迫为每个入口重复实现。

### 6. `init` 与 `doctor` 共享检查项注册表

**Decision:** 使用统一的 `Check` 抽象定义环境检查项，例如配置存在性、密钥可读性、模型连通性、端口占用、数据库迁移状态、Webhook 回调有效性和连接器授权状态。`init` 负责引导并执行关键检查，`doctor` 负责完整检查与修复建议输出。

**Rationale:** 共享检查逻辑可以避免初始化流程和故障诊断逻辑分叉，降低维护成本，并支持 `doctor --json` 供 Web 或自动化流程消费。

**Alternatives considered:**

- 初始化流程硬编码检查：实现快，但无法复用于运行期诊断。
- 只做文档式排错：用户体验差，且不适合多渠道问题排查。

### 7. 连接器采用统一 `Connector` trait，并由网关托管其生命周期

**Decision:** 每个渠道实现统一 trait，例如 `ingest()`, `send()`, `health_check()`, `capabilities()`, `register_webhook()`；网关负责连接器注册、隔离、事件回放、重试和熔断。

**Rationale:** 这样可以在不修改核心运行时的前提下扩展新渠道，并把渠道异常限制在连接器边界内。

**Alternatives considered:**

- 每个渠道直接调用运行时：会造成渠道逻辑和运行时强耦合。
- 连接器独立外部服务：更灵活，但会显著增加 v1 复杂度。

### 8. Web/TUI 只做表现层，业务统一走应用服务层

**Decision:** 在守护进程侧暴露 `SessionService`、`MessageService`、`ConnectorService`、`DoctorService` 等应用服务。CLI/TUI/Web 都只调用这些服务，不各自实现业务流程。

**Rationale:** 可以保证同一会话在不同界面下行为一致，也方便测试和协议演进。

**Alternatives considered:**

- 为每个界面各自封装业务逻辑：交付速度快，但长期会产生行为漂移。

### 9. 安全和可观测性作为底层默认能力，而不是最后补丁

**Decision:** 从首个版本开始加入结构化日志、请求/会话 ID、敏感字段脱敏、Webhook 签名校验、连接器密钥隔离和失败审计。

**Rationale:** 该系统天然涉及模型密钥、企业渠道 token、本地服务端口和消息内容，若后补安全措施，返工成本高。

**Alternatives considered:**

- MVP 后置安全：短期省时，但会让渠道接入和问题定位变得脆弱。

## Risks / Trade-offs

- [跨平台后台服务行为不一致] → 通过 `ServiceManager` 抽象屏蔽 systemd、launchd、Windows Service 差异，并保留前台 direct mode 用于开发和回退。
- [微信生态接入形态不统一] → 明确 v1 支持边界，优先实现官方或可合规接入方式，其他形态以实验性适配器占位。
- [守护进程成为单点故障] → 增加本地 `status`、自动重启、健康探针、事件落盘和 direct mode 降级路径。
- [多前端协议快速演进导致兼容性压力] → 版本化本地 API，并在客户端增加 capability negotiation。
- [连接器故障影响本地交互体验] → 通过异步队列、隔离任务和熔断机制避免单个连接器阻塞主会话。
- [诊断项过多导致误报] → 采用严重级别和建议动作分层输出，区分 fatal / warning / info。

## Migration Plan

1. 先交付 direct mode CLI，验证核心运行时、配置和 Provider 链路。
2. 引入 GoldClaw gateway 服务和本地 API，把 direct mode 逻辑下沉为守护进程可复用服务。
3. 接入 `init` 与 `doctor`，建立配置、密钥和数据库初始化的标准入口。
4. 再增加 TUI 和 Web，统一接入会话服务和事件流。
5. 最后落地飞书、企业微信、微信等连接器，并加入 webhook、健康检查和重试。
6. 发布 beta 安装包，验证后台常驻、升级和故障恢复路径。

回滚策略：

- 始终保留 direct mode CLI 作为最小可用路径。
- 配置和数据库迁移采用版本号管理，并在升级前自动备份。
- 连接器按独立开关启用，可逐个关闭而不影响本地核心交互。

## Open Questions

- v1 默认支持哪些模型 Provider，是否同时支持云端和本地模型（例如 Ollama）？
- Web 端是默认仅监听 `127.0.0.1` 的本地面板，还是支持受控远程访问？
- 微信 v1 的目标形态是公众号/企业接口、桥接网关，还是实验性适配器？
- 是否在 v1 引入简单记忆/知识库能力，还是先只做会话与工具调用底座？
- 是否需要在安装阶段就完成系统服务注册，还是默认按用户命令启停？
