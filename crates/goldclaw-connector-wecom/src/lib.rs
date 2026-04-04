use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use goldclaw_core::{
    AssistantEvent, Connector, ConversationRef, Envelope, EnvelopeSource, GoldClawError, Result,
    RuntimeHandle,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, MissedTickBehavior, interval, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message},
};
use tracing::{info, warn};
use uuid::Uuid;

pub const DEFAULT_WECOM_WS_URL: &str = "wss://openws.work.weixin.qq.com";

const CMD_SUBSCRIBE: &str = "aibot_subscribe";
const CMD_PING: &str = "ping";
const CMD_MESSAGE_CALLBACK: &str = "aibot_msg_callback";
const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";
const CMD_REPLY: &str = "aibot_respond_msg";
const EVENT_DISCONNECTED: &str = "disconnected_event";
const MAX_REPLY_BYTES: usize = 20 * 1024;

type WsWriter = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

#[derive(Clone, Debug)]
pub struct WeComConnectorConfig {
    pub bot_id: String,
    pub secret: String,
    pub ws_url: String,
    pub heartbeat_interval: Duration,
    pub reconnect_delay: Duration,
    pub reconnect_max_delay: Duration,
    pub auth_retry_delay: Duration,
    pub auth_retry_max_delay: Duration,
    pub auth_timeout: Duration,
    pub reply_timeout: Duration,
    pub max_reconnect_attempts: i32,
    pub max_auth_failure_attempts: i32,
    pub scene: Option<u32>,
    pub plug_version: Option<String>,
}

