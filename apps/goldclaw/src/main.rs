use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    net::TcpStream,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use dialoguer::{Input, Password, Select};
use goldclaw_config::{
    AgentSettings, ConnectorSettings, GatewaySettings, GoldClawConfig, ProjectPaths,
    ProviderSettings, RuntimeSettings, WeComSettings,
};
use goldclaw_connector_wecom::{WeComConnector, WeComConnectorConfig};
use goldclaw_connector_weixin::{WeixinConnector, WeixinConnectorConfig};
use goldclaw_core::Connector as _;
use goldclaw_doctor::{DoctorReport, HealthStatus, run_doctor};
use goldclaw_gateway::{GatewayConfig, GatewayServer};
use goldclaw_memory::SqliteMemoryStore;
use goldclaw_provider_glm::GlmProvider;
use goldclaw_runtime::{
    EchoProvider, InMemoryRuntime, StandardMessageBuilder, StaticPolicy,
    tools::{BuiltinTool, ReadWorkspaceTool, UpdateSoulTool},
};
use goldclaw_store::{SqliteStore, StoreLayout, current_schema_version};
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "goldclaw", version, about = "GoldClaw local AI assistant")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Init,
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Start,
    Stop,
    Restart,
    Status,
    Connector {
        #[command(subcommand)]
        command: ConnectorCommand,
    },
    Gateway {
        #[command(subcommand)]
        command: GatewayCommand,
    },
}

