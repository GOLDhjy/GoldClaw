use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use anyhow::Result as AnyhowResult;
use axum::{
    Router,
    extract::{Json, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{StreamExt, stream::Stream};
use goldclaw_core::{
    AssistantEvent, ConversationRef, Envelope, EnvelopeSource, GoldClawError, RuntimeHandle,
    SessionDetail, SessionSummary,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
pub struct GatewayConfig {
    pub bind: SocketAddr,
    pub allowed_origins: Vec<String>,
}

#[derive(Clone)]
pub struct GatewayServer {
    config: GatewayConfig,
}

#[derive(Clone)]
struct AppState {
    runtime: Arc<dyn RuntimeHandle>,
    allowed_origins: Vec<String>,
}

impl GatewayServer {
    pub fn new(config: GatewayConfig) -> Self {
        Self { config }
    }

    pub async fn serve<F>(&self, runtime: Arc<dyn RuntimeHandle>, shutdown: F) -> AnyhowResult<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let state = AppState {
            runtime,
            allowed_origins: self.config.allowed_origins.clone(),
        };

        let router = Router::new()
            .route("/healthz", get(healthz))
            .route("/status", get(status))
            .route("/sessions", get(list_sessions).post(create_session).options(preflight))
            .route("/sessions/{session_id}", get(load_session))
            .route("/messages", post(submit_message).options(preflight))
            .route("/sessions/{session_id}/events", get(session_events))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(state, enforce_origin));

        let listener = TcpListener::bind(self.config.bind).await?;
        info!("goldclaw gateway listening on http://{}", self.config.bind);
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await?;
        Ok(())
    }
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn healthz(
    State(state): State<AppState>,
) -> std::result::Result<Json<HealthResponse>, ApiError> {
    let health = state.runtime.health().await?;
    Ok(Json(HealthResponse { health }))
}

async fn status(
    State(state): State<AppState>,
) -> std::result::Result<Json<StatusResponse>, ApiError> {
    let health = state.runtime.health().await?;
    let sessions = state.runtime.list_sessions().await?;
    Ok(Json(StatusResponse { health, sessions }))
}

async fn list_sessions(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<SessionSummary>>, ApiError> {
    Ok(Json(state.runtime.list_sessions().await?))
}

async fn load_session(
    Path(session_id): Path<Uuid>,
    State(state): State<AppState>,
) -> std::result::Result<Json<SessionDetail>, ApiError> {
    Ok(Json(state.runtime.load_session(session_id).await?))
}

async fn create_session(
    State(state): State<AppState>,
    payload: Option<Json<CreateSessionRequest>>,
) -> std::result::Result<Json<SessionSummary>, ApiError> {
    let title = payload.and_then(|value| value.0.title);
    Ok(Json(state.runtime.create_session(title).await?))
}

async fn submit_message(
    State(state): State<AppState>,
    Json(payload): Json<SubmitMessageRequest>,
) -> std::result::Result<Json<goldclaw_core::SubmissionReceipt>, ApiError> {
    let mut envelope = Envelope::user(
        payload.content,
        payload.source.unwrap_or(EnvelopeSource::Cli),
        payload.session_id,
    );
    envelope.conversation = payload.conversation;
    Ok(Json(state.runtime.submit(envelope).await?))
}

async fn session_events(
    Path(session_id): Path<Uuid>,
    State(state): State<AppState>,
) -> std::result::Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>, ApiError>
{
    let receiver = state.runtime.subscribe(session_id).await?;
    let stream = BroadcastStream::new(receiver).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok(to_sse_event(event))),
            Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn enforce_origin(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Preflight — respond immediately.
    if request.method() == Method::OPTIONS {
        if let Some(ref o) = origin {
            if origin_allowed(o, &state.allowed_origins) {
                return cors_preflight_response(o);
            }
        }
        return StatusCode::FORBIDDEN.into_response();
    }

    if let Some(ref o) = origin {
        if !origin_allowed(o, &state.allowed_origins) {
            return (
                StatusCode::FORBIDDEN,
                Json(ApiProblem {
                    error: "origin_not_allowed".into(),
                    message: format!("origin `{o}` is not allowed"),
                }),
            )
                .into_response();
        }
    }

    let mut response = next.run(request).await;

    if let Some(o) = origin {
        let hdrs = response.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&o) {
            hdrs.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        hdrs.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }

    response
}

fn cors_preflight_response(origin: &str) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let hdrs = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(origin) {
        hdrs.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    hdrs.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    hdrs.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    hdrs.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    response
}

fn origin_allowed(origin: &str, allowed_origins: &[String]) -> bool {
    let parsed = match Url::parse(origin) {
        Ok(value) => value,
        Err(_) => return false,
    };

    // Allow any http/https request from localhost or 127.0.0.1, regardless of port.
    if matches!(parsed.scheme(), "http" | "https") {
        if matches!(parsed.host_str(), Some("localhost") | Some("127.0.0.1")) {
            return true;
        }
    }

    let base = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default()
    );
    allowed_origins
        .iter()
        .any(|allowed| allowed == origin || allowed == &base)
}

fn to_sse_event(event: AssistantEvent) -> Event {
    let name = event.event_name();
    let data = serde_json::to_string(&event).unwrap_or_else(|error| {
        serde_json::json!({
            "type": "error",
            "message": format!("failed to serialize event: {error}")
        })
        .to_string()
    });
    Event::default().event(name).data(data)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    health: goldclaw_core::RuntimeHealth,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    health: goldclaw_core::RuntimeHealth,
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubmitMessageRequest {
    session_id: Option<Uuid>,
    content: String,
    source: Option<EnvelopeSource>,
    conversation: Option<ConversationRef>,
}

#[derive(Debug, Serialize)]
struct ApiProblem {
    error: String,
    message: String,
}

struct ApiError(GoldClawError);

impl From<GoldClawError> for ApiError {
    fn from(value: GoldClawError) -> Self {
        Self(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            GoldClawError::NotFound(_) => StatusCode::NOT_FOUND,
            GoldClawError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            GoldClawError::Unauthorized(_) => StatusCode::FORBIDDEN,
            GoldClawError::Io(_) | GoldClawError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(ApiProblem {
            error: "gateway_error".into(),
            message: self.0.to_string(),
        });
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests;