impl WeComConnectorConfig {
    pub fn new(bot_id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            bot_id: bot_id.into(),
            secret: secret.into(),
            ws_url: DEFAULT_WECOM_WS_URL.into(),
            heartbeat_interval: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(30),
            auth_retry_delay: Duration::from_secs(1),
            auth_retry_max_delay: Duration::from_secs(30),
            auth_timeout: Duration::from_secs(10),
            reply_timeout: Duration::from_secs(90),
            max_reconnect_attempts: 10,
            max_auth_failure_attempts: 5,
            scene: None,
            plug_version: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WeComConnector {
    config: WeComConnectorConfig,
}

impl WeComConnector {
    pub fn new(config: WeComConnectorConfig) -> Self {
        Self { config }
    }

    async fn run_forever(&self, runtime: Arc<dyn RuntimeHandle>) -> Result<()> {
        let mut reconnect_attempts = 0u32;
        let mut auth_failure_attempts = 0u32;

        loop {
            match self.run_connection(runtime.clone()).await {
                Ok(()) => {
                    reconnect_attempts = 0;
                    auth_failure_attempts = 0;
                    info!("wecom connector disconnected cleanly; reconnecting");
                    tokio::time::sleep(self.config.reconnect_delay).await;
                }
                Err(error) => {
                    warn!("wecom connector connection failed: {error}");
                    match retry_policy_for_error(&error) {
                        RetryPolicy::DoNotRetry => return Err(error),
                        RetryPolicy::AuthFailure => {
                            auth_failure_attempts += 1;
                            if reached_retry_limit(
                                auth_failure_attempts,
                                self.config.max_auth_failure_attempts,
                            ) {
                                return Err(GoldClawError::Unauthorized(format!(
                                    "wecom auth failed too many times ({auth_failure_attempts}): {error}"
                                )));
                            }

                            let delay = backoff_delay(
                                self.config.auth_retry_delay,
                                self.config.auth_retry_max_delay,
                                auth_failure_attempts,
                            );
                            warn!(
                                "wecom auth retry scheduled in {:?} (attempt {})",
                                delay, auth_failure_attempts
                            );
                            tokio::time::sleep(delay).await;
                        }
                        RetryPolicy::Reconnect => {
                            reconnect_attempts += 1;
                            if reached_retry_limit(
                                reconnect_attempts,
                                self.config.max_reconnect_attempts,
                            ) {
                                return Err(GoldClawError::Io(format!(
                                    "wecom connection failed too many times ({reconnect_attempts}): {error}"
                                )));
                            }

                            let delay = backoff_delay(
                                self.config.reconnect_delay,
                                self.config.reconnect_max_delay,
                                reconnect_attempts,
                            );
                            warn!(
                                "wecom reconnect scheduled in {:?} (attempt {})",
                                delay, reconnect_attempts
                            );
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }
        }
    }

    async fn run_connection(&self, runtime: Arc<dyn RuntimeHandle>) -> Result<()> {
        let (stream, _) = connect_async(self.config.ws_url.as_str())
            .await
            .map_err(ws_error)?;
        let (write, mut read) = stream.split();
        let writer = Arc::new(Mutex::new(write));

        self.authenticate(writer.clone(), &mut read).await?;
        info!(bot_id = %self.config.bot_id, "wecom connector authenticated");

        let heartbeat = spawn_heartbeat(writer.clone(), self.config.heartbeat_interval);

        while let Some(message) = read.next().await {
            match message.map_err(ws_error)? {
                Message::Text(payload) => {
                    let frame: WsFrame<Value> = serde_json::from_str(payload.as_ref())
                        .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;
                    self.handle_frame(runtime.clone(), writer.clone(), frame)
                        .await?;
                }
                Message::Binary(payload) => {
                    let text = String::from_utf8(payload.to_vec())
                        .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;
                    let frame: WsFrame<Value> = serde_json::from_str(&text)
                        .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;
                    self.handle_frame(runtime.clone(), writer.clone(), frame)
                        .await?;
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Frame(_) => {}
                Message::Close(_) => break,
            }
        }

        heartbeat.abort();
        let _ = heartbeat.await;
        Ok(())
    }

    async fn authenticate(
        &self,
        writer: Arc<Mutex<WsWriter>>,
        read: &mut futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> Result<()> {
        let req_id = generate_req_id(CMD_SUBSCRIBE);
        let frame = WsFrame {
            cmd: Some(CMD_SUBSCRIBE.into()),
            headers: WsHeaders::new(req_id.clone()),
            body: Some(
                serde_json::to_value(AuthBody {
                    bot_id: self.config.bot_id.clone(),
                    secret: self.config.secret.clone(),
                    scene: self.config.scene,
                    plug_version: self.config.plug_version.clone(),
                })
                .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?,
            ),
            errcode: None,
            errmsg: None,
        };

        send_ws_frame(writer, &frame).await?;

        let response = timeout(self.config.auth_timeout, read.next())
            .await
            .map_err(|_| GoldClawError::Internal("wecom auth timed out".into()))?
            .ok_or_else(|| GoldClawError::Internal("wecom auth stream closed".into()))?
            .map_err(ws_error)?;

        let text = match response {
            Message::Text(payload) => payload.to_string(),
            Message::Binary(payload) => String::from_utf8(payload.to_vec())
                .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?,
            Message::Close(_) => {
                return Err(GoldClawError::Internal(
                    "wecom auth connection closed".into(),
                ));
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                return Err(GoldClawError::Internal(
                    "wecom auth returned non-data frame".into(),
                ));
            }
        };

        let response: WsFrame<Value> = serde_json::from_str(&text)
            .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;

        if response.headers.req_id != req_id {
            return Err(GoldClawError::Internal(format!(
                "wecom auth response req_id mismatch: expected {req_id}, got {}",
                response.headers.req_id
            )));
        }

        match response.errcode.unwrap_or_default() {
            0 => Ok(()),
            code => Err(GoldClawError::Unauthorized(format!(
                "wecom auth failed with errcode {code}: {}",
                response
                    .errmsg
                    .unwrap_or_else(|| format!("unknown auth error {code}"))
            ))),
        }
    }

    async fn handle_frame(
        &self,
        runtime: Arc<dyn RuntimeHandle>,
        writer: Arc<Mutex<WsWriter>>,
        frame: WsFrame<Value>,
    ) -> Result<()> {
        match frame.cmd.as_deref() {
            Some(CMD_MESSAGE_CALLBACK) => {
                let Some(body) = frame.body else {
                    return Ok(());
                };
                let inbound: InboundMessage = serde_json::from_value(body)
                    .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;

                let Some(envelope) = self.message_to_envelope(&inbound)? else {
                    return Ok(());
                };

                let runtime = runtime.clone();
                let writer = writer.clone();
                let req_id = frame.headers.req_id;
                let reply_timeout = self.config.reply_timeout;
                tokio::spawn(async move {
                    if let Err(error) =
                        process_inbound_message(runtime, writer, req_id, envelope, reply_timeout)
                            .await
                    {
                        warn!("wecom message handling failed: {error}");
                    }
                });
            }
            Some(CMD_EVENT_CALLBACK) => {
                if let Some(body) = frame.body {
                    let event: InboundEvent = serde_json::from_value(body)
                        .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;
                    if event
                        .event
                        .as_ref()
                        .and_then(|value| value.eventtype.as_deref())
                        == Some(EVENT_DISCONNECTED)
                    {
                        return Err(GoldClawError::Internal(
                            "wecom server requested reconnect".into(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn message_to_envelope(&self, message: &InboundMessage) -> Result<Option<Envelope>> {
        let sender_id = message
            .from
            .as_ref()
            .map(|from| from.userid.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                GoldClawError::InvalidInput("wecom message missing from.userid".into())
            })?;

        let content = match message.msgtype.as_str() {
            "text" => message
                .text
                .as_ref()
                .map(|text| text.content.trim().to_string()),
            "voice" => message
                .voice
                .as_ref()
                .map(|voice| voice.content.trim().to_string()),
            _ => None,
        }
        .filter(|value| !value.is_empty());

        let Some(content) = content else {
            return Ok(None);
        };

        let conversation_id = match message.chattype.as_deref() {
            Some("group") => message
                .chatid
                .as_ref()
                .map(|chatid| format!("group:{chatid}"))
                .unwrap_or_else(|| format!("group:{sender_id}")),
            _ => format!("dm:{sender_id}"),
        };

        let mut envelope = Envelope::user(content, EnvelopeSource::Connector("wecom".into()), None);
        envelope.conversation = Some(ConversationRef {
            source_instance: Some(message.aibotid.clone()),
            conversation_id,
            sender_id: Some(sender_id),
            external_message_id: Some(message.msgid.clone()),
        });
        Ok(Some(envelope))
    }
}

#[async_trait]
impl Connector for WeComConnector {
    fn name(&self) -> &'static str {
        "wecom"
    }

    async fn run(self: Box<Self>, runtime: Arc<dyn RuntimeHandle>) -> Result<()> {
        self.run_forever(runtime).await
    }
}

async fn process_inbound_message(
    runtime: Arc<dyn RuntimeHandle>,
    writer: Arc<Mutex<WsWriter>>,
    req_id: String,
    envelope: Envelope,
    reply_timeout: Duration,
) -> Result<()> {
    let receipt = runtime.submit(envelope).await?;
    let reply = wait_for_assistant_reply(
        runtime,
        receipt.session_id,
        receipt.accepted_at,
        reply_timeout,
    )
    .await?;

    if reply.trim().is_empty() {
        return Ok(());
    }

    let frame = WsFrame {
        cmd: Some(CMD_REPLY.into()),
        headers: WsHeaders::new(req_id),
        body: Some(
            serde_json::to_value(StreamReplyBody {
                msgtype: "stream".into(),
                stream: StreamReply {
                    id: generate_req_id("stream"),
                    finish: Some(true),
                    content: Some(truncate_utf8(&reply, MAX_REPLY_BYTES)),
                },
            })
            .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?,
        ),
        errcode: None,
        errmsg: None,
    };

    send_ws_frame(writer, &frame).await
}

fn spawn_heartbeat(
    writer: Arc<Mutex<WsWriter>>,
    heartbeat_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(heartbeat_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let heartbeat = WsFrame::<Value> {
                cmd: Some(CMD_PING.into()),
                headers: WsHeaders::new(generate_req_id(CMD_PING)),
                body: None,
                errcode: None,
                errmsg: None,
            };
            if let Err(error) = send_ws_frame(writer.clone(), &heartbeat).await {
                warn!("wecom heartbeat stopped: {error}");
                break;
            }
        }
    })
}

async fn send_ws_frame(writer: Arc<Mutex<WsWriter>>, frame: &WsFrame<Value>) -> Result<()> {
    let payload = serde_json::to_string(frame)
        .map_err(|error| GoldClawError::InvalidInput(error.to_string()))?;
    let mut guard = writer.lock().await;
    guard
        .send(Message::Text(payload.into()))
        .await
        .map_err(ws_error)
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
                    "wecom reply stream failed: {error}"
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

fn generate_req_id(prefix: &str) -> String {
    let random = Uuid::new_v4().simple().to_string();
    format!(
        "{prefix}_{}_{}",
        Utc::now().timestamp_millis(),
        &random[..8]
    )
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = 0usize;
    for (idx, ch) in input.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }

    input[..end].to_string()
}

fn ws_error(error: tungstenite::Error) -> GoldClawError {
    GoldClawError::Io(error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryPolicy {
    DoNotRetry,
    AuthFailure,
    Reconnect,
}

fn retry_policy_for_error(error: &GoldClawError) -> RetryPolicy {
    match error {
        GoldClawError::Unauthorized(message) if message.contains("errcode 40058") => {
            RetryPolicy::DoNotRetry
        }
        GoldClawError::Unauthorized(message) if message.contains("invalid Request Parameter") => {
            RetryPolicy::DoNotRetry
        }
        GoldClawError::Unauthorized(message) if message.contains("errcode 45009") => {
            RetryPolicy::AuthFailure
        }
        GoldClawError::Unauthorized(message) if message.contains("api freq out of limit") => {
            RetryPolicy::AuthFailure
        }
        GoldClawError::Unauthorized(_) => RetryPolicy::AuthFailure,
        GoldClawError::Io(_) | GoldClawError::Internal(_) => RetryPolicy::Reconnect,
        GoldClawError::NotFound(_) | GoldClawError::InvalidInput(_) => RetryPolicy::DoNotRetry,
    }
}

fn reached_retry_limit(attempts: u32, max_attempts: i32) -> bool {
    max_attempts >= 0 && attempts > max_attempts as u32
}

fn backoff_delay(base: Duration, max: Duration, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(10);
    let multiplier = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
    let delay = base.saturating_mul(multiplier);
    delay.min(max)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WsHeaders {
    req_id: String,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl WsHeaders {
    fn new(req_id: String) -> Self {
        Self {
            req_id,
            extra: serde_json::Map::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WsFrame<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd: Option<String>,
    headers: WsHeaders,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    errcode: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    errmsg: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct AuthBody {
    bot_id: String,
    secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scene: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plug_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct InboundMessage {
    msgid: String,
    aibotid: String,
    #[allow(dead_code)]
    chatid: Option<String>,
    chattype: Option<String>,
    from: Option<InboundFrom>,
    msgtype: String,
    text: Option<TextContent>,
    voice: Option<VoiceContent>,
}

#[derive(Clone, Debug, Deserialize)]
struct InboundFrom {
    userid: String,
}

#[derive(Clone, Debug, Deserialize)]
struct TextContent {
    content: String,
}

#[derive(Clone, Debug, Deserialize)]
struct VoiceContent {
    content: String,
}

#[derive(Clone, Debug, Deserialize)]
struct InboundEvent {
    #[allow(dead_code)]
    msgid: Option<String>,
    event: Option<EventContent>,
}

#[derive(Clone, Debug, Deserialize)]
struct EventContent {
    eventtype: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct StreamReplyBody {
    msgtype: String,
    stream: StreamReply,
}

#[derive(Clone, Debug, Serialize)]
struct StreamReply {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };

    use goldclaw_core::{
        MessageRole, RuntimeHealth, SessionDetail, SessionMessage, SessionSummary,
        SubmissionReceipt,
    };
    use tokio::{net::TcpListener, sync::broadcast};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[test]
    fn text_message_maps_to_wecom_envelope() {
        let connector = WeComConnector::new(WeComConnectorConfig::new("bot-1", "secret-1"));
        let message = InboundMessage {
            msgid: "msg-1".into(),
            aibotid: "bot-1".into(),
            chatid: None,
            chattype: Some("single".into()),
            from: Some(InboundFrom {
                userid: "alice".into(),
            }),
            msgtype: "text".into(),
            text: Some(TextContent {
                content: " hello ".into(),
            }),
            voice: None,
        };

        let envelope = connector
            .message_to_envelope(&message)
            .expect("envelope result")
            .expect("envelope exists");

        assert_eq!(envelope.source, EnvelopeSource::Connector("wecom".into()));
        let conversation = envelope.conversation.expect("conversation");
        assert_eq!(conversation.source_instance.as_deref(), Some("bot-1"));
        assert_eq!(conversation.conversation_id, "dm:alice");
        assert_eq!(conversation.sender_id.as_deref(), Some("alice"));
        assert_eq!(conversation.external_message_id.as_deref(), Some("msg-1"));
        assert_eq!(envelope.content, "hello");
    }

    #[tokio::test]
    async fn run_connection_authenticates_and_replies() {
        let server = Arc::new(MockWeComServer::default());
        let ws_url = spawn_mock_wecom_server(server.clone()).await;

        let mut config = WeComConnectorConfig::new("bot-1", "secret-1");
        config.ws_url = ws_url;
        config.heartbeat_interval = Duration::from_secs(3600);
        config.reply_timeout = Duration::from_secs(2);

        let connector = WeComConnector::new(config);
        let runtime = Arc::new(MockRuntime::default());

        connector
            .run_connection(runtime.clone())
            .await
            .expect("run connection");

        let submissions = runtime.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].content, "ping");
        assert_eq!(
            submissions[0]
                .conversation
                .as_ref()
                .and_then(|value| value.sender_id.as_deref()),
            Some("alice")
        );
        drop(submissions);

        let reply = server
            .reply_frame
            .lock()
            .unwrap()
            .clone()
            .expect("reply frame");
        assert_eq!(reply["cmd"], CMD_REPLY);
        assert_eq!(reply["headers"]["req_id"], "req-msg-1");
        assert_eq!(reply["body"]["msgtype"], "stream");
        assert_eq!(reply["body"]["stream"]["finish"], true);
        assert_eq!(reply["body"]["stream"]["content"], "reply: ping");
    }

    #[derive(Default)]
    struct MockWeComServer {
        auth_frame: Mutex<Option<Value>>,
        reply_frame: Mutex<Option<Value>>,
    }

    async fn spawn_mock_wecom_server(state: Arc<MockWeComServer>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let ws = accept_async(stream).await.expect("ws accept");
            let (mut write, mut read) = ws.split();

            let auth = match read
                .next()
                .await
                .expect("auth frame")
                .expect("auth message")
            {
                Message::Text(payload) => payload.to_string(),
                other => panic!("unexpected auth message: {other:?}"),
            };
            let auth_value: Value = serde_json::from_str(&auth).expect("auth json");
            *state.auth_frame.lock().unwrap() = Some(auth_value.clone());

            let auth_req_id = auth_value["headers"]["req_id"]
                .as_str()
                .expect("auth req id")
                .to_string();
            write
                .send(Message::Text(
                    serde_json::json!({
                        "headers": { "req_id": auth_req_id },
                        "errcode": 0,
                        "errmsg": "ok"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("auth ack");

            write
                .send(Message::Text(
                    serde_json::json!({
                        "cmd": CMD_MESSAGE_CALLBACK,
                        "headers": { "req_id": "req-msg-1" },
                        "body": {
                            "msgid": "msg-1",
                            "aibotid": "bot-1",
                            "chattype": "single",
                            "from": { "userid": "alice" },
                            "msgtype": "text",
                            "text": { "content": "ping" }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("callback");

            while let Some(message) = read.next().await {
                let message = message.expect("reply message");
                let Message::Text(payload) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(payload.as_ref()).expect("reply json");
                if value["cmd"] == CMD_REPLY {
                    *state.reply_frame.lock().unwrap() = Some(value);
                    break;
                }
            }

            write.close().await.expect("close");
        });

        format!("ws://{}", address)
    }

    #[derive(Default)]
    struct MockRuntime {
        sessions: Mutex<HashMap<Uuid, SessionDetail>>,
        channels: OnceLock<Mutex<HashMap<Uuid, broadcast::Sender<AssistantEvent>>>>,
        submissions: Mutex<Vec<Envelope>>,
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
            self.submissions.lock().unwrap().push(envelope.clone());

            let reply = SessionMessage {
                id: Uuid::new_v4(),
                session_id,
                role: MessageRole::Assistant,
                source: EnvelopeSource::Connector("wecom".into()),
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
