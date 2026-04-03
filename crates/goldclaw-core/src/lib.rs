mod error;
mod models;
mod runtime;

pub use error::{GoldClawError, Result};
pub use models::{
    AssistantEvent, ConversationRef, Envelope, EnvelopeSource, MessageId, MessageRole,
    PolicyDecision, RuntimeHealth, SessionBinding, SessionId, SessionMessage, SessionSummary,
    SubmissionReceipt, ToolInvocation, ToolOutput,
};
pub use runtime::{Policy, Provider, RuntimeHandle, Tool};
