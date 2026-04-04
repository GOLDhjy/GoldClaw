use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use goldclaw_core::{
    AssistantEvent, ChatMessage, EmbeddingProvider, Envelope, EnvelopeSource, GoldClawError,
    MemoryChunk, MemoryQuery, MemoryStore, MessageBuilder, MessageRole, Policy, PolicyDecision,
    Provider, Result, RuntimeHandle, RuntimeHealth, SessionBinding, SessionDetail, SessionMessage,
    SessionSummary, SubmissionReceipt, Tool, ToolInvocation, ToolOutput,
};
use goldclaw_store::{SqliteStore, StoreError};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{RwLock, broadcast};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone)]
pub struct InMemoryRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    sessions: RwLock<HashMap<Uuid, SessionState>>,
    bindings: RwLock<HashMap<String, SessionBinding>>,
    channels: RwLock<HashMap<Uuid, broadcast::Sender<AssistantEvent>>>,
    store: Option<SqliteStore>,
    message_builder: Arc<dyn MessageBuilder>,
    provider: Arc<dyn Provider>,
    policy: Arc<dyn Policy>,
    tools: HashMap<String, Arc<dyn Tool>>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    memory_store: Option<Arc<dyn MemoryStore>>,
}

struct SessionState {
    summary: SessionSummary,
    history: Vec<SessionMessage>,
}

impl InMemoryRuntime {
    pub fn new(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self::build(message_builder, provider, policy, tools, None, None, None)
    }

    pub async fn with_store(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn Tool>>,
        store: SqliteStore,
    ) -> Result<Self> {
        Self::with_store_and_memory(message_builder, provider, policy, tools, store, None, None)
            .await
    }

    pub async fn with_store_and_memory(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn Tool>>,
        store: SqliteStore,
        embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
        memory_store: Option<Arc<dyn MemoryStore>>,
    ) -> Result<Self> {
        let runtime = Self::build(
            message_builder,
            provider,
            policy,
            tools,
            Some(store.clone()),
            embedding_provider,
            memory_store,
        );
        runtime.restore_from_store(&store).await?;
        Ok(runtime)
    }