#[derive(Subcommand, Debug)]
enum GatewayCommand {
    Run {
        #[arg(long)]
        bind: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConnectorCommand {
    Wecom {
        #[command(subcommand)]
        command: WecomCommand,
    },
    Weixin {
        #[command(subcommand)]
        command: WeixinCommand,
    },
}

#[derive(Subcommand, Debug)]
enum WecomCommand {
    Run {
        #[arg(long, env = "GOLDCLAW_WECOM_BOT_ID")]
        bot_id: Option<String>,
        #[arg(long, env = "GOLDCLAW_WECOM_SECRET")]
        secret: Option<String>,
        #[arg(long, env = "GOLDCLAW_WECOM_WS_URL")]
        ws_url: Option<String>,
        #[arg(long, env = "GOLDCLAW_WECOM_SCENE")]
        scene: Option<u32>,
        #[arg(long, env = "GOLDCLAW_WECOM_PLUG_VERSION")]
        plug_version: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum WeixinCommand {
    Login,
    Run,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeState {
    pid: u32,
    bind: String,
    profile: String,
    started_at: DateTime<Utc>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => init_config(false)?,
        Commands::Doctor { json } => run_doctor_command(json)?,
        Commands::Start => start_gateway()?,
        Commands::Stop => stop_gateway()?,
        Commands::Restart => {
            stop_gateway()?;
            start_gateway()?;
        }
        Commands::Status => show_status()?,
        Commands::Connector { command } => match command {
            ConnectorCommand::Wecom { command } => match command {
                WecomCommand::Run {
                    bot_id,
                    secret,
                    ws_url,
                    scene,
                    plug_version,
                } => {
                    wecom_run(bot_id, secret, ws_url, scene, plug_version).await?;
                }
            },
            ConnectorCommand::Weixin { command } => match command {
                WeixinCommand::Login => weixin_login().await?,
                WeixinCommand::Run => weixin_run().await?,
            },
        },
        Commands::Gateway { command } => match command {
            GatewayCommand::Run { bind } => gateway_run(bind).await?,
        },
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .try_init();
}

fn init_config(_force: bool) -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;

    let config_path = paths.config_file();

    // Load existing config as defaults; fall back to built-in defaults if not yet initialized.
    let current_dir =
        std::env::current_dir().context("failed to determine current working directory")?;
    let existing = GoldClawConfig::load(&config_path).unwrap_or_default();

    let soul_path = paths.soul_path();
    let existing_soul = fs::read_to_string(&soul_path).unwrap_or_default();
    let soul_answers = parse_soul_answers(&existing_soul, &existing.agent.name);

    println!("── 使用偏好 ────────────────────────────");

    let user_name = prompt_text("助手如何称呼你", Some(soul_answers.user_name), true)?;

    let assistant_name = prompt_text("助手的名字", Some(soul_answers.assistant_name), true)?;

    let language = prompt_text(
        "默认回复语言（默认普通话）",
        Some(soul_answers.language),
        true,
    )?;

    let conversation_style = prompt_text("对话风格", Some(soul_answers.conversation_style), true)?;

    println!("── Provider ────────────────────────────");

    let api_key = prompt_text(
        "BigModel API key (留空保持不变)",
        existing.provider.api_key.clone(),
        true,
    )?;

    let api_key = api_key.trim().to_string();

    println!("\n── 网关 ────────────────────────────────");

    let bind: String = Input::new()
        .with_prompt("Gateway bind address")
        .default(existing.gateway.bind.clone())
        .interact_text()?;

    let default_read_root = existing
        .runtime
        .read_roots
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| current_dir.display().to_string());

    let read_root: String = Input::new()
        .with_prompt("工作区根目录")
        .default(default_read_root)
        .interact_text()?;

    println!("\n── 渠道接入 ─────────────────────────────");

    let connectors = match prompt_connector_selection(existing.connectors.wecom.is_some())? {
        InitConnectorChoice::None => ConnectorSettings::default(),
        InitConnectorChoice::WeCom => {
            let existing_wecom = existing.connectors.wecom.as_ref();
            let bot_id = prompt_text(
                "企微 Bot ID",
                existing_wecom.map(|settings| settings.bot_id.clone()),
                false,
            )?;
            let secret_input = prompt_secret(
                if existing_wecom
                    .and_then(|settings| settings.secret.as_ref())
                    .is_some()
                {
                    "企微 Secret（留空保持不变）"
                } else {
                    "企微 Secret"
                },
                existing_wecom
                    .and_then(|settings| settings.secret.as_ref())
                    .is_some(),
            )?;

            let secret = if secret_input.trim().is_empty() {
                existing_wecom.and_then(|settings| settings.secret.clone())
            } else {
                Some(secret_input)
            };

            let bot_id = bot_id.trim().to_string();
            if bot_id.is_empty() {
                bail!("企微 Bot ID 不能为空");
            }
            if secret.as_deref().unwrap_or("").trim().is_empty() {
                bail!("企微 Secret 不能为空");
            }

            let ws_url = prompt_optional_text(
                "企微 WebSocket 地址（可选，默认官方地址）",
                existing_wecom.and_then(|settings| settings.ws_url.clone()),
            )?;
            let scene = prompt_optional_u32(
                "企微 Scene（可选）",
                existing_wecom.and_then(|settings| settings.scene),
            )?;
            let plug_version = prompt_optional_text(
                "企微 Plug Version（可选）",
                existing_wecom.and_then(|settings| settings.plug_version.clone()),
            )?;

            ConnectorSettings {
                wecom: Some(WeComSettings {
                    bot_id,
                    secret,
                    ws_url,
                    scene,
                    plug_version,
                }),
            }
        }
    };

    println!();

    let config = GoldClawConfig {
        version: existing.version,
        profile: existing.profile.clone(),
        agent: AgentSettings {
            name: if assistant_name.trim().is_empty() {
                existing.agent.name.clone()
            } else {
                assistant_name.trim().to_string()
            },
            personality: String::new(),
            style: String::new(),
        },
        gateway: GatewaySettings {
            bind,
            allowed_origins: existing.gateway.allowed_origins,
        },
        runtime: RuntimeSettings {
            read_roots: vec![PathBuf::from(read_root)],
        },
        provider: ProviderSettings {
            api_key: if api_key.is_empty() {
                None
            } else {
                Some(api_key)
            },
            model: existing.provider.model,
        },
        connectors,
    };

    let config = config.normalize()?;
    config.save(&config_path)?;

    let soul = build_soul_content(
        &config.agent.name,
        &SoulAnswers {
            user_name,
            assistant_name: config.agent.name.clone(),
            language,
            conversation_style,
        },
    )?;
    fs::write(&soul_path, soul.as_bytes())
        .with_context(|| format!("failed to write {}", soul_path.display()))?;
    println!("Soul 文件已更新: {}", soul_path.display());

    let store = StoreLayout::from_project_paths(&paths);
    store.ensure_parent_dirs()?;
    let sqlite = SqliteStore::open(store.clone())?;

    println!("GoldClaw 配置已写入: {}", config_path.display());
    println!(
        "SQLite 数据库路径: {}",
        store.paths().database_file.display()
    );
    println!(
        "已应用 schema 版本: {} / {}",
        sqlite.applied_schema_version()?,
        current_schema_version()
    );
    if config.connectors.wecom.is_some() {
        println!("已配置企微长连接 connector。后续可直接运行 `goldclaw connector wecom run`。");
    } else {
        println!("当前未启用任何外部渠道 connector。");
    }

    println!("\n正在启动后台服务...");
    match start_gateway() {
        Ok(()) => {} // start_gateway already printed the web URL
        Err(e) => {
            println!("警告: 后台服务启动失败: {e}");
            println!("可以稍后手动运行 `goldclaw start`");
        }
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SoulAnswers {
    user_name: String,
    assistant_name: String,
    language: String,
    conversation_style: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitConnectorChoice {
    None,
    WeCom,
}

fn default_soul_answers(agent_name: &str) -> SoulAnswers {
    SoulAnswers {
        user_name: String::new(),
        assistant_name: if agent_name.trim().is_empty() {
            "GoldClaw".into()
        } else {
            agent_name.trim().into()
        },
        language: "普通话".into(),
        conversation_style: String::new(),
    }
}

fn build_soul_content(agent_name: &str, answers: &SoulAnswers) -> Result<String> {
    let agent_name = if agent_name.trim().is_empty() {
        "GoldClaw"
    } else {
        agent_name.trim()
    };
    let user_name = section_value(&answers.user_name, "未指定，必要时先问清楚用户希望的称呼。");
    let language = section_value(&answers.language, "普通话");
    let conversation_style = section_value(
        &answers.conversation_style,
        "默认使用清晰、直接、务实的表达，优先给出结论和下一步。",
    );

    normalize_soul_content(&format!(
        "# 助手身份\n\n你是 {agent_name}，一个本地 AI 助手。你需要长期根据下面的信息调整自己的表达和协作方式。\n\n# 助手名字\n\n{agent_name}\n\n# 用户称呼\n\n{user_name}\n\n# 默认回复语言\n\n{language}\n\n# 对话风格\n\n{conversation_style}\n\n# 持续规则\n\n- 上面的信息优先作为长期规则执行。\n- 如果用户希望修改人设、语气、协作方式或长期规则，应更新 soul，而不是只在当前回复里临时答应。\n"
    ))
}

fn parse_soul_answers(content: &str, agent_name: &str) -> SoulAnswers {
    let defaults = default_soul_answers(agent_name);
    if content.trim().is_empty() {
        return defaults;
    }

    SoulAnswers {
        user_name: extract_soul_section(content, "用户称呼").unwrap_or(defaults.user_name),
        assistant_name: extract_soul_section(content, "助手名字")
            .unwrap_or(defaults.assistant_name),
        language: extract_soul_section(content, "默认回复语言").unwrap_or(defaults.language),
        conversation_style: extract_soul_section(content, "对话风格")
            .unwrap_or(defaults.conversation_style),
    }
}

fn extract_soul_section(content: &str, heading: &str) -> Option<String> {
    let marker = format!("# {heading}");
    let start = content.find(&marker)? + marker.len();
    let rest = content[start..].trim_start_matches(['\r', '\n']);
    let end = rest.find("\n# ").unwrap_or(rest.len());
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn section_value(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.into()
    } else {
        trimmed.into()
    }
}

fn prompt_text(prompt: &str, default: Option<String>, allow_empty: bool) -> Result<String> {
    let mut input = Input::<String>::new()
        .with_prompt(prompt)
        .allow_empty(allow_empty);
    if let Some(default) = default.filter(|value| !value.is_empty()) {
        input = input.default(default);
    }
    input.interact_text().map_err(Into::into)
}

fn prompt_optional_text(prompt: &str, default: Option<String>) -> Result<Option<String>> {
    let value = prompt_text(prompt, default, true)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn prompt_optional_u32(prompt: &str, default: Option<u32>) -> Result<Option<u32>> {
    let default_text = default.map(|value| value.to_string());
    let value = prompt_text(prompt, default_text, true)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        trimmed
            .parse::<u32>()
            .map(Some)
            .map_err(|error| anyhow!("`{prompt}` 不是有效数字: {error}"))
    }
}

fn prompt_secret(prompt: &str, allow_empty: bool) -> Result<String> {
    Password::new()
        .with_prompt(prompt)
        .allow_empty_password(allow_empty)
        .interact()
        .map_err(Into::into)
}

fn prompt_connector_selection(existing_wecom_enabled: bool) -> Result<InitConnectorChoice> {
    let options = ["暂不接入渠道", "企业微信（长连接机器人）"];
    let selection = Select::new()
        .with_prompt("要接入哪个渠道")
        .items(options)
        .default(if existing_wecom_enabled { 1 } else { 0 })
        .interact()?;

    Ok(match selection {
        1 => InitConnectorChoice::WeCom,
        _ => InitConnectorChoice::None,
    })
}

fn normalize_soul_content(content: &str) -> Result<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        bail!("soul 内容不能为空");
    }

    let mut normalized = trimmed.to_string();
    normalized.push('\n');
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_soul_answers_reads_existing_sections() {
        let soul = "# 助手身份\n\n你是 GoldClaw，一个本地 AI 助手。\n\n# 助手名字\n\nGoldClaw\n\n# 用户称呼\n\n阿金\n\n# 默认回复语言\n\n普通话\n\n# 对话风格\n\n直接、简洁，优先给结论。\n";
        let answers = parse_soul_answers(soul, "GoldClaw");

        assert_eq!(
            answers,
            SoulAnswers {
                user_name: "阿金".into(),
                assistant_name: "GoldClaw".into(),
                language: "普通话".into(),
                conversation_style: "直接、简洁，优先给结论。".into(),
            }
        );
    }

    #[test]
    fn build_soul_content_formats_answers_into_sections() {
        let soul = build_soul_content(
            "GoldClaw",
            &SoulAnswers {
                user_name: "阿金".into(),
                assistant_name: "GoldClaw".into(),
                language: "普通话".into(),
                conversation_style: "回答更直接，并给出下一步。".into(),
            },
        )
        .expect("build soul");

        assert!(soul.contains("# 助手名字\n\nGoldClaw"));
        assert!(soul.contains("# 用户称呼\n\n阿金"));
        assert!(soul.contains("# 默认回复语言\n\n普通话"));
        assert!(soul.contains("# 对话风格\n\n回答更直接，并给出下一步。"));
    }

    #[test]
    fn resolve_wecom_connector_config_falls_back_to_saved_settings() {
        let config = GoldClawConfig {
            connectors: ConnectorSettings {
                wecom: Some(WeComSettings {
                    bot_id: "bot-from-config".into(),
                    secret: Some("secret-from-config".into()),
                    ws_url: Some("wss://example.test/ws".into()),
                    scene: Some(7),
                    plug_version: Some("1.2.3".into()),
                }),
            },
            ..GoldClawConfig::default()
        };

        let resolved =
            resolve_wecom_connector_config(&config, None, None, None, None, None).expect("resolve");

        assert_eq!(resolved.bot_id, "bot-from-config");
        assert_eq!(resolved.secret, "secret-from-config");
        assert_eq!(resolved.ws_url, "wss://example.test/ws");
        assert_eq!(resolved.scene, Some(7));
        assert_eq!(resolved.plug_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn resolve_wecom_connector_config_prefers_cli_over_saved_settings() {
        let config = GoldClawConfig {
            connectors: ConnectorSettings {
                wecom: Some(WeComSettings {
                    bot_id: "bot-from-config".into(),
                    secret: Some("secret-from-config".into()),
                    ws_url: None,
                    scene: Some(7),
                    plug_version: None,
                }),
            },
            ..GoldClawConfig::default()
        };

        let resolved = resolve_wecom_connector_config(
            &config,
            Some("bot-from-cli".into()),
            Some("secret-from-cli".into()),
            Some("wss://override.test/ws".into()),
            Some(9),
            Some("2.0.0".into()),
        )
        .expect("resolve");

        assert_eq!(resolved.bot_id, "bot-from-cli");
        assert_eq!(resolved.secret, "secret-from-cli");
        assert_eq!(resolved.ws_url, "wss://override.test/ws");
        assert_eq!(resolved.scene, Some(9));
        assert_eq!(resolved.plug_version.as_deref(), Some("2.0.0"));
    }
}

fn run_doctor_command(json: bool) -> Result<()> {
    let paths = ProjectPaths::discover()?;
    let report = run_doctor(&paths);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_doctor_report(&report);
    }

    if report.has_failures() {
        std::process::exit(1);
    }

    Ok(())
}

fn start_gateway() -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;
    let config = load_config(&paths)?;
    let store = StoreLayout::from_project_paths(&paths);
    store.ensure_parent_dirs()?;
    let sqlite = SqliteStore::open(store.clone())?;
    config.validate_loopback_bind()?;

    if let Some(state) = load_runtime_state(&paths)? {
        if port_open(&state.bind) {
            println!(
                "GoldClaw gateway 已在运行: {} (pid {})",
                state.bind, state.pid
            );
            return Ok(());
        }
    }

    if sqlite.has_pending_migrations()? {
        bail!(
            "database {} still has pending migrations after initialization",
            store.paths().database_file.display()
        );
    }

    let log_path = paths.gateway_log_file();
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let err_file = log_file.try_clone()?;

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(&exe);
    command.arg("gateway").arg("run");
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(log_file));
    command.stderr(Stdio::from(err_file));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let child = command.spawn().context("failed to spawn gateway process")?;

    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(150));
        if port_open(&config.gateway.bind) {
            println!("GoldClaw gateway 已启动 (pid {})", child.id());
            break;
        }
    }

    if !port_open(&config.gateway.bind) {
        bail!(
            "gateway process spawned (pid {}), but port {} did not become reachable; inspect {}",
            child.id(),
            config.gateway.bind,
            log_path.display()
        );
    }

    // Start the web UI server.
    let web_bind =
        std::env::var("GOLDCLAW_WEB_BIND").unwrap_or_else(|_| "127.0.0.1:4264".to_string());
    let web_exe = exe.with_file_name("goldclaw-web");
    if web_exe.exists() {
        let web_log = log_path.with_file_name("web.log");
        let web_log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&web_log)
            .with_context(|| format!("failed to open {}", web_log.display()))?;
        let web_err_file = web_log_file.try_clone()?;

        let mut web_cmd = Command::new(&web_exe);
        web_cmd.env("GOLDCLAW_WEB_BIND", &web_bind);
        web_cmd.stdin(Stdio::null());
        web_cmd.stdout(Stdio::from(web_log_file));
        web_cmd.stderr(Stdio::from(web_err_file));

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            web_cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }

        let web_child = web_cmd.spawn().context("failed to spawn web process")?;
        println!("GoldClaw web    已启动 (pid {})", web_child.id());
        println!("\n打开浏览器开始对话: http://{web_bind}");
    } else {
        println!("\n打开浏览器开始对话: http://{web_bind}  (需先安装 goldclaw-web)");
    }

    Ok(())
}

fn stop_gateway() -> Result<()> {
    let paths = ProjectPaths::discover()?;
    let Some(state) = load_runtime_state(&paths)? else {
        println!("GoldClaw gateway 当前未运行。");
        return Ok(());
    };

    #[cfg(windows)]
    let status = Command::new("taskkill")
        .args(["/PID", &state.pid.to_string(), "/T", "/F"])
        .status()
        .context("failed to invoke taskkill")?;

    #[cfg(not(windows))]
    let status = Command::new("kill")
        .arg(state.pid.to_string())
        .status()
        .context("failed to invoke kill")?;

    if !status.success() {
        bail!("failed to stop gateway pid {}", state.pid);
    }

    remove_runtime_state(&paths)?;
    println!("GoldClaw gateway 已停止。");
    Ok(())
}

fn show_status() -> Result<()> {
    let paths = ProjectPaths::discover()?;
    let config = load_config(&paths)?;
    let store = StoreLayout::from_project_paths(&paths);
    let state = load_runtime_state(&paths)?;
    let inspection = SqliteStore::inspect(&store)?;

    match state {
        Some(state) if port_open(&state.bind) => {
            println!("status: running");
            println!("bind:   {}", state.bind);
            println!("pid:    {}", state.pid);
            println!("since:  {}", state.started_at);
        }
        Some(state) => {
            println!("status: stale");
            println!("bind:   {}", state.bind);
            println!("pid:    {}", state.pid);
            println!("hint:   gateway state exists but port is unreachable");
        }
        None => {
            println!("status: stopped");
            println!("bind:   {}", config.gateway.bind);
        }
    }
    println!("db:     {}", store.paths().database_file.display());
    if inspection.database_exists {
        println!(
            "schema: v{} / {}",
            inspection.applied_schema_version, inspection.target_schema_version
        );
    } else {
        println!("schema: v{}", current_schema_version());
    }

    Ok(())
}

