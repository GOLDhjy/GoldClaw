mod error;
mod models;
mod runtime;

#[cfg(test)]
mod tests;

pub use error::{GoldClawError, Result};
pub use models::{
    AssistantEvent, ChatFunction, ChatMessage, ChatToolCall, ConversationRef, Envelope,
    EnvelopeSource, MemoryChunk, MemoryChunkId, MessageId, MessageRole, PolicyDecision,
    ProviderOutput, RuntimeHealth, SessionBinding, SessionDetail, SessionId, SessionMessage,
    SessionSummary, SubmissionReceipt, ToolDefinition, ToolInvocation, ToolOutput,
};
pub use runtime::{
    Connector, EmbeddingProvider, MemoryQuery, MemoryStore, MessageBuilder, Policy, Provider,
    RuntimeHandle, Tool,
};