    fn build(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn Tool>>,
        store: Option<SqliteStore>,
        embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
        memory_store: Option<Arc<dyn MemoryStore>>,
    ) -> Self {
        let tools = tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool))
            .collect();

        Self {
            inner: Arc::new(RuntimeInner {
                sessions: RwLock::new(HashMap::new()),
                bindings: RwLock::new(HashMap::new()),
                channels: RwLock::new(HashMap::new()),
                store,
                message_builder,
                provider,
                policy,
                tools,
                embedding_provider,
                memory_store,
            }),
        }
    }

    async fn restore_from_store(&self, store: &SqliteStore) -> Result<()> {
        let snapshot = store.load_snapshot().map_err(map_store_error)?;
        let mut history_by_session: HashMap<Uuid, Vec<SessionMessage>> = HashMap::new();
        for message in snapshot.messages {
            history_by_session
                .entry(message.session_id)
                .or_default()
                .push(message);
        }

        let sessions = snapshot
            .sessions
            .into_iter()
            .map(|session| {
                let history = history_by_session.remove(&session.id).unwrap_or_default();
                (
                    session.id,
                    SessionState {
                        summary: session,
                        history,
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        let bindings = snapshot
            .bindings
            .into_iter()
            .map(|binding| (binding.binding_key(), binding))
            .collect::<HashMap<_, _>>();

        self.inner.sessions.write().await.extend(sessions);
        self.inner.bindings.write().await.extend(bindings);
        Ok(())
    }

    async fn channel_for(&self, session_id: Uuid) -> broadcast::Sender<AssistantEvent> {
        let mut channels = self.inner.channels.write().await;
        channels
            .entry(session_id)
            .or_insert_with(|| {
                let (sender, _) = broadcast::channel(128);
                sender
            })
            .clone()
    }

    async fn emit(&self, session_id: Uuid, event: AssistantEvent) {
        let sender = self.channel_for(session_id).await;
        let _ = sender.send(event);
    }

    async fn append_message(&self, message: SessionMessage) -> Result<()> {
        let summary = {
            let mut sessions = self.inner.sessions.write().await;
            let state = sessions.get_mut(&message.session_id).ok_or_else(|| {
                GoldClawError::NotFound(format!("session `{}`", message.session_id))
            })?;
            state.summary.updated_at = message.created_at;
            state.history.push(message.clone());
            state.summary.clone()
        };

        self.persist_session(&summary)?;
        self.persist_message(&message)?;
        Ok(())
    }

    async fn recall_memory(&self, user_text: &str) -> Option<String> {
        let memory_store = self.inner.memory_store.as_ref()?;

        let embedding = if let Some(embedder) = &self.inner.embedding_provider {
            match embedder.embed(user_text).await {
                Ok(embedding) => {
                    info!(
                        embedding_dims = embedding.len(),
                        query_chars = user_text.chars().count(),
                        "memory query embedding generated"
                    );
                    Some(embedding)
                }
                Err(error) => {
                    warn!("failed to embed memory query, falling back to text recall: {error}");
                    None
                }
            }
        } else {
            None
        };

        let chunks = match memory_store
            .recall(MemoryQuery {
                text: user_text.to_string(),
                embedding,
                limit: 5,
            })
            .await
        {
            Ok(chunks) => chunks,
            Err(error) => {
                warn!("memory recall failed: {error}");
                return None;
            }
        };

        if chunks.is_empty() {
            info!(
                query_chars = user_text.chars().count(),
                "memory recall returned no hits"
            );
            return None;
        }
        info!(
            query_chars = user_text.chars().count(),
            hits = chunks.len(),
            "memory recall returned hits"
        );

        let lines: Vec<String> = chunks
            .iter()
            .map(|c| format!("- {}", c.content.replace('\n', " | ")))
            .collect();

        Some(format!("[Memory]\n{}", lines.join("\n")))
    }

    async fn save_memory_chunk(&self, session_id: Uuid, user_content: &str, assistant_reply: &str) {
        let Some(memory_store) = &self.inner.memory_store else {
            return;
        };

        let content = format!("User: {user_content}\nAssistant: {assistant_reply}");
        let embedding = if let Some(embedder) = &self.inner.embedding_provider {
            match embedder.embed(&content).await {
                Ok(embedding) => {
                    info!(
                        session_id = %session_id,
                        embedding_dims = embedding.len(),
                        content_chars = content.chars().count(),
                        "memory chunk embedding generated"
                    );
                    Some(embedding)
                }
                Err(error) => {
                    warn!("failed to embed memory chunk, saving without embedding: {error}");
                    None
                }
            }
        } else {
            None
        };

        let chunk = MemoryChunk {
            id: Uuid::new_v4(),
            session_id: Some(session_id),
            content,
            embedding,
            created_at: Utc::now(),
            metadata: json!({}),
        };
        match memory_store.save_chunk(chunk).await {
            Ok(()) => info!(session_id = %session_id, "memory chunk persisted"),
            Err(error) => warn!("failed to save memory chunk: {error}"),
        }
    }

    fn persist_session(&self, session: &SessionSummary) -> Result<()> {
        if let Some(store) = &self.inner.store {
            store.upsert_session(session).map_err(map_store_error)?;
        }
        Ok(())
    }

    fn persist_message(&self, message: &SessionMessage) -> Result<()> {
        if let Some(store) = &self.inner.store {
            store.append_message(message).map_err(map_store_error)?;
        }
        Ok(())
    }

    fn persist_binding(&self, binding: &SessionBinding) -> Result<()> {
        if let Some(store) = &self.inner.store {
            store
                .upsert_session_binding(binding)
                .map_err(map_store_error)?;
        }
        Ok(())
    }

    async fn process_envelope(&self, session_id: Uuid, envelope: Envelope) {
        let outcome = if let Some(path) = parse_read_command(&envelope.content) {
            self.run_read_tool(session_id, &envelope.source, path).await
        } else {
            self.run_provider(session_id, &envelope).await
        };

        if let Err(error) = outcome {
            self.emit(
                session_id,
                AssistantEvent::Error {
                    session_id: Some(session_id),
                    message: error.to_string(),
                    at: Utc::now(),
                },
            )
            .await;
        }
    }

    async fn ensure_session_binding(
        &self,
        session: &SessionSummary,
        envelope: &Envelope,
    ) -> Result<()> {
        let Some(binding_key) = envelope.binding_key() else {
            return Ok(());
        };
        let Some(conversation) = envelope.conversation.as_ref() else {
            return Ok(());
        };

        let binding = SessionBinding {
            session_id: session.id,
            source: envelope.source.clone(),
            source_instance: conversation
                .source_instance
                .clone()
                .unwrap_or_else(|| "default".into()),
            conversation_id: conversation.conversation_id.clone(),
            sender_id: conversation.sender_id.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let to_persist = {
            let mut bindings = self.inner.bindings.write().await;
            let entry = bindings
                .entry(binding_key)
                .or_insert_with(|| binding.clone());
            entry.session_id = binding.session_id;
            entry.updated_at = binding.updated_at;
            entry.sender_id = binding.sender_id.clone();
            entry.source = binding.source.clone();
            entry.source_instance = binding.source_instance.clone();
            entry.conversation_id = binding.conversation_id.clone();
            entry.clone()
        };

        self.persist_binding(&to_persist)?;
        Ok(())
    }

    async fn resolve_session(&self, envelope: &Envelope) -> Result<SessionSummary> {
        if let Some(session_id) = envelope.session_id {
            let sessions = self.inner.sessions.read().await;
            return sessions
                .get(&session_id)
                .map(|state| state.summary.clone())
                .ok_or_else(|| GoldClawError::NotFound(format!("session `{session_id}`")));
        }

        if let Some(binding_key) = envelope.binding_key() {
            let maybe_existing_id = {
                let bindings = self.inner.bindings.read().await;
                bindings.get(&binding_key).map(|binding| binding.session_id)
            };

            if let Some(session_id) = maybe_existing_id {
                let sessions = self.inner.sessions.read().await;
                if let Some(state) = sessions.get(&session_id) {
                    return Ok(state.summary.clone());
                }
            }

            let title = envelope
                .conversation
                .as_ref()
                .map(auto_title_for_conversation)
                .or_else(|| Some("New session".into()));
            let session = self.create_session(title).await?;
            self.ensure_session_binding(&session, envelope).await?;
            return Ok(session);
        }

        self.create_session(None).await
    }

    async fn run_provider(&self, session_id: Uuid, envelope: &Envelope) -> Result<()> {
        let memory_block = self.recall_memory(&envelope.content).await;

        for _ in 0..4 {
            let history = {
                let sessions = self.inner.sessions.read().await;
                sessions
                    .get(&session_id)
                    .map(|s| s.history.clone())
                    .unwrap_or_default()
            };

            let mut messages = self.inner.message_builder.build(&history);

            // Inject relevant memories into the last user message.
            if let Some(memory_block) = memory_block.as_deref() {
                if let Some(last_user) = messages.iter_mut().rfind(|m| m.role == "user") {
                    last_user.content =
                        format!("{memory_block}\n\n[User Input]\n{}", last_user.content);
                }
            }

            let content = self.inner.provider.chat(&messages).await?;
            if let Some(tool_call) = parse_assistant_tool_call(&content)? {
                self.execute_tool(ToolInvocation {
                    session_id,
                    tool_name: tool_call.tool,
                    source: EnvelopeSource::System,
                    args: tool_call.args,
                })
                .await?;
                continue;
            }

            let completed_at = Utc::now();
            self.emit(
                session_id,
                AssistantEvent::MessageChunk {
                    session_id,
                    content: content.clone(),
                    at: completed_at,
                },
            )
            .await;
            self.emit(
                session_id,
                AssistantEvent::MessageCompleted {
                    session_id,
                    content: content.clone(),
                    at: completed_at,
                },
            )
            .await;

            self.append_message(SessionMessage {
                id: Uuid::new_v4(),
                session_id,
                role: MessageRole::Assistant,
                source: envelope.source.clone(),
                content: content.clone(),
                metadata: json!({ "kind": "provider_response" }),
                created_at: completed_at,
            })
            .await?;

            // Save memory chunk after response (best effort).
            self.save_memory_chunk(session_id, &envelope.content, &content)
                .await;

            return Ok(());
        }

        Err(GoldClawError::Internal(
            "assistant exceeded tool call limit".into(),
        ))
    }

    async fn execute_tool(&self, invocation: ToolInvocation) -> Result<ToolOutput> {
        match self.inner.policy.authorize(&invocation).await? {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny { reason } => {
                return Err(GoldClawError::Unauthorized(reason));
            }
        }

        let tool = self
            .inner
            .tools
            .get(&invocation.tool_name)
            .cloned()
            .ok_or_else(|| {
                GoldClawError::Internal(format!("tool `{}` missing", invocation.tool_name))
            })?;

        self.emit(
            invocation.session_id,
            AssistantEvent::ToolStarted {
                session_id: invocation.session_id,
                tool_name: tool.name().into(),
                at: Utc::now(),
            },
        )
        .await;

        let output = tool.execute(&invocation).await?;
        let completed_at = Utc::now();
        self.emit(
            invocation.session_id,
            AssistantEvent::ToolCompleted {
                session_id: invocation.session_id,
                tool_name: tool.name().into(),
                output: output.clone(),
                at: completed_at,
            },
        )
        .await;

        self.append_message(SessionMessage {
            id: Uuid::new_v4(),
            session_id: invocation.session_id,
            role: MessageRole::Tool,
            source: EnvelopeSource::System,
            content: output.content.clone(),
            metadata: json!({
                "kind": "tool_output",
                "tool_name": tool.name(),
                "summary": output.summary,
            }),
            created_at: completed_at,
        })
        .await?;
        Ok(output)
    }

    async fn run_read_tool(
        &self,
        session_id: Uuid,
        source: &EnvelopeSource,
        path: String,
    ) -> Result<()> {
        let output = self
            .execute_tool(ToolInvocation {
                session_id,
                tool_name: "read_file".into(),
                source: source.clone(),
                args: json!({ "path": path }),
            })
            .await?;

        self.emit(
            session_id,
            AssistantEvent::MessageCompleted {
                session_id,
                content: output.content,
                at: Utc::now(),
            },
        )
        .await;
        Ok(())
    }
}

#[async_trait]
impl RuntimeHandle for InMemoryRuntime {
    async fn create_session(&self, title: Option<String>) -> Result<SessionSummary> {
        let now = Utc::now();
        let session = SessionSummary {
            id: Uuid::new_v4(),
            title: title.unwrap_or_else(|| "New session".into()),
            created_at: now,
            updated_at: now,
        };

        {
            let mut sessions = self.inner.sessions.write().await;
            sessions.insert(
                session.id,
                SessionState {
                    summary: session.clone(),
                    history: Vec::new(),
                },
            );
        }

        self.persist_session(&session)?;

        self.emit(
            session.id,
            AssistantEvent::SessionCreated {
                session: session.clone(),
                at: now,
            },
        )
        .await;

        Ok(session)
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let sessions = self.inner.sessions.read().await;
        let mut list = sessions
            .values()
            .map(|state| state.summary.clone())
            .collect::<Vec<_>>();
        list.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(list)
    }

    async fn load_session(&self, session_id: Uuid) -> Result<SessionDetail> {
        let sessions = self.inner.sessions.read().await;
        let state = sessions
            .get(&session_id)
            .ok_or_else(|| GoldClawError::NotFound(format!("session `{session_id}`")))?;
        Ok(SessionDetail {
            session: state.summary.clone(),
            messages: state.history.clone(),
        })
    }

    async fn submit(&self, mut envelope: Envelope) -> Result<SubmissionReceipt> {
        let session = self.resolve_session(&envelope).await?;
        let session_id = session.id;
        let envelope_id = envelope.id;
        let accepted_at = Utc::now();

        envelope.session_id = Some(session_id);
        self.ensure_session_binding(&session, &envelope).await?;
        self.append_message(message_from_envelope(&envelope))
            .await?;

        self.emit(
            session_id,
            AssistantEvent::MessageAccepted {
                session_id,
                envelope_id: envelope.id,
                at: accepted_at,
            },
        )
        .await;

        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.process_envelope(session_id, envelope).await;
        });

        Ok(SubmissionReceipt {
            session_id,
            envelope_id,
            accepted_at,
        })
    }

    async fn subscribe(&self, session_id: Uuid) -> Result<broadcast::Receiver<AssistantEvent>> {
        Ok(self.channel_for(session_id).await.subscribe())
    }

    async fn health(&self) -> Result<RuntimeHealth> {
        let sessions = self.inner.sessions.read().await;
        Ok(RuntimeHealth {
            healthy: true,
            provider: self.inner.provider.name().into(),
            session_count: sessions.len(),
        })
    }
}

fn message_from_envelope(envelope: &Envelope) -> SessionMessage {
    SessionMessage {
        id: envelope.id,
        session_id: envelope.session_id.expect("resolved envelope session"),
        role: MessageRole::User,
        source: envelope.source.clone(),
        content: envelope.content.clone(),
        metadata: json!({
            "envelope_id": envelope.id,
            "conversation": &envelope.conversation,
        }),
        created_at: envelope.created_at,
    }
}

fn map_store_error(error: StoreError) -> GoldClawError {
    match error {
        StoreError::Io(error) => GoldClawError::Io(error.to_string()),
        StoreError::Sqlite(error) => GoldClawError::Internal(format!("sqlite error: {error}")),
        StoreError::Json(error) => GoldClawError::InvalidInput(error.to_string()),
        StoreError::InvalidData(message) => GoldClawError::Internal(message),
        StoreError::LockPoisoned => GoldClawError::Internal("store lock poisoned".into()),
    }
}

fn auto_title_for_conversation(conversation: &goldclaw_core::ConversationRef) -> String {
    if let Some(sender_id) = &conversation.sender_id {
        format!("{} ({sender_id})", conversation.conversation_id)
    } else {
        conversation.conversation_id.clone()
    }
}

const ASSISTANT_TOOL_CALL_OPEN: &str = "<tool_call>";
const ASSISTANT_TOOL_CALL_CLOSE: &str = "</tool_call>";
const LOCAL_ASSISTANT_RUNTIME_PROMPT: &str = r#"你运行在 GoldClaw 中。

下面的 [Active Soul] 是当前生效的人设文件，每次模型调用都会重新读取。
如果用户要求修改你的人设、自我介绍、长期行为规则、回答风格、语气、格式习惯，或者其他持续生效的对话规则，你必须调用 `update_soul`，不能只在回复里口头答应。
你已经能看到完整的当前 soul 文件内容，不需要先再读取一次才能修改。

工具调用格式：你必须只输出下面这个 XML 块，不能输出其它文本：
<tool_call>
{"tool":"tool_name","args":{...}}
</tool_call>

工具执行后，你会基于最新上下文再次被调用，然后再正常回复用户。

可用工具：
- update_soul: {"content":"完整的新 soul.md 内容"}。你必须基于当前 [Active Soul] 生成修改后的完整全文，不要只传片段、补丁或说明文字。工具会把写入后的全文返回给你。
- read_file: {"path":"相对或绝对路径"}。它会读取允许范围内的 UTF-8 文本文件。"#;

#[derive(Deserialize)]
struct AssistantToolCall {
    tool: String,
    #[serde(default = "default_tool_args")]
    args: Value,
}

fn default_tool_args() -> Value {
    json!({})
}

pub struct StandardMessageBuilder {
    system_prompt: Option<String>,
    soul_path: Option<PathBuf>,
}

impl StandardMessageBuilder {
    pub fn new(system_prompt: Option<String>) -> Self {
        Self {
            system_prompt,
            soul_path: None,
        }
    }

    pub fn with_soul_path(soul_path: PathBuf) -> Self {
        Self {
            system_prompt: Some(LOCAL_ASSISTANT_RUNTIME_PROMPT.into()),
            soul_path: Some(soul_path),
        }
    }

    fn load_system_prompt(&self) -> Option<String> {
        let mut sections = Vec::new();
        if let Some(prompt) = &self.system_prompt {
            let prompt = prompt.trim();
            if !prompt.is_empty() {
                sections.push(prompt.to_string());
            }
        }

        if let Some(soul_path) = &self.soul_path {
            if let Ok(content) = fs::read_to_string(soul_path) {
                let content = content.trim();
                if !content.is_empty() {
                    sections.push(format!("[Active Soul]\n{content}"));
                }
            }
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }
}

impl MessageBuilder for StandardMessageBuilder {
    fn build(&self, history: &[SessionMessage]) -> Vec<ChatMessage> {
        let mut messages: Vec<ChatMessage> = Vec::new();

        if let Some(prompt) = self.load_system_prompt() {
            messages.push(ChatMessage {
                role: "system".into(),
                content: prompt,
            });
        }

        for msg in history {
            if is_legacy_soul_message(msg) {
                continue;
            }

            messages.push(match msg.role {
                MessageRole::System => ChatMessage {
                    role: "system".into(),
                    content: msg.content.clone(),
                },
                MessageRole::User => ChatMessage {
                    role: "user".into(),
                    content: msg.content.clone(),
                },
                MessageRole::Assistant => ChatMessage {
                    role: "assistant".into(),
                    content: msg.content.clone(),
                },
                MessageRole::Tool => format_tool_message(msg),
            });
        }

        messages
    }
}

pub struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        let content = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(format!("echo: {}", content.trim()))
    }
}

pub struct StaticPolicy {
    allowed_tools: HashSet<String>,
}

impl StaticPolicy {
    pub fn allow_only<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed_tools: tools.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl Policy for StaticPolicy {
    async fn authorize(&self, invocation: &ToolInvocation) -> Result<PolicyDecision> {
        if self.allowed_tools.contains(&invocation.tool_name) {
            Ok(PolicyDecision::Allow)
        } else {
            Ok(PolicyDecision::Deny {
                reason: format!(
                    "tool `{}` is not allowed in runtime v0",
                    invocation.tool_name
                ),
            })
        }
    }
}

pub struct ReadWorkspaceTool {
    roots: Vec<PathBuf>,
    max_bytes: u64,
}

impl ReadWorkspaceTool {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            max_bytes: 64 * 1024,
        }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let requested = PathBuf::from(path);
        let candidate = if requested.is_absolute() {
            requested
        } else {
            let base = self
                .roots
                .first()
                .ok_or_else(|| GoldClawError::InvalidInput("no read roots configured".into()))?;
            base.join(requested)
        };

        let canonical_target = fs::canonicalize(&candidate).map_err(|error| {
            GoldClawError::Io(format!(
                "failed to resolve `{}`: {error}",
                candidate.display()
            ))
        })?;

        let inside_root = self.roots.iter().any(|root| {
            fs::canonicalize(root)
                .map(|canonical_root| canonical_target.starts_with(canonical_root))
                .unwrap_or(false)
        });

        if inside_root {
            Ok(canonical_target)
        } else {
            Err(GoldClawError::Unauthorized(format!(
                "path `{}` is outside configured read roots",
                candidate.display()
            )))
        }
    }

    fn read_text_file(&self, path: &Path) -> Result<String> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(GoldClawError::InvalidInput(format!(
                "`{}` is not a regular file",
                path.display()
            )));
        }
        if metadata.len() > self.max_bytes {
            return Err(GoldClawError::InvalidInput(format!(
                "`{}` exceeds the {} byte limit",
                path.display(),
                self.max_bytes
            )));
        }

        fs::read_to_string(path).map_err(|error| {
            GoldClawError::Io(format!("failed to read `{}`: {error}", path.display()))
        })
    }
}

