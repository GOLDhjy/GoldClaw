use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type SessionId = Uuid;
pub type EnvelopeId = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeSource {
    Cli,
    Tui,
    Web,
    Connector(String),
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub id: EnvelopeId,
    pub session_id: Option<SessionId>,
    pub source: EnvelopeSource,
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
            content: content.into(),
            created_at: Utc::now(),
        }
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
