use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, response::Html, routing::get};
use goldclaw_config::ProjectPaths;
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;

const INDEX_HTML: &str = include_str!("index.html");
const DEFAULT_BIND: &str = "127.0.0.1:4264";

#[derive(Clone, Serialize)]
struct WebConfig {
    gateway: String,
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn web_config(State(cfg): State<WebConfig>) -> Json<WebConfig> {
    Json(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "goldclaw_web=info".parse().unwrap()),
        )
        .init();

    let paths = ProjectPaths::discover().context("failed to resolve project paths")?;
    let gc_config = goldclaw_config::GoldClawConfig::load(&paths.config_file()).unwrap_or_default();

    let gateway_url = format!("http://{}", gc_config.gateway.bind);
    let bind: std::net::SocketAddr = std::env::var("GOLDCLAW_WEB_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()
        .context("invalid bind address")?;

    let web_cfg = WebConfig {
        gateway: gateway_url,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/config.json", get(web_config))
        .with_state(web_cfg);

    info!("goldclaw-web listening on http://{bind}");
    println!("打开浏览器访问: http://{bind}");
    let listener = TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
