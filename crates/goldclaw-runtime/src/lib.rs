use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use goldclaw_core::{
    AssistantEvent, Envelope, EnvelopeSource, GoldClawError, Policy, PolicyDecision, Provider,
    Result, RuntimeHandle, RuntimeHealth, SessionSummary, SubmissionReceipt, Tool, ToolInvocation,
    ToolOutput,
};
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
    channels: RwLock<HashMap<Uuid, broadcast::Sender<AssistantEvent>>>,
    provider: Arc<dyn Provider>,
    policy: Arc<dyn Policy>,
    tools: HashMap<String, Arc<dyn Tool>>,
}

struct SessionState {
    summary: SessionSummary,
    history: Vec<Envelope>,
}

impl InMemoryRuntime {
    pub fn new(
        provider: Arc<dyn Provider>,
        policy: Arc<dyn Policy>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let tools = tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool))
            .collect();

        Self {
            inner: Arc::new(RuntimeInner {
                sessions: RwLock::new(HashMap::new()),
                channels: RwLock::new(HashMap::new()),
                provider,
                policy,
                tools,
            }),
        }
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

    async fn run_provider(&self, session_id: Uuid, envelope: &Envelope) -> Result<()> {
        let content = self.inner.provider.generate(envelope).await?;
        self.emit(
            session_id,
            AssistantEvent::MessageChunk {
                session_id,
                content: content.clone(),
                at: Utc::now(),
            },
        )
        .await;
        self.emit(
            session_id,
            AssistantEvent::MessageCompleted {
                session_id,
                content,
                at: Utc::now(),
            },
        )
        .await;
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
        self.emit(
            session_id,
            AssistantEvent::ToolCompleted {
                session_id,
                tool_name: tool.name().into(),
                output: output.clone(),
                at: Utc::now(),
            },
        )
        .await;
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

    async fn submit(&self, mut envelope: Envelope) -> Result<SubmissionReceipt> {
        let session_id = if let Some(existing) = envelope.session_id {
            existing
        } else {
            self.create_session(None).await?.id
        };

        envelope.session_id = Some(session_id);

        {
            let mut sessions = self.inner.sessions.write().await;
            let state = sessions
                .get_mut(&session_id)
                .ok_or_else(|| GoldClawError::NotFound(format!("session `{session_id}`")))?;
            state.summary.updated_at = Utc::now();
            state.history.push(envelope.clone());
        }

        self.emit(
            session_id,
            AssistantEvent::MessageAccepted {
                session_id,
                envelope_id: envelope.id,
                at: Utc::now(),
            },
        )
        .await;

        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.process_envelope(session_id, envelope).await;
        });

        Ok(SubmissionReceipt {
            session_id,
            envelope_id: envelope.id,
            accepted_at: Utc::now(),
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

pub struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn generate(&self, envelope: &Envelope) -> Result<String> {
        Ok(format!("echo: {}", envelope.content.trim()))
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
mod tests {
    use super::*;
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn read_tool_rejects_escape() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock went backwards")
            .as_nanos();
        let root = env::temp_dir().join(format!("goldclaw-runtime-{unique}"));
        fs::create_dir_all(&root).expect("create temp root");

        let tool = ReadWorkspaceTool::new(vec![root.clone()]);
        let invocation = ToolInvocation {
            session_id: Uuid::new_v4(),
            tool_name: "read_file".into(),
            source: EnvelopeSource::Cli,
            args: json!({ "path": "../../secret.txt" }),
        };

        let error = tool
            .execute(&invocation)
            .await
            .expect_err("expected read to fail");
        assert!(matches!(
            error,
            GoldClawError::Io(_) | GoldClawError::Unauthorized(_)
        ));

        let _ = fs::remove_dir_all(root);
    }
}
