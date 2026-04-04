use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use goldclaw_core::{
    AssistantEvent, Connector, ConversationRef, Envelope, EnvelopeSource, GoldClawError, Result,
    RuntimeHandle,
};
use reqwest::{
    Client,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub const DEFAULT_WEIXIN_API_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const DEFAULT_BOT_TYPE: &str = "3";
const MESSAGE_TYPE_BOT: i32 = 2;
const MESSAGE_STATE_FINISH: i32 = 2;
const MESSAGE_ITEM_TYPE_TEXT: i32 = 1;
const MESSAGE_ITEM_TYPE_VOICE: i32 = 3;

#[derive(Clone, Debug)]
pub struct WeixinConnectorConfig {
    pub state_dir: PathBuf,
    pub api_base_url: String,
    pub bot_type: String,
    pub login_timeout: Duration,
    pub poll_timeout: Duration,
    pub reply_timeout: Duration,
    pub retry_delay: Duration,
}

impl WeixinConnectorConfig {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            api_base_url: DEFAULT_WEIXIN_API_BASE_URL.into(),
            bot_type: DEFAULT_BOT_TYPE.into(),
            login_timeout: Duration::from_secs(180),
            poll_timeout: Duration::from_secs(35),
            reply_timeout: Duration::from_secs(90),
            retry_delay: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeixinAccount {
    pub account_id: String,
    pub bot_token: String,
    pub user_id: Option<String>,
    pub api_base_url: String,
    pub saved_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeixinLoginTicket {
    pub qr_code: String,
    pub qr_code_url: String,
}

#[derive(Clone, Debug)]
pub struct WeixinConnector {
    config: WeixinConnectorConfig,
    state: WeixinStateStore,
    client: WeixinApiClient,
}

impl WeixinConnector {
    pub fn new(config: WeixinConnectorConfig) -> Self {
        let state = WeixinStateStore::new(config.state_dir.clone());
        let client = WeixinApiClient::new(config.api_base_url.clone());
        Self {
            config,
            state,
            client,
        }
    }

    pub fn state_dir(&self) -> &Path {
        self.state.root()
    }

    pub fn load_account(&self) -> Result<Option<WeixinAccount>> {
        self.state.load_account()
    }

    pub async fn login(&self, force: bool) -> Result<WeixinAccount> {
        if !force {
            if let Some(existing) = self.state.load_account()? {
                info!(
                    account_id = %existing.account_id,
                    "reusing saved weixin account"
                );
                return Ok(existing);
            }
        }

        let ticket = self.client.start_login(&self.config.bot_type).await?;
        println!("请使用微信扫描以下二维码链接完成授权：");
        println!("{}", ticket.qr_code_url);

        let account = self
            .client
            .wait_for_login(&ticket, self.config.login_timeout)
            .await?;
        self.state.save_account(&account)?;
        println!("微信登录成功，账号: {}", account.account_id);
        Ok(account)
    }

    pub async fn ensure_logged_in(&self) -> Result<WeixinAccount> {
        self.login(false).await
    }

    pub async fn run_once(
        &self,
        runtime: Arc<dyn RuntimeHandle>,
        account: &WeixinAccount,
    ) -> Result<usize> {
        let cursor = self.state.load_cursor()?;
        let response = self
            .client
            .get_updates(account, cursor.as_deref(), self.config.poll_timeout)
            .await?;

        if let Some(next_cursor) = response.get_updates_buf.as_deref() {
            self.state.save_cursor(next_cursor)?;
        }

        let mut handled = 0usize;
        for message in response.msgs.unwrap_or_default() {
            if self
                .handle_message(runtime.clone(), account, message)
                .await?
            {
                handled += 1;
            }
        }

        Ok(handled)
    }

    async fn handle_message(
        &self,
        runtime: Arc<dyn RuntimeHandle>,
        account: &WeixinAccount,
        message: WeixinMessage,
    ) -> Result<bool> {
        if message.message_type == Some(MESSAGE_TYPE_BOT) {
            return Ok(false);
        }

        let from_user_id = message.from_user_id.clone().unwrap_or_default();
        if from_user_id.is_empty() {
            return Ok(false);
        }

        if account.user_id.as_deref() == Some(from_user_id.as_str()) {
            return Ok(false);
        }

        let Some(content) = extract_text(&message.item_list.unwrap_or_default()) else {
            return Ok(false);
        };
        let content = content.trim().to_string();
        if content.is_empty() {
            return Ok(false);
        }

        let mut envelope =
            Envelope::user(content, EnvelopeSource::Connector("weixin".into()), None);
        envelope.conversation = Some(ConversationRef {
            source_instance: Some(account.account_id.clone()),
            conversation_id: format!("dm:{from_user_id}"),
            sender_id: Some(from_user_id.clone()),
            external_message_id: message.message_id.map(|value| value.to_string()),
        });

        let receipt = runtime.submit(envelope).await?;
        let reply = wait_for_assistant_reply(
            runtime.clone(),
            receipt.session_id,
            receipt.accepted_at,
            self.config.reply_timeout,
        )
        .await?;

        self.client
            .send_text(
                account,
                &from_user_id,
                message.context_token.as_deref(),
                &reply,
            )
            .await?;

        Ok(true)
    }

    pub async fn run_forever(&self, runtime: Arc<dyn RuntimeHandle>) -> Result<()> {
        let account = self.ensure_logged_in().await?;
        info!(account_id = %account.account_id, "weixin connector started");

        loop {
            match self.run_once(runtime.clone(), &account).await {
                Ok(_) => {}
                Err(error) => {
                    warn!("weixin connector poll failed: {error}");
                    sleep(self.config.retry_delay).await;
                }
            }
        }
    }
}

#[async_trait]
impl Connector for WeixinConnector {
    fn name(&self) -> &'static str {
        "weixin"
    }

    async fn run(self: Box<Self>, runtime: Arc<dyn RuntimeHandle>) -> Result<()> {
        self.run_forever(runtime).await
    }
}

async fn wait_for_assistant_reply(
    runtime: Arc<dyn RuntimeHandle>,
    session_id: Uuid,
    accepted_at: DateTime<Utc>,
    reply_timeout: Duration,
) -> Result<String> {
    if let Some(reply) = latest_provider_reply(runtime.clone(), session_id, accepted_at).await? {
        return Ok(reply);
    }

    let mut receiver = runtime.subscribe(session_id).await?;
    let deadline = Instant::now() + reply_timeout;

    loop {
        if let Some(reply) = latest_provider_reply(runtime.clone(), session_id, accepted_at).await?
        {
            return Ok(reply);
        }

        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };

        match timeout(remaining, receiver.recv()).await {
            Ok(Ok(AssistantEvent::MessageCompleted { content, .. })) => return Ok(content),
            Ok(Ok(AssistantEvent::Error { message, .. })) => {
                return Err(GoldClawError::Internal(message));
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                return Err(GoldClawError::Internal(format!(
                    "weixin reply stream failed: {error}"
                )));
            }
            Err(_) => break,
        }
    }

    latest_provider_reply(runtime, session_id, accepted_at)
        .await?
        .ok_or_else(|| GoldClawError::Internal("assistant reply timed out".into()))
}

async fn latest_provider_reply(
    runtime: Arc<dyn RuntimeHandle>,
    session_id: Uuid,
    accepted_at: DateTime<Utc>,
) -> Result<Option<String>> {
    let detail = runtime.load_session(session_id).await?;
    Ok(detail
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == goldclaw_core::MessageRole::Assistant
                && message.created_at >= accepted_at
                && message
                    .metadata
                    .get("kind")
                    .and_then(|value| value.as_str())
                    == Some("provider_response")
        })
        .map(|message| message.content.clone()))
}

