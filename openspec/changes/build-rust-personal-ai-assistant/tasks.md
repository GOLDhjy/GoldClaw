## 1. Workspace Foundation

- [x] 1.1 创建 Rust workspace，并拆分 `goldclaw-core`、`goldclaw-config`、`goldclaw-store`、`goldclaw-runtime`、`goldclaw-gateway`、`goldclaw-doctor` 等基础 crate
- [x] 1.2 建立 `goldclaw`、TUI、Web 三类应用入口和统一的版本管理方式
- [ ] 1.3 接入基础依赖与开发工具链，包括 `tokio`、`serde`、`tracing`、`axum`、`clap`、`ratatui`、数据库迁移工具
- [x] 1.4 定义核心领域模型、事件模型、错误模型和公共 trait
- [ ] 1.5 建立基础 CI，包括格式化、lint、单元测试和 workspace 构建检查

## 2. Configuration and Local State

- [ ] 2.1 设计 `config.toml`、profile 覆盖和环境变量覆写规则
- [ ] 2.2 实现非敏感配置加载、schema 校验和默认值填充
- [ ] 2.3 接入 OS keyring 或等价密钥后端，支持凭据存储与读取
- [ ] 2.4 设计 SQLite schema，覆盖会话索引、消息元数据、任务队列、连接器状态和诊断缓存
- [ ] 2.5 建立数据库迁移、备份和版本回滚机制

## 3. Assistant Runtime Core

- [ ] 3.1 实现统一的会话管理器，支持创建、恢复、列出和归档会话
- [ ] 3.2 定义模型 Provider 抽象，先接入至少一个云端 Provider 和一个本地 Provider 适配位
- [ ] 3.3 定义工具调用接口、工具注册机制和工具执行上下文
- [x] 3.4 建立统一 `Envelope` 输入结构和 `AssistantEvent` 输出结构
- [x] 3.5 实现运行时事件总线和基础流式响应链路
- [ ] 3.6 将关键运行时状态持久化到本地状态库，并验证重启恢复

## 4. Gateway Daemon and Control Plane

- [x] 4.1 实现 GoldClaw gateway 前台模式和后台模式的启动入口
- [x] 4.2 设计并实现本地 HTTP API，用于会话查询、消息提交、状态读取和诊断调用
- [x] 4.3 设计并实现 WebSocket 或 SSE 事件流接口，用于流式输出和状态推送
- [ ] 4.4 实现跨平台 IPC 生命周期接口，覆盖 install/start/stop/status/logs
- [ ] 4.5 建立多客户端并发访问下的连接管理、订阅管理和冲突处理
- [ ] 4.6 提供 direct mode 回退路径，并保证与守护进程模式共享同一运行时服务层

## 5. Init and Doctor Experience

- [ ] 5.1 设计首次启动向导流程，包括 profile 命名、Provider 选择、凭据录入和可选渠道启用
- [ ] 5.2 实现可复用的检查项框架，支持 fatal、warning、info 三种严重级别
- [ ] 5.3 增加配置、密钥、Provider 连通性、端口占用、数据库迁移和本地目录权限检查
- [ ] 5.4 增加 Webhook、渠道授权和连接器健康检查
- [ ] 5.5 实现 `doctor --json` 与人类可读输出格式
- [ ] 5.6 为常见失败场景提供可执行修复建议或自动修复入口

## 6. CLI and TUI Surfaces

- [ ] 6.1 设计 CLI 命令层级，覆盖 `init`、`doctor`、`run`、`daemon`、`session`、`connector` 等命令
- [ ] 6.2 实现 CLI 的 direct mode 对话和 daemon mode 代理调用
- [ ] 6.3 使用 `ratatui` 构建 TUI 会话列表、消息面板、状态栏和诊断视图
- [ ] 6.4 让 TUI 接入统一事件流，支持流式输出、工具事件和错误提示
- [ ] 6.5 实现会话切换、历史查看、快捷键帮助和重连逻辑

## 7. Web Surface

- [ ] 7.1 确定 Web 技术形态，并实现本地 Web 面板的基础骨架
- [ ] 7.2 接入会话列表、会话详情、消息输入和流式响应显示
- [ ] 7.3 增加系统状态页，展示守护进程状态、连接器状态和 doctor 结果
- [ ] 7.4 增加配置页或设置入口，用于查看 profile、Provider 和渠道启用状态
- [ ] 7.5 增加本地安全控制，例如 loopback 绑定、origin 校验和敏感信息隐藏

## 8. Connector Framework

- [ ] 8.1 定义统一 `Connector` trait、连接器注册器和生命周期状态机
- [ ] 8.2 设计入站消息标准化流程，把不同渠道事件映射为统一 `Envelope`
- [ ] 8.3 设计出站消息发送接口、重试策略、死信记录和禁用策略
- [ ] 8.4 为连接器增加健康检查、能力声明和凭据校验接口
- [ ] 8.5 建立连接器隔离执行模型，避免单个渠道故障阻塞主运行时

## 9. Feishu / WeCom / WeChat Adapters

- [ ] 9.1 实现飞书连接器的认证、Webhook 或事件接收、消息发送和健康检查
- [ ] 9.2 实现企业微信连接器的认证、事件接收、消息发送和健康检查
- [ ] 9.3 明确微信 v1 支持边界，并实现对应接入路径或实验性适配器骨架
- [ ] 9.4 为三类连接器补充签名校验、重试、幂等和速率限制处理
- [ ] 9.5 让 `init` 和 `doctor` 能识别并配置这些连接器

## 10. Security, Observability, and Reliability

- [ ] 10.1 接入结构化日志、trace/span 和请求/会话 ID 贯穿能力
- [ ] 10.2 对模型密钥、渠道 token、Webhook secret 等敏感字段做统一脱敏
- [ ] 10.3 增加运行时和连接器级别的审计事件记录
- [ ] 10.4 为关键后台任务增加超时、取消、重试和熔断控制
- [ ] 10.5 增加守护进程健康探针、崩溃恢复和启动自检

## 11. Packaging and Service Management

- [ ] 11.1 抽象 `ServiceManager`，封装 systemd、launchd 和 Windows Service 安装逻辑
- [ ] 11.2 实现安装、卸载、升级、状态查看和日志查看命令
- [ ] 11.3 设计本地目录结构，包括配置、数据库、日志、缓存和临时文件位置
- [ ] 11.4 产出跨平台安装说明和 beta 分发方案
- [ ] 11.5 验证升级回滚和 direct mode 降级路径

## 12. Verification and Beta Readiness

- [ ] 12.1 为运行时、配置、数据库和诊断模块编写单元测试
- [ ] 12.2 为守护进程 API、事件流和多客户端并发访问编写集成测试
- [ ] 12.3 为飞书、企业微信、微信连接器编写契约测试和失败恢复测试
- [ ] 12.4 进行跨平台手工验收，覆盖首次安装、后台常驻、Web/TUI 切换和渠道收发
- [ ] 12.5 整理开发者文档、运维文档和故障排查手册，准备 beta 发布
