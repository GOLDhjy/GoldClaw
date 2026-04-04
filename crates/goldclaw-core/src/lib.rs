mod error;
mod models;
mod runtime;

#[cfg(test)]
mod tests;

pub use error::{GoldClawError, Result};
pub use models::{
    AssistantEvent, ChatMessage, ConversationRef, Envelope, EnvelopeSource, MemoryChunk,
    MemoryChunkId, MessageId, MessageRole, PolicyDecision, RuntimeHealth, SessionBinding,
    SessionDetail, SessionId, SessionMessage, SessionSummary, SubmissionReceipt, ToolInvocation,
    ToolOutput,
};
pub use runtime::{
    Connector, EmbeddingProvider, MemoryQuery, MemoryStore, MessageBuilder, Policy, Provider,
    RuntimeHandle, Tool,
};
