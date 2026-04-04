mod error;
mod models;
mod runtime;

pub use error::{GoldClawError, Result};
pub use models::{
    AssistantEvent, ChatMessage, ConversationRef, Envelope, EnvelopeSource, MessageId, MessageRole,
    PolicyDecision, RuntimeHealth, SessionBinding, SessionDetail, SessionId, SessionMessage,
    SessionSummary, SubmissionReceipt, ToolInvocation, ToolOutput,
};
pub use runtime::{Connector, MessageBuilder, Policy, Provider, RuntimeHandle, Tool};
