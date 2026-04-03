use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type SessionId = Uuid;
pub type EnvelopeId = Uuid;
pub type MessageId = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeSource {
    Cli,
    Tui,
    Web,
    Connector(String),
    System,
}

impl EnvelopeSource {
    pub fn key(&self) -> String {
        match self {
            Self::Cli => "cli".into(),
            Self::Tui => "tui".into(),
            Self::Web => "web".into(),
            Self::Connector(name) => format!("connector:{name}"),
            Self::System => "system".into(),
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "cli" => Some(Self::Cli),
            "tui" => Some(Self::Tui),
            "web" => Some(Self::Web),
            "system" => Some(Self::System),
            value if value.starts_with("connector:") => {
                Some(Self::Connector(value["connector:".len()..].to_string()))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationRef {
    pub source_instance: Option<String>,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub external_message_id: Option<String>,
}

impl ConversationRef {
    pub fn new(conversation_id: impl Into<String>) -> Self {
        Self {
            source_instance: None,
            conversation_id: conversation_id.into(),
            sender_id: None,
            external_message_id: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub id: EnvelopeId,
    pub session_id: Option<SessionId>,
    pub source: EnvelopeSource,
    pub conversation: Option<ConversationRef>,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

impl Envelope {
    pub fn user(
        content: impl Into<String>,
        source: EnvelopeSource,
        session_id: Option<SessionId>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            source,
            conversation: None,
            content: content.into(),
            created_at: Utc::now(),
        }
    }

    pub fn binding_key(&self) -> Option<String> {
        let conversation = self.conversation.as_ref()?;
        let source_instance = conversation.source_instance.as_deref().unwrap_or("default");
        Some(format!(
            "{}|{}|{}",
            self.source.key(),
            source_instance,
            conversation.conversation_id
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionBinding {
    pub session_id: SessionId,
    pub source: EnvelopeSource,
    pub source_instance: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionBinding {
    pub fn binding_key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.source.key(),
            self.source_instance,
            self.conversation_id
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMessage {
    pub id: MessageId,
    pub session_id: SessionId,
    pub role: MessageRole,
    pub source: EnvelopeSource,
    pub content: String,
    #[serde(default = "default_message_metadata")]
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

fn default_message_metadata() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmissionReceipt {
    pub session_id: SessionId,
    pub envelope_id: EnvelopeId,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub session_id: SessionId,
    pub tool_name: String,
    pub source: EnvelopeSource,
    pub args: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolOutput {
    pub summary: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Deny { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeHealth {
    pub healthy: bool,
    pub provider: String,
    pub session_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantEvent {
    SessionCreated {
        session: SessionSummary,
        at: DateTime<Utc>,
    },
    MessageAccepted {
        session_id: SessionId,
        envelope_id: EnvelopeId,
        at: DateTime<Utc>,
    },
    ToolStarted {
        session_id: SessionId,
        tool_name: String,
        at: DateTime<Utc>,
    },
    ToolCompleted {
        session_id: SessionId,
        tool_name: String,
        output: ToolOutput,
        at: DateTime<Utc>,
    },
    MessageChunk {
        session_id: SessionId,
        content: String,
        at: DateTime<Utc>,
    },
    MessageCompleted {
        session_id: SessionId,
        content: String,
        at: DateTime<Utc>,
    },
    Error {
        session_id: Option<SessionId>,
        message: String,
        at: DateTime<Utc>,
    },
}

impl AssistantEvent {
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::MessageAccepted { .. } => "message_accepted",
            Self::ToolStarted { .. } => "tool_started",
            Self::ToolCompleted { .. } => "tool_completed",
            Self::MessageChunk { .. } => "message_chunk",
            Self::MessageCompleted { .. } => "message_completed",
            Self::Error { .. } => "error",
        }
    }
}
