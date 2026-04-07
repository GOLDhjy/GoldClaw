use async_trait::async_trait;
use goldclaw_core::{
    ChatMessage, EmbeddingProvider, GoldClawError, Provider, ProviderOutput, Result,
    ToolDefinition,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

const CHAT_ENDPOINT: &str =
    "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions";
const EMBEDDING_ENDPOINT: &str =
    "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings";
const DEFAULT_CHAT_MODEL: &str = "qwen-plus";
const EMBEDDING_MODEL: &str = "text-embedding-v4";
pub const EMBEDDING_DIMENSION: usize = 1024;

// ── Shared HTTP client builder ─────────────────────────────────────────────

fn build_client() -> std::result::Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

fn api_key_from_env_or_config(config_key: Option<String>) -> std::result::Result<String, String> {
    std::env::var("DASHSCOPE_API_KEY")
        .ok()
        .or(config_key)
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| "DASHSCOPE_API_KEY is not set".into())
}

// ── Wire types (OpenAI-compatible) ─────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

#[derive(Serialize)]
struct WireMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: WireToolFunction,
}

#[derive(Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: WireCallFunction,
}

#[derive(Serialize, Deserialize, Clone)]
struct WireCallFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

fn to_wire_message(m: &ChatMessage) -> WireMessage {
    WireMessage {
        role: m.role.clone(),
        content: if m.tool_calls.is_empty() {
            Some(m.content.clone())
        } else {
            None
        },
        tool_calls: m
            .tool_calls
            .iter()
            .map(|tc| WireToolCall {
                id: tc.id.clone(),
                call_type: tc.call_type.clone(),
                function: WireCallFunction {
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                },
            })
            .collect(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

fn to_wire_tool(td: &ToolDefinition) -> WireTool {
    WireTool {
        tool_type: "function",
        function: WireToolFunction {
            name: td.name.clone(),
            description: td.description.clone(),
            parameters: td.parameters.clone(),
        },
    }
}

// ── QwenChatProvider ───────────────────────────────────────────────────────

pub struct QwenChatProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl QwenChatProvider {
    pub fn from_env_or_config(
        config_api_key: Option<String>,
        config_model: Option<String>,
    ) -> std::result::Result<Self, String> {
        let api_key = api_key_from_env_or_config(config_api_key)?;
        let model = std::env::var("DASHSCOPE_MODEL")
            .ok()
            .or(config_model)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string());
        Ok(Self {
            client: build_client()?,
            api_key,
            model,
        })
    }
}

#[async_trait]
impl Provider for QwenChatProvider {
    fn name(&self) -> &'static str {
        "qwen"
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
    ) -> Result<ProviderOutput> {
        if messages.is_empty() {
            return Err(GoldClawError::InvalidInput(
                "no messages to send to Qwen".into(),
            ));
        }

        debug!(
            model = %self.model,
            turns = messages.len(),
            tool_count = tools.len(),
            "calling Qwen chat API"
        );

        let body = ChatRequest {
            model: &self.model,
            messages: messages.iter().map(to_wire_message).collect(),
            tools: tools.iter().map(to_wire_tool).collect(),
        };

        let resp = self
            .client
            .post(CHAT_ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| GoldClawError::Internal(format!("Qwen chat request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(GoldClawError::Internal(format!(
                "Qwen chat API error {status}: {text}"
            )));
        }

        let parsed: ChatResponse = resp.json().await.map_err(|e| {
            GoldClawError::Internal(format!("failed to parse Qwen chat response: {e}"))
        })?;

        let msg = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| GoldClawError::Internal("Qwen returned no choices".into()))?;

        if let Some(mut calls) = msg.tool_calls.filter(|v| !v.is_empty()) {
            let call = calls.remove(0);
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            return Ok(ProviderOutput::ToolCall {
                id: call.id,
                name: call.function.name,
                args,
            });
        }

        msg.content
            .filter(|s| !s.is_empty())
            .map(ProviderOutput::Text)
            .ok_or_else(|| GoldClawError::Internal("Qwen returned empty content".into()))
    }
}

// ── QwenEmbeddingProvider ──────────────────────────────────────────────────

pub struct QwenEmbeddingProvider {
    client: reqwest::Client,
    api_key: String,
}

impl QwenEmbeddingProvider {
    pub fn from_env_or_config(
        config_api_key: Option<String>,
    ) -> std::result::Result<Self, String> {
        let api_key = api_key_from_env_or_config(config_api_key)?;
        Ok(Self {
            client: build_client()?,
            api_key,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for QwenEmbeddingProvider {
    fn dimension(&self) -> usize {
        EMBEDDING_DIMENSION
    }

    fn model_name(&self) -> &str {
        EMBEDDING_MODEL
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        debug!(model = EMBEDDING_MODEL, "calling DashScope embedding API");

        let resp = self
            .client
            .post(EMBEDDING_ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&serde_json::json!({
                "model": EMBEDDING_MODEL,
                "input": text,
                "encoding_format": "float"
            }))
            .send()
            .await
            .map_err(|e| {
                GoldClawError::Internal(format!("DashScope embedding request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(GoldClawError::Internal(format!(
                "DashScope embedding API error {status}: {body}"
            )));
        }

        let parsed: EmbeddingResponse = resp.json().await.map_err(|e| {
            GoldClawError::Internal(format!(
                "failed to parse DashScope embedding response: {e}"
            ))
        })?;

        parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| GoldClawError::Internal("DashScope returned empty embedding".into()))
    }
}
