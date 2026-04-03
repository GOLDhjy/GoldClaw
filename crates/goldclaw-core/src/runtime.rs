use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{
    AssistantEvent, Envelope, GoldClawError, PolicyDecision, Result, RuntimeHealth, SessionMessage,
    SessionSummary, SubmissionReceipt, ToolInvocation, ToolOutput,
};

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn generate(&self, envelope: &Envelope, history: &[SessionMessage]) -> Result<String>;
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

impl From<serde_json::Error> for GoldClawError {
    fn from(value: serde_json::Error) -> Self {
        GoldClawError::InvalidInput(value.to_string())
    }
}