fn extract_text(items: &[MessageItem]) -> Option<String> {
    let text_parts: Vec<String> = items
        .iter()
        .filter_map(|item| match item.item_type {
            Some(MESSAGE_ITEM_TYPE_TEXT) => item.text_item.as_ref()?.text.clone(),
            Some(MESSAGE_ITEM_TYPE_VOICE) => item.voice_item.as_ref()?.text.clone(),
            _ => None,
        })
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect();

    if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    }
}

#[derive(Clone, Debug)]
struct WeixinStateStore {
    root: PathBuf,
}

impl WeixinStateStore {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_parent_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(io_error)
    }

    fn account_path(&self) -> PathBuf {
        self.root.join("account.json")
    }

    fn cursor_path(&self) -> PathBuf {
        self.root.join("sync_cursor.txt")
    }

    fn load_account(&self) -> Result<Option<WeixinAccount>> {
        match fs::read_to_string(self.account_path()) {
            Ok(raw) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|error| GoldClawError::InvalidInput(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }

    fn save_account(&self, account: &WeixinAccount) -> Result<()> {
        self.ensure_parent_dir()?;
        let raw = serde_json::to_string_pretty(account)
            .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;
        fs::write(self.account_path(), raw).map_err(io_error)
    }

    fn load_cursor(&self) -> Result<Option<String>> {
        match fs::read_to_string(self.cursor_path()) {
            Ok(raw) => {
                let cursor = raw.trim().to_string();
                if cursor.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(cursor))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }

    fn save_cursor(&self, cursor: &str) -> Result<()> {
        self.ensure_parent_dir()?;
        fs::write(self.cursor_path(), cursor).map_err(io_error)
    }
}

#[derive(Clone, Debug)]
struct WeixinApiClient {
    base_url: String,
    http: Client,
}

impl WeixinApiClient {
    fn new(base_url: String) -> Self {
        Self {
            base_url: trim_base_url(&base_url),
            http: Client::new(),
        }
    }

    async fn start_login(&self, bot_type: &str) -> Result<WeixinLoginTicket> {
        let response = self
            .http
            .get(format!("{}/ilink/bot/get_bot_qrcode", self.base_url))
            .query(&[("bot_type", bot_type)])
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json::<QrCodeResponse>()
            .await
            .map_err(http_error)?;

        Ok(WeixinLoginTicket {
            qr_code: response.qrcode,
            qr_code_url: response.qrcode_img_content,
        })
    }

    async fn wait_for_login(
        &self,
        ticket: &WeixinLoginTicket,
        login_timeout: Duration,
    ) -> Result<WeixinAccount> {
        let deadline = Instant::now() + login_timeout;

        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(GoldClawError::Internal(
                    "weixin login timed out while waiting for QR approval".into(),
                ));
            };

            let response = self
                .http
                .get(format!("{}/ilink/bot/get_qrcode_status", self.base_url))
                .query(&[("qrcode", ticket.qr_code.as_str())])
                .timeout(remaining.min(Duration::from_secs(35)))
                .send()
                .await
                .map_err(http_error)?
                .error_for_status()
                .map_err(http_error)?
                .json::<QrStatusResponse>()
                .await
                .map_err(http_error)?;

            match response.status.as_str() {
                "wait" => {}
                "scaned" => println!("已扫码，请在微信里确认授权..."),
                "confirmed" => {
                    let bot_token = response.bot_token.ok_or_else(|| {
                        GoldClawError::Internal("weixin login confirmed without bot_token".into())
                    })?;
                    let account_id = response.ilink_bot_id.ok_or_else(|| {
                        GoldClawError::Internal(
                            "weixin login confirmed without ilink_bot_id".into(),
                        )
                    })?;

                    return Ok(WeixinAccount {
                        account_id,
                        bot_token,
                        user_id: response.ilink_user_id,
                        api_base_url: trim_base_url(
                            response
                                .baseurl
                                .as_deref()
                                .unwrap_or(self.base_url.as_str()),
                        ),
                        saved_at: Utc::now(),
                    });
                }
                "expired" => {
                    return Err(GoldClawError::Internal(
                        "weixin login QR code expired; please rerun login".into(),
                    ));
                }
                other => debug!("weixin login status: {other}"),
            }

            sleep(Duration::from_millis(750)).await;
        }
    }

    async fn get_updates(
        &self,
        account: &WeixinAccount,
        cursor: Option<&str>,
        poll_timeout: Duration,
    ) -> Result<GetUpdatesResponse> {
        let payload = GetUpdatesRequest {
            get_updates_buf: cursor.unwrap_or_default().to_string(),
            base_info: BaseInfo {
                channel_version: env!("CARGO_PKG_VERSION").into(),
            },
        };

        self.http
            .post(format!("{}/ilink/bot/getupdates", account.api_base_url))
            .headers(build_authenticated_headers(&account.bot_token)?)
            .timeout(poll_timeout)
            .json(&payload)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json::<GetUpdatesResponse>()
            .await
            .map_err(http_error)
    }

    async fn send_text(
        &self,
        account: &WeixinAccount,
        to_user_id: &str,
        context_token: Option<&str>,
        text: &str,
    ) -> Result<()> {
        let payload = build_text_message_request(to_user_id, context_token, text);

        self.http
            .post(format!("{}/ilink/bot/sendmessage", account.api_base_url))
            .headers(build_authenticated_headers(&account.bot_token)?)
            .json(&payload)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;

        Ok(())
    }
}

