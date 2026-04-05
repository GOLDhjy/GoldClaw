use async_trait::async_trait;
use futures_util::StreamExt;
use goldclaw_core::{
    AssistantEvent, ConversationRef, Envelope, EnvelopeSource, GoldClawError, Result,
    RuntimeHandle, RuntimeHealth, SessionDetail, SessionSummary, SubmissionReceipt,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

/// A `RuntimeHandle` implementation that forwards all calls to a running gateway via HTTP/SSE.
/// This is what connectors (WeCom, Weixin, …) should use so that sessions and messages are
/// persisted by the gateway's `SqliteStore` rather than living only in connector memory.
pub struct GatewayHandle {
    base_url: String,
    client: Client,
}

impl GatewayHandle {
    pub fn new(bind_addr: &str) -> Self {
        Self {
            base_url: format!("http://{bind_addr}"),
            client: Client::new(),
        }
    }
}

// ── request / response types ──────────────────────────────────────────────────

#[derive(Serialize)]
struct CreateSessionRequest {
    title: Option<String>,
}

#[derive(Serialize)]
struct SubmitMessageRequest {
    session_id: Option<Uuid>,
    content: String,
    source: Option<EnvelopeSource>,
    conversation: Option<ConversationRef>,
}

#[derive(Deserialize)]
struct HealthResponse {
    health: RuntimeHealth,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn http_err(err: impl std::fmt::Display) -> GoldClawError {
    GoldClawError::Internal(format!("gateway HTTP error: {err}"))
}

async fn check_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(GoldClawError::Internal(format!(
        "gateway returned {status}: {body}"
    )))
}

// ── RuntimeHandle impl ────────────────────────────────────────────────────────

#[async_trait]
impl RuntimeHandle for GatewayHandle {
    async fn create_session(&self, title: Option<String>) -> Result<SessionSummary> {
        let response = self
            .client
            .post(format!("{}/sessions", self.base_url))
            .json(&CreateSessionRequest { title })
            .send()
            .await
            .map_err(http_err)?;

        check_response(response)
            .await?
            .json::<SessionSummary>()
            .await
            .map_err(http_err)
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let response = self
            .client
            .get(format!("{}/sessions", self.base_url))
            .send()
            .await
            .map_err(http_err)?;

        check_response(response)
            .await?
            .json::<Vec<SessionSummary>>()
            .await
            .map_err(http_err)
    }

    async fn load_session(&self, session_id: Uuid) -> Result<SessionDetail> {
        let response = self
            .client
            .get(format!("{}/sessions/{session_id}", self.base_url))
            .send()
            .await
            .map_err(http_err)?;

        check_response(response)
            .await?
            .json::<SessionDetail>()
            .await
            .map_err(http_err)
    }

    async fn submit(&self, envelope: Envelope) -> Result<SubmissionReceipt> {
        let request = SubmitMessageRequest {
            session_id: envelope.session_id,
            content: envelope.content,
            source: Some(envelope.source),
            conversation: envelope.conversation,
        };

        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .json(&request)
            .send()
            .await
            .map_err(http_err)?;

        check_response(response)
            .await?
            .json::<SubmissionReceipt>()
            .await
            .map_err(http_err)
    }

    async fn subscribe(&self, session_id: Uuid) -> Result<broadcast::Receiver<AssistantEvent>> {
        let url = format!("{}/sessions/{session_id}/events", self.base_url);
        let client = self.client.clone();
        let (tx, rx) = broadcast::channel(64);

        tokio::spawn(async move {
            if let Err(err) = pump_sse(client, url, tx).await {
                warn!("gateway SSE stream ended: {err}");
            }
        });

        Ok(rx)
    }

    async fn health(&self) -> Result<RuntimeHealth> {
        let response = self
            .client
            .get(format!("{}/healthz", self.base_url))
            .send()
            .await
            .map_err(http_err)?;

        check_response(response)
            .await?
            .json::<HealthResponse>()
            .await
            .map(|r| r.health)
            .map_err(http_err)
    }
}

// ── SSE streaming ─────────────────────────────────────────────────────────────

/// Opens an SSE connection and forwards parsed `AssistantEvent`s to `tx`.
/// Returns when the stream closes or all receivers are dropped.
async fn pump_sse(
    client: Client,
    url: String,
    tx: broadcast::Sender<AssistantEvent>,
) -> anyhow::Result<()> {
    let response = client.get(&url).send().await?;
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // SSE events are separated by blank lines (\n\n).
        loop {
            let Some(end) = buffer.find("\n\n") else {
                break;
            };
            let block = buffer[..end].to_string();
            buffer = buffer[end + 2..].to_string();

            // Extract the `data:` line from the event block.
            let data = block
                .lines()
                .find_map(|line| line.strip_prefix("data: ").map(str::to_owned));

            let Some(json) = data else { continue };

            match serde_json::from_str::<AssistantEvent>(&json) {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        return Ok(()); // all receivers dropped, stop pumping
                    }
                }
                Err(err) => {
                    warn!("failed to parse SSE event: {err}");
                }
            }
        }
    }

    Ok(())
}
