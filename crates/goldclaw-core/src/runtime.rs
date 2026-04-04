use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{
    AssistantEvent, ChatMessage, Envelope, GoldClawError, MemoryChunk, PolicyDecision, Result,
    RuntimeHealth, SessionDetail, SessionMessage, SessionSummary, SubmissionReceipt, ToolInvocation,
    ToolOutput,
};

pub trait MessageBuilder: Send + Sync {
    fn build(&self, history: &[SessionMessage]) -> Vec<ChatMessage>;
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn chat(&self, messages: &[ChatMessage]) -> Result<String>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput>;
}

#[async_trait]
pub trait Policy: Send + Sync {
    async fn authorize(&self, invocation: &ToolInvocation) -> Result<PolicyDecision>;
}

#[async_trait]
pub trait RuntimeHandle: Send + Sync {
    async fn create_session(&self, title: Option<String>) -> Result<SessionSummary>;
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    async fn load_session(&self, session_id: uuid::Uuid) -> Result<SessionDetail>;
    async fn submit(&self, envelope: Envelope) -> Result<SubmissionReceipt>;
    async fn subscribe(
        &self,
        session_id: uuid::Uuid,
    ) -> Result<broadcast::Receiver<AssistantEvent>>;
    async fn health(&self) -> Result<RuntimeHealth>;
}

/// A connector bridges an external input source (stdin, Slack, Feishu, …) to
/// the runtime. Implement this trait and call [`Connector::run`] to start
/// forwarding messages as [`Envelope`]s.
#[async_trait]
pub trait Connector: Send {
    fn name(&self) -> &'static str;
    async fn run(self: Box<Self>, runtime: Arc<dyn RuntimeHandle>) -> Result<()>;
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn dimension(&self) -> usize;
    fn model_name(&self) -> &str;
}

pub struct MemoryQuery {
    pub text: String,
    pub embedding: Option<Vec<f32>>,
    pub limit: usize,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn save_chunk(&self, chunk: MemoryChunk) -> Result<()>;
    async fn recall(&self, query: MemoryQuery) -> Result<Vec<MemoryChunk>>;
}

impl From<serde_json::Error> for GoldClawError {
    fn from(value: serde_json::Error) -> Self {
        GoldClawError::InvalidInput(value.to_string())
    }
}