fn build_provider(config: &GoldClawConfig) -> std::sync::Arc<dyn goldclaw_core::Provider> {
    match GlmProvider::from_env_or_config(
        config.provider.api_key.clone(),
        config.provider.model.clone(),
    ) {
        Ok(p) => {
            tracing::info!("using GLM provider");
            std::sync::Arc::new(p)
        }
        Err(reason) => {
            tracing::warn!(%reason, "falling back to echo provider");
            std::sync::Arc::new(EchoProvider)
        }
    }
}

fn build_message_builder(
    paths: &ProjectPaths,
) -> std::sync::Arc<dyn goldclaw_core::MessageBuilder> {
    std::sync::Arc::new(StandardMessageBuilder::with_soul_path(paths.soul_path()))
}

fn build_embedding_provider(
    config: &GoldClawConfig,
) -> Option<std::sync::Arc<dyn goldclaw_core::EmbeddingProvider>> {
    match goldclaw_provider_glm::GlmProvider::from_env_or_config(
        config.provider.api_key.clone(),
        config.provider.model.clone(),
    ) {
        Ok(p) => Some(std::sync::Arc::new(p)),
        Err(_) => None,
    }
}

async fn gateway_run(bind_override: Option<String>) -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;
    let mut config = load_config(&paths)?;
    if let Some(bind) = bind_override {
        config.gateway.bind = bind;
    }
    config.validate_loopback_bind()?;

    let bind = config.bind_addr()?;
    let runtime_state = RuntimeState {
        pid: std::process::id(),
        bind: config.gateway.bind.clone(),
        profile: config.profile.clone(),
        started_at: Utc::now(),
    };
    write_runtime_state(&paths, &runtime_state)?;
    let _state_guard = RuntimeStateGuard::new(paths.runtime_state_file());
    let runtime = build_runtime(&paths, &config)?;

    let gateway = GatewayServer::new(GatewayConfig {
        bind,
        allowed_origins: config.gateway.allowed_origins.clone(),
    });

    gateway
        .serve(std::sync::Arc::new(runtime), async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

fn build_runtime(paths: &ProjectPaths, config: &GoldClawConfig) -> Result<InMemoryRuntime> {
    let read_roots = if config.runtime.read_roots.is_empty() {
        vec![std::env::current_dir().context("failed to determine current directory")?]
    } else {
        config.runtime.read_roots.clone()
    };

    let provider = build_provider(config);
    let soul_path = paths.soul_path();

    let builtin_tools: Vec<std::sync::Arc<dyn BuiltinTool>> = vec![
        std::sync::Arc::new(ReadWorkspaceTool::new(read_roots)),
        std::sync::Arc::new(UpdateSoulTool::new(soul_path.clone())),
    ];
    let message_builder = build_message_builder(paths);

    let memory_store: Option<std::sync::Arc<dyn goldclaw_core::MemoryStore>> =
        SqliteMemoryStore::open(&paths.database_file())
            .ok()
            .map(|s| std::sync::Arc::new(s) as std::sync::Arc<dyn goldclaw_core::MemoryStore>);

    let embedding_provider: Option<std::sync::Arc<dyn goldclaw_core::EmbeddingProvider>> =
        build_embedding_provider(config);

    tracing::info!(
        soul_enabled = soul_path.exists(),
        embedding_enabled = embedding_provider.is_some(),
        memory_enabled = memory_store.is_some(),
        "starting runtime (sessions: in-memory, memory: persisted)"
    );

    Ok(InMemoryRuntime::with_memory(
        message_builder,
        provider,
        std::sync::Arc::new(StaticPolicy::allow_only(["read_file", "update_soul"])),
        builtin_tools,
        embedding_provider,
        memory_store,
    ))
}

async fn weixin_login() -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;
    let connector = build_weixin_connector(&paths);
    connector.login(true).await?;
    Ok(())
}