fn build_text_message_request(
    to_user_id: &str,
    context_token: Option<&str>,
    text: &str,
) -> SendMessageRequest {
    SendMessageRequest {
        msg: OutboundMessage {
            from_user_id: String::new(),
            to_user_id: to_user_id.to_string(),
            client_id: Uuid::new_v4().to_string(),
            message_type: MESSAGE_TYPE_BOT,
            message_state: MESSAGE_STATE_FINISH,
            item_list: vec![OutboundItem {
                item_type: MESSAGE_ITEM_TYPE_TEXT,
                text_item: OutboundTextItem {
                    text: text.to_string(),
                },
            }],
            context_token: context_token.map(ToOwned::to_owned),
        },
    }
}

fn trim_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

fn build_authenticated_headers(bot_token: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "AuthorizationType",
        HeaderValue::from_static("ilink_bot_token"),
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", bot_token.trim())).map_err(|error| {
            GoldClawError::InvalidInput(format!("invalid weixin bot token header: {error}"))
        })?,
    );
    headers.insert(
        "X-WECHAT-UIN",
        HeaderValue::from_str(&build_wechat_uin())
            .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?,
    );
    Ok(headers)
}

fn build_wechat_uin() -> String {
    let raw = format!(
        "{}:{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    STANDARD.encode(raw)
}

fn io_error(error: std::io::Error) -> GoldClawError {
    GoldClawError::Io(error.to_string())
}

fn http_error(error: reqwest::Error) -> GoldClawError {
    GoldClawError::Io(error.to_string())
}

#[derive(Debug, Serialize)]
struct BaseInfo {
    channel_version: String,
}

#[derive(Debug, Serialize)]
struct GetUpdatesRequest {
    get_updates_buf: String,
    base_info: BaseInfo,
}

#[derive(Clone, Debug, Deserialize)]
struct QrCodeResponse {
    qrcode: String,
    qrcode_img_content: String,
}

#[derive(Clone, Debug, Deserialize)]
struct QrStatusResponse {
    status: String,
    bot_token: Option<String>,
    ilink_bot_id: Option<String>,
    ilink_user_id: Option<String>,
    baseurl: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct GetUpdatesResponse {
    #[allow(dead_code)]
    ret: Option<i32>,
    msgs: Option<Vec<WeixinMessage>>,
    get_updates_buf: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct WeixinMessage {
    message_id: Option<i64>,
    from_user_id: Option<String>,
    #[allow(dead_code)]
    to_user_id: Option<String>,
    #[allow(dead_code)]
    create_time_ms: Option<i64>,
    message_type: Option<i32>,
    item_list: Option<Vec<MessageItem>>,
    context_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct MessageItem {
    #[serde(rename = "type")]
    item_type: Option<i32>,
    text_item: Option<TextItem>,
    voice_item: Option<VoiceItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct TextItem {
    text: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct VoiceItem {
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    msg: OutboundMessage,
}

#[derive(Debug, Serialize)]
struct OutboundMessage {
    from_user_id: String,
    to_user_id: String,
    client_id: String,
    message_type: i32,
    message_state: i32,
    item_list: Vec<OutboundItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutboundItem {
    #[serde(rename = "type")]
    item_type: i32,
    text_item: OutboundTextItem,
}

#[derive(Debug, Serialize)]
struct OutboundTextItem {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use goldclaw_core::{
        MessageRole, RuntimeHealth, SessionDetail, SessionMessage, SessionSummary,
        SubmissionReceipt,
    };
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };
    use tokio::sync::broadcast;

    #[test]
    fn extract_text_reads_text_and_voice_items() {
        let items = vec![
            MessageItem {
                item_type: Some(MESSAGE_ITEM_TYPE_TEXT),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                voice_item: None,
            },
            MessageItem {
                item_type: Some(MESSAGE_ITEM_TYPE_VOICE),
                text_item: None,
                voice_item: Some(VoiceItem {
                    text: Some("world".into()),
                }),
            },
        ];

        assert_eq!(extract_text(&items).as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn state_store_round_trips_account() {
        let temp_dir = temp_path("store");
        let store = WeixinStateStore::new(temp_dir.clone());
        let account = WeixinAccount {
            account_id: "bot-main@im.bot".into(),
            bot_token: "token-1".into(),
            user_id: Some("bot-user".into()),
            api_base_url: DEFAULT_WEIXIN_API_BASE_URL.into(),
            saved_at: Utc::now(),
        };

        store.save_account(&account).expect("save account");
        assert_eq!(store.load_account().expect("load account"), Some(account));
    }

    #[test]
    fn text_message_request_contains_context_token() {
        let payload = build_text_message_request("alice@im.wechat", Some("ctx-1"), "reply: ping");
        let json = serde_json::to_value(payload).expect("serialize");

        assert_eq!(json["msg"]["to_user_id"], "alice@im.wechat");
        assert_eq!(json["msg"]["context_token"], "ctx-1");
        assert_eq!(json["msg"]["item_list"][0]["type"], MESSAGE_ITEM_TYPE_TEXT);
        assert_eq!(
            json["msg"]["item_list"][0]["text_item"]["text"],
            "reply: ping"
        );
    }

    #[tokio::test]
    async fn wait_for_assistant_reply_falls_back_to_session_history() {
        let runtime = Arc::new(MockRuntime::default());
        let accepted_at = Utc::now();
        let session_id = runtime.seed_reply("reply: ping", accepted_at);

        let reply =
            wait_for_assistant_reply(runtime, session_id, accepted_at, Duration::from_secs(1))
                .await
                .expect("reply");

        assert_eq!(reply, "reply: ping");
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("goldclaw-weixin-{label}-{}", Uuid::new_v4()))
    }

    #[derive(Default)]
    struct MockRuntime {
        sessions: Mutex<HashMap<Uuid, SessionDetail>>,
        channels: OnceLock<Mutex<HashMap<Uuid, broadcast::Sender<AssistantEvent>>>>,
    }

    impl MockRuntime {
        fn channel(&self, session_id: Uuid) -> broadcast::Sender<AssistantEvent> {
            let channels = self.channels.get_or_init(|| Mutex::new(HashMap::new()));
            let mut guard = channels.lock().unwrap();
            guard
                .entry(session_id)
                .or_insert_with(|| {
                    let (sender, _) = broadcast::channel(16);
                    sender
                })
                .clone()
        }

        fn seed_reply(&self, content: &str, accepted_at: DateTime<Utc>) -> Uuid {
            let session_id = Uuid::new_v4();
            let reply = SessionMessage {
                id: Uuid::new_v4(),
                session_id,
                role: MessageRole::Assistant,
                source: EnvelopeSource::Connector("weixin".into()),
                content: content.into(),
                metadata: serde_json::json!({ "kind": "provider_response" }),
                created_at: accepted_at,
            };
            self.sessions.lock().unwrap().insert(
                session_id,
                SessionDetail {
                    session: SessionSummary {
                        id: session_id,
                        title: "mock".into(),
                        created_at: accepted_at,
                        updated_at: accepted_at,
                    },
                    messages: vec![reply],
                },
            );
            session_id
        }
    }

    #[async_trait]
    impl RuntimeHandle for MockRuntime {
        async fn create_session(&self, title: Option<String>) -> Result<SessionSummary> {
            let session = SessionSummary {
                id: Uuid::new_v4(),
                title: title.unwrap_or_else(|| "mock".into()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            self.sessions.lock().unwrap().insert(
                session.id,
                SessionDetail {
                    session: session.clone(),
                    messages: Vec::new(),
                },
            );
            Ok(session)
        }

        async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .values()
                .map(|detail| detail.session.clone())
                .collect())
        }

        async fn load_session(&self, session_id: Uuid) -> Result<SessionDetail> {
            self.sessions
                .lock()
                .unwrap()
                .get(&session_id)
                .cloned()
                .ok_or_else(|| GoldClawError::NotFound(format!("session `{session_id}`")))
        }

        async fn submit(&self, envelope: Envelope) -> Result<SubmissionReceipt> {
            let accepted_at = Utc::now();
            let session_id = Uuid::new_v4();
            let reply = SessionMessage {
                id: Uuid::new_v4(),
                session_id,
                role: MessageRole::Assistant,
                source: EnvelopeSource::Connector("weixin".into()),
                content: format!("reply: {}", envelope.content),
                metadata: serde_json::json!({ "kind": "provider_response" }),
                created_at: Utc::now(),
            };

            self.sessions.lock().unwrap().insert(
                session_id,
                SessionDetail {
                    session: SessionSummary {
                        id: session_id,
                        title: "mock".into(),
                        created_at: accepted_at,
                        updated_at: Utc::now(),
                    },
                    messages: vec![reply.clone()],
                },
            );

            let _ = self
                .channel(session_id)
                .send(AssistantEvent::MessageCompleted {
                    session_id,
                    content: reply.content.clone(),
                    at: Utc::now(),
                });

            Ok(SubmissionReceipt {
                session_id,
                envelope_id: envelope.id,
                accepted_at,
            })
        }

        async fn subscribe(&self, session_id: Uuid) -> Result<broadcast::Receiver<AssistantEvent>> {
            Ok(self.channel(session_id).subscribe())
        }

        async fn health(&self) -> Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                healthy: true,
                provider: "mock".into(),
                session_count: self.sessions.lock().unwrap().len(),
            })
        }
    }
}