pub struct UpdateSoulTool {
    soul_path: PathBuf,
}

impl UpdateSoulTool {
    pub fn new(soul_path: PathBuf) -> Self {
        Self { soul_path }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
struct UpdateSoulArgs {
    content: String,
}

#[async_trait]
impl Tool for ReadWorkspaceTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: ReadArgs = serde_json::from_value(invocation.args.clone())?;
        let path = self.resolve_path(&args.path)?;
        let content = self.read_text_file(&path)?;
        Ok(ToolOutput {
            summary: format!("read {}", path.display()),
            content,
        })
    }
}

#[async_trait]
impl Tool for UpdateSoulTool {
    fn name(&self) -> &'static str {
        "update_soul"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: UpdateSoulArgs = serde_json::from_value(invocation.args.clone())?;
        let content = normalize_soul_content(&args.content)?;

        if let Some(parent) = self.soul_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.soul_path, content.as_bytes()).map_err(|error| {
            GoldClawError::Io(format!(
                "failed to write `{}`: {error}",
                self.soul_path.display()
            ))
        })?;

        Ok(ToolOutput {
            summary: format!("updated {}", self.soul_path.display()),
            content,
        })
    }
}

fn parse_read_command(content: &str) -> Option<String> {
    let trimmed = content.trim();
    trimmed
        .strip_prefix("/read ")
        .or_else(|| trimmed.strip_prefix("read "))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_assistant_tool_call(content: &str) -> Result<Option<AssistantToolCall>> {
    let trimmed = content.trim();
    if !trimmed.starts_with(ASSISTANT_TOOL_CALL_OPEN) {
        return Ok(None);
    }
    if !trimmed.ends_with(ASSISTANT_TOOL_CALL_CLOSE) {
        return Err(GoldClawError::Internal(
            "assistant emitted malformed tool call wrapper".into(),
        ));
    }

    let payload =
        &trimmed[ASSISTANT_TOOL_CALL_OPEN.len()..trimmed.len() - ASSISTANT_TOOL_CALL_CLOSE.len()];
    let call: AssistantToolCall = serde_json::from_str(payload.trim()).map_err(|error| {
        GoldClawError::Internal(format!(
            "assistant emitted malformed tool call JSON: {error}"
        ))
    })?;

    if call.tool.trim().is_empty() {
        return Err(GoldClawError::Internal(
            "assistant emitted tool call without a tool name".into(),
        ));
    }

    Ok(Some(call))
}

fn is_legacy_soul_message(message: &SessionMessage) -> bool {
    message.role == MessageRole::System
        && message
            .metadata
            .get("kind")
            .and_then(|value| value.as_str())
            == Some("soul")
}

fn format_tool_message(message: &SessionMessage) -> ChatMessage {
    let tool_name = message
        .metadata
        .get("tool_name")
        .and_then(|value| value.as_str())
        .unwrap_or("tool");
    let summary = message
        .metadata
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or("");

    let mut content = format!("[Tool `{tool_name}` Result]");
    if !summary.is_empty() {
        content.push_str(&format!("\nSummary: {summary}"));
    }
    if !message.content.is_empty() {
        content.push_str(&format!("\n{}", message.content));
    }

    ChatMessage {
        role: "system".into(),
        content,
    }
}

fn normalize_soul_content(content: &str) -> Result<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(GoldClawError::InvalidInput(
            "soul content cannot be empty".into(),
        ));
    }

    let mut normalized = trimmed.to_string();
    normalized.push('\n');
    Ok(normalized)
}

#[cfg(test)]
mod tests;