async fn weixin_run() -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;
    let config = load_config(&paths)?;
    let runtime = build_runtime(&paths, &config)?;
    let connector = build_weixin_connector(&paths);

    println!("微信 connector 已启动，按 Ctrl-C 退出。");
    Box::new(connector)
        .run(std::sync::Arc::new(runtime))
        .await
        .map_err(Into::into)
}

async fn wecom_run(
    bot_id: Option<String>,
    secret: Option<String>,
    ws_url: Option<String>,
    scene: Option<u32>,
    plug_version: Option<String>,
) -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;
    let config = load_config(&paths)?;
    let runtime = build_runtime(&paths, &config)?;
    let connector_config =
        resolve_wecom_connector_config(&config, bot_id, secret, ws_url, scene, plug_version)?;

    println!("企微 long-link connector 已启动，按 Ctrl-C 退出。");
    Box::new(WeComConnector::new(connector_config))
        .run(std::sync::Arc::new(runtime))
        .await
        .map_err(Into::into)
}

fn resolve_wecom_connector_config(
    config: &GoldClawConfig,
    bot_id: Option<String>,
    secret: Option<String>,
    ws_url: Option<String>,
    scene: Option<u32>,
    plug_version: Option<String>,
) -> Result<WeComConnectorConfig> {
    let configured = config.connectors.wecom.as_ref();

    let bot_id = bot_id
        .or_else(|| configured.map(|settings| settings.bot_id.clone()))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("缺少企微 Bot ID。请在 `goldclaw init` 中配置，或通过 `--bot-id` 传入。")
        })?;

    let secret = secret
        .or_else(|| configured.and_then(|settings| settings.secret.clone()))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("缺少企微 Secret。请在 `goldclaw init` 中配置，或通过 `--secret` 传入。")
        })?;

    let mut connector_config = WeComConnectorConfig::new(bot_id, secret);

    if let Some(ws_url) = ws_url
        .or_else(|| configured.and_then(|settings| settings.ws_url.clone()))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        connector_config.ws_url = ws_url;
    }
    connector_config.scene = scene.or_else(|| configured.and_then(|settings| settings.scene));
    connector_config.plug_version =
        plug_version.or_else(|| configured.and_then(|settings| settings.plug_version.clone()));

    Ok(connector_config)
}

