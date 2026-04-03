use async_trait::async_trait;
use goldclaw_core::{Envelope, GoldClawError, MessageRole, Provider, Result, SessionMessage};
use serde::{Deserialize, Serialize};
use tracing::debug;

const DEFAULT_MODEL: &str = "GLM-5.1";
const DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/coding/paas/v4";

pub struct GlmProvider {
    client: reqwest::Client,
    model: String,
    api_key: String,
    system_prompt: Option<String>,
}

impl GlmProvider {
    /// Build from environment variables, with optional fallbacks from config.
    ///
    /// Priority:
    ///   - `BIGMODEL_API_KEY` env var → `config_api_key` argument
    ///   - `BIGMODEL_MODEL` / `BIGMODEL_CODING_MODEL` env var → `config_model` argument
    ///
    /// Other env vars: `BIGMODEL_BASE_URL`, `BIGMODEL_CODING_BASE_URL`,
    ///                 `HTTP_PROXY`, `API_TIMEOUT_MS`
    pub fn from_env_or_config(
        config_api_key: Option<String>,
        config_model: Option<String>,
        system_prompt: Option<String>,
    ) -> std::result::Result<Self, String> {
        let api_key = std::env::var("BIGMODEL_API_KEY")
            .ok()
            .or(config_api_key)
            .filter(|k| !k.trim().is_empty())
            .ok_or("BIGMODEL_API_KEY is not set")?;

        let model = std::env::var("BIGMODEL_MODEL")
            .or_else(|_| std::env::var("BIGMODEL_CODING_MODEL"))
            .ok()
            .or(config_model)
            .and_then(|m| normalize_glm_model(&m))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let client =
            build_http_client().map_err(|e| format!("failed to build HTTP client: {e}"))?;

        Ok(Self { client, model, api_key, system_prompt })
    }
}

fn build_http_client() -> std::result::Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder();

    if let Ok(proxy_url) = std::env::var("HTTP_PROXY") {
        if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
            builder = builder.proxy(proxy);
        }
    }

    if let Ok(ms) = std::env::var("API_TIMEOUT_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            builder = builder
                .timeout(std::time::Duration::from_millis(ms))
                .connect_timeout(std::time::Duration::from_secs(10));
        }
    }

    builder.build()
}

fn base_url_from_env() -> String {
    if let Ok(v) = std::env::var("BIGMODEL_BASE_URL") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var("BIGMODEL_CODING_BASE_URL") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    DEFAULT_BASE_URL.to_string()
}

fn normalize_glm_model(model: &str) -> Option<String> {
    match model.trim() {
        "glm-5.1" | "GLM-5.1" => Some("GLM-5.1".to_string()),
        "glm-5" | "GLM-5" => Some("GLM-5".to_string()),
        "glm-5v-turbo" | "GLM-5V-TURBO" | "GLM-5v-Turbo" => Some("glm-5v-turbo".to_string()),
        _ => None,
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    messages: Vec<ApiMessage>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiChoiceMessage,
}

#[derive(Deserialize)]
struct ApiChoiceMessage {
    content: Option<String>,
}

// ── Provider implementation ───────────────────────────────────────────────────

#[async_trait]
impl Provider for GlmProvider {
    fn name(&self) -> &'static str {
        "glm"
    }

    async fn generate(&self, _envelope: &Envelope, history: &[SessionMessage]) -> Result<String> {
        let messages = build_messages(history, self.system_prompt.as_deref());
        if messages.is_empty() {
            return Err(GoldClawError::InvalidInput(
                "no messages to send to GLM".into(),
            ));
        }

        let base_url = base_url_from_env();
        debug!(model = %self.model, base_url = %base_url, turns = messages.len(), "calling GLM API");

        let body = ApiRequest {
            model: &self.model,
            messages,
        };

        let resp = self
            .client
            .post(format!("{base_url}/chat/completions"))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| GoldClawError::Internal(format!("GLM request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(GoldClawError::Internal(format!(
                "GLM API error {status}: {text}"
            )));
        }

        let parsed: ApiResponse = resp
            .json()
            .await
            .map_err(|e| GoldClawError::Internal(format!("failed to parse GLM response: {e}")))?;

        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GoldClawError::Internal("GLM returned empty content".into()))
    }
}

// ── Convert session history to GLM messages ───────────────────────────────────

fn build_messages(history: &[SessionMessage], system_prompt: Option<&str>) -> Vec<ApiMessage> {
    let mut messages: Vec<ApiMessage> = Vec::new();

    // Inject system prompt if the history doesn't already start with one.
    if let Some(prompt) = system_prompt {
        let has_system = history.iter().any(|m| m.role == MessageRole::System);
        if !has_system {
            messages.push(ApiMessage {
                role: "system",
                content: prompt.to_string(),
            });
        }
    }

    for msg in history {
        let (role, content): (&'static str, String) = match msg.role {
            MessageRole::System => ("system", msg.content.clone()),
            MessageRole::User => ("user", msg.content.clone()),
            MessageRole::Assistant => ("assistant", msg.content.clone()),
            MessageRole::Tool => ("assistant", format!("[tool output]\n{}", msg.content)),
        };

        // Merge consecutive same-role messages (GLM requires alternating user/assistant)
        if let Some(last) = messages.last_mut() {
            if last.role == role {
                last.content.push('\n');
                last.content.push_str(&content);
                continue;
            }
        }

        messages.push(ApiMessage { role, content });
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url_is_coding_endpoint() {
        assert_eq!(DEFAULT_BASE_URL, "https://open.bigmodel.cn/api/coding/paas/v4");
    }

    #[test]
    fn normalize_glm_model_maps_aliases() {
        assert_eq!(normalize_glm_model("glm-5.1"), Some("GLM-5.1".to_string()));
        assert_eq!(normalize_glm_model("GLM-5.1"), Some("GLM-5.1".to_string()));
        assert_eq!(normalize_glm_model("glm-5"), Some("GLM-5".to_string()));
        assert_eq!(normalize_glm_model("glm-5v-turbo"), Some("glm-5v-turbo".to_string()));
        assert_eq!(normalize_glm_model("unknown"), None);
    }

    #[test]
    fn default_model_is_glm_5_1() {
        assert_eq!(DEFAULT_MODEL, "GLM-5.1");
    }
}
