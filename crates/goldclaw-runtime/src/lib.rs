pub mod tools;

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use goldclaw_core::{
    AssistantEvent, ChatFunction, ChatMessage, ChatToolCall, EmbeddingProvider, Envelope,
    EnvelopeSource, GoldClawError, MemoryChunk, MemoryQuery, MemoryStore, MessageBuilder,
    MessageRole, Policy, PolicyDecision, Provider, ProviderOutput, Result, RuntimeHandle,
    RuntimeHealth, SessionBinding, SessionDetail, SessionMessage, SessionSummary,
    SubmissionReceipt, ToolDefinition, ToolInvocation, ToolOutput,
};
use goldclaw_store::{SqliteStore, StoreError};
use serde_json::json;
use tokio::sync::{RwLock, broadcast};
use tracing::{info, warn};
use uuid::Uuid;

use tools::BuiltinTool;

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
    tools: HashMap<String, Arc<dyn BuiltinTool>>,
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
        tools: Vec<Arc<dyn BuiltinTool>>,
    ) -> Self {
        Self::build(message_builder, provider, policy, tools, None, None, None)
    }

    /// Sessions are in-memory only; only the shared memory store is persisted.
    pub fn with_memory(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn BuiltinTool>>,
        embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
        memory_store: Option<Arc<dyn MemoryStore>>,
    ) -> Self {
        Self::build(
            message_builder,
            provider,
            policy,
            tools,
            None,
            embedding_provider,
            memory_store,
        )
    }

    pub async fn with_store(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn BuiltinTool>>,
        store: SqliteStore,
    ) -> Result<Self> {
        Self::with_store_and_memory(message_builder, provider, policy, tools, store, None, None)
            .await
    }

    pub async fn with_store_and_memory(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn BuiltinTool>>,
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
        tools: Vec<Arc<dyn BuiltinTool>>,
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

        let tool_defs: Vec<ToolDefinition> = self
            .inner
            .tools
            .values()
            .map(|t| t.tool_definition())
            .collect();

        for _ in 0..8 {
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

            for (i, msg) in messages.iter().enumerate() {
                tracing::debug!(
                    turn = i,
                    role = %msg.role,
                    tool_calls = msg.tool_calls.len(),
                    "→ llm message"
                );
            }

            let output = self.inner.provider.chat(&messages, &tool_defs).await?;

            match output {
                ProviderOutput::ToolCall { id, name, args } => {
                    tracing::debug!(tool = %name, call_id = %id, "← llm tool call");
                    let arguments = serde_json::to_string(&args).unwrap_or_default();
                    self.append_message(SessionMessage {
                        id: Uuid::new_v4(),
                        session_id,
                        role: MessageRole::Assistant,
                        source: envelope.source.clone(),
                        content: String::new(),
                        metadata: json!({
                            "kind": "tool_call",
                            "tool_call_id": id,
                            "tool_name": name,
                            "arguments": arguments,
                        }),
                        created_at: Utc::now(),
                    })
                    .await?;

                    self.execute_tool(ToolInvocation {
                        session_id,
                        tool_name: name,
                        source: EnvelopeSource::System,
                        args,
                        tool_call_id: id,
                    })
                    .await?;
                }

                ProviderOutput::Text(content) => {
                    tracing::debug!(chars = content.chars().count(), "← llm text response");
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

                    self.save_memory_chunk(session_id, &envelope.content, &content)
                        .await;

                    return Ok(());
                }
            }
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
                "tool_call_id": invocation.tool_call_id,
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
                tool_call_id: "internal-read".into(),
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

const RUNTIME_BASE_PROMPT: &str = "\
你运行在 GoldClaw 中。

下面的 [Active Soul] 是当前生效的人设文件，每次模型调用都会重新读取。
重要：如果用户要求修改你的人设、名字、称呼、语言、风格或任何长期规则，你必须调用 `update_soul` 工具，\
绝对不能只口头答应却不调用工具——不调用等于没改。";

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
            system_prompt: Some(RUNTIME_BASE_PROMPT.into()),
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
            messages.push(ChatMessage::text("system", prompt));
        }

        for msg in history {
            if is_legacy_soul_message(msg) {
                continue;
            }

            messages.push(match msg.role {
                MessageRole::System => ChatMessage::text("system", msg.content.clone()),
                MessageRole::User => ChatMessage::text("user", msg.content.clone()),
                MessageRole::Assistant => {
                    let kind = msg.metadata.get("kind").and_then(|v| v.as_str());
                    if kind == Some("tool_call") {
                        let tool_call_id = msg
                            .metadata
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let tool_name = msg
                            .metadata
                            .get("tool_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = msg
                            .metadata
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        ChatMessage {
                            role: "assistant".into(),
                            content: String::new(),
                            tool_calls: vec![ChatToolCall {
                                id: tool_call_id,
                                call_type: "function".into(),
                                function: ChatFunction {
                                    name: tool_name,
                                    arguments,
                                },
                            }],
                            tool_call_id: None,
                        }
                    } else {
                        ChatMessage::text("assistant", msg.content.clone())
                    }
                }
                MessageRole::Tool => {
                    let tool_call_id = msg
                        .metadata
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    ChatMessage {
                        role: "tool".into(),
                        content: msg.content.clone(),
                        tool_calls: vec![],
                        tool_call_id,
                    }
                }
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

    async fn chat(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
    ) -> Result<ProviderOutput> {
        let content = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(ProviderOutput::Text(format!("echo: {}", content.trim())))
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

fn parse_read_command(content: &str) -> Option<String> {
    let trimmed = content.trim();
    trimmed
        .strip_prefix("/read ")
        .or_else(|| trimmed.strip_prefix("read "))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

fn is_legacy_soul_message(message: &SessionMessage) -> bool {
    message.role == MessageRole::System
        && message
            .metadata
            .get("kind")
            .and_then(|value| value.as_str())
            == Some("soul")
}

#[cfg(test)]
mod tests;