fn build_weixin_connector(paths: &ProjectPaths) -> WeixinConnector {
    let state_dir = paths.base_dir().join("connectors").join("weixin");
    WeixinConnector::new(WeixinConnectorConfig::new(state_dir))
}

fn load_config(paths: &ProjectPaths) -> Result<GoldClawConfig> {
    GoldClawConfig::load_resolved(&paths.config_file()).map_err(|error| match error {
        goldclaw_config::ConfigError::MissingConfig(_) => {
            anyhow!("GoldClaw 尚未初始化，请先运行 `goldclaw init`。")
        }
        other => other.into(),
    })
}

fn port_open(bind: &str) -> bool {
    let Ok(address) = bind.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

fn load_runtime_state(paths: &ProjectPaths) -> Result<Option<RuntimeState>> {
    let path = paths.runtime_state_file();
    match fs::read_to_string(&path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_runtime_state(paths: &ProjectPaths, state: &RuntimeState) -> Result<()> {
    let raw = serde_json::to_string_pretty(state)?;
    fs::write(paths.runtime_state_file(), raw)?;
    Ok(())
}

fn remove_runtime_state(paths: &ProjectPaths) -> Result<()> {
    match fs::remove_file(paths.runtime_state_file()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct RuntimeStateGuard {
    path: PathBuf,
}

impl RuntimeStateGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for RuntimeStateGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn print_doctor_report(report: &DoctorReport) {
    println!("GoldClaw Doctor");
    println!("generated_at: {}", report.generated_at);
    println!(
        "summary: {}",
        if report.healthy {
            "healthy"
        } else {
            "issues detected"
        }
    );

    for check in &report.checks {
        let marker = match check.status {
            HealthStatus::Pass => "[ok]",
            HealthStatus::Warn => "[warn]",
            HealthStatus::Fail => "[fail]",
        };
        println!("{marker} {}: {}", check.id, check.summary);
        println!("       {}", check.detail);
    }
}
