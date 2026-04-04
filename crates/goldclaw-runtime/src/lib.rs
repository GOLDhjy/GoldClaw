use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use goldclaw_core::{
    AssistantEvent, ChatMessage, Envelope, EnvelopeSource, GoldClawError, MessageBuilder,
    MessageRole, Policy, PolicyDecision, Provider, Result, RuntimeHandle, RuntimeHealth,
    SessionBinding, SessionDetail, SessionMessage, SessionSummary, SubmissionReceipt, Tool,
    ToolInvocation, ToolOutput,
};
use goldclaw_store::{SqliteStore, StoreError};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{RwLock, broadcast};
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
        Self::build(message_builder, provider, policy, tools, None)
    }

    pub async fn with_store(
        message_builder: Arc<dyn MessageBuilder>,
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn Tool>>,
        store: SqliteStore,
    ) -> Result<Self> {
        let runtime = Self::build(
            message_builder,
            provider,
            policy,
            tools,
            Some(store.clone()),
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
        let history = {
            let sessions = self.inner.sessions.read().await;
            sessions
                .get(&session_id)
                .map(|s| s.history.clone())
                .unwrap_or_default()
        };
        let messages = self.inner.message_builder.build(&history);
        let content = self.inner.provider.chat(&messages).await?;
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
            content,
            metadata: json!({ "kind": "provider_response" }),
            created_at: completed_at,
        })
        .await?;
        Ok(())
    }

    async fn run_read_tool(
        &self,
        session_id: Uuid,
        source: &EnvelopeSource,
        path: String,
    ) -> Result<()> {
        let invocation = ToolInvocation {
            session_id,
            tool_name: "read_file".into(),
            source: source.clone(),
            args: json!({ "path": path }),
        };

        match self.inner.policy.authorize(&invocation).await? {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny { reason } => {
                return Err(GoldClawError::Unauthorized(reason));
            }
        }

        let tool = self
            .inner
            .tools
            .get("read_file")
            .cloned()
            .ok_or_else(|| GoldClawError::Internal("read_file tool missing".into()))?;

        self.emit(
            session_id,
            AssistantEvent::ToolStarted {
                session_id,
                tool_name: tool.name().into(),
                at: Utc::now(),
            },
        )
        .await;

        let output = tool.execute(&invocation).await?;
        let completed_at = Utc::now();
        self.emit(
            session_id,
            AssistantEvent::ToolCompleted {
                session_id,
                tool_name: tool.name().into(),
                output: output.clone(),
                at: completed_at,
            },
        )
        .await;
        self.emit(
            session_id,
            AssistantEvent::MessageCompleted {
                session_id,
                content: output.content.clone(),
                at: completed_at,
            },
        )
        .await;

        self.append_message(SessionMessage {
            id: Uuid::new_v4(),
            session_id,
            role: MessageRole::Tool,
            source: EnvelopeSource::System,
            content: output.content,
            metadata: json!({
                "kind": "tool_output",
                "tool_name": tool.name(),
                "summary": output.summary,
            }),
            created_at: completed_at,
        })
        .await?;
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

pub struct StandardMessageBuilder {
    system_prompt: Option<String>,
}

impl StandardMessageBuilder {
    pub fn new(system_prompt: Option<String>) -> Self {
        Self { system_prompt }
    }
}

impl MessageBuilder for StandardMessageBuilder {
    fn build(&self, history: &[SessionMessage]) -> Vec<ChatMessage> {
        let mut messages: Vec<ChatMessage> = Vec::new();

        if let Some(prompt) = &self.system_prompt {
            let has_system = history.iter().any(|m| m.role == MessageRole::System);
            if !has_system {
                messages.push(ChatMessage {
                    role: "system".into(),
                    content: prompt.clone(),
                });
            }
        }

        for msg in history {
            let role = match msg.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            messages.push(ChatMessage {
                role: role.into(),
                content: msg.content.clone(),
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

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
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

fn parse_read_command(content: &str) -> Option<String> {
    let trimmed = content.trim();
    trimmed
        .strip_prefix("/read ")
        .or_else(|| trimmed.strip_prefix("read "))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests;
