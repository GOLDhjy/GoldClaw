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
use dialoguer::Input;
use goldclaw_config::{AgentSettings, GatewaySettings, GoldClawConfig, ProjectPaths, ProviderSettings, RuntimeSettings};
use goldclaw_doctor::{DoctorReport, HealthStatus, run_doctor};
use goldclaw_gateway::{GatewayConfig, GatewayServer};
use goldclaw_connector_stdin::StdinConnector;
use goldclaw_core::Connector;
use goldclaw_provider_glm::GlmProvider;
use goldclaw_runtime::{EchoProvider, InMemoryRuntime, ReadWorkspaceTool, StaticPolicy};
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
    Chat,
    Start,
    Stop,
    Status,
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
        Commands::Chat => chat_run().await?,
        Commands::Start => start_gateway()?,
        Commands::Stop => stop_gateway()?,
        Commands::Status => show_status()?,
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

    println!("── 智能体 ──────────────────────────────");

    let agent_name: String = Input::new()
        .with_prompt("智能体名称")
        .default(existing.agent.name.clone())
        .interact_text()?;

    let personality: String = Input::new()
        .with_prompt("性格 (留空跳过)")
        .default(existing.agent.personality.clone())
        .allow_empty(true)
        .interact_text()?;

    let style: String = Input::new()
        .with_prompt("说话风格 (留空跳过)")
        .default(existing.agent.style.clone())
        .allow_empty(true)
        .interact_text()?;

    println!("\n── Provider ────────────────────────────");

    let api_key: String = Input::new()
        .with_prompt("BigModel API key (留空保持不变)")
        .default(existing.provider.api_key.clone().unwrap_or_default())
        .allow_empty(true)
        .interact_text()?;

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

    println!();

    let config = GoldClawConfig {
        version: existing.version,
        profile: existing.profile.clone(),
        agent: AgentSettings {
            name: agent_name,
            personality,
            style,
        },
        gateway: GatewaySettings {
            bind,
            allowed_origins: existing.gateway.allowed_origins,
        },
        runtime: RuntimeSettings {
            read_roots: vec![PathBuf::from(read_root)],
        },
        provider: ProviderSettings {
            api_key: if api_key.is_empty() { None } else { Some(api_key) },
            model: existing.provider.model,
        },
    };

    let config = config.normalize()?;
    config.save(&config_path)?;
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
    println!("下一步可以运行 `goldclaw start` 启动后台服务。");
    Ok(())
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
    let mut command = Command::new(exe);
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
            println!(
                "GoldClaw gateway 已启动: {} (pid {})",
                config.gateway.bind,
                child.id()
            );
            return Ok(());
        }
    }

    bail!(
        "gateway process spawned (pid {}), but port {} did not become reachable; inspect {}",
        child.id(),
        config.gateway.bind,
        log_path.display()
    );
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
        config.agent.system_prompt(),
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

    let read_roots = if config.runtime.read_roots.is_empty() {
        vec![std::env::current_dir().context("failed to determine current directory")?]
    } else {
        config.runtime.read_roots.clone()
    };

    let store = SqliteStore::open(StoreLayout::from_project_paths(&paths))?;
    let provider = build_provider(&config);
    let runtime = InMemoryRuntime::with_store(
        provider,
        std::sync::Arc::new(StaticPolicy::allow_only(["read_file"])),
        vec![std::sync::Arc::new(ReadWorkspaceTool::new(read_roots))],
        store,
    )
    .await?;

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

async fn chat_run() -> Result<()> {
    let paths = ProjectPaths::discover()?;
    paths.ensure_all()?;
    let config = load_config(&paths)?;

    let read_roots = if config.runtime.read_roots.is_empty() {
        vec![std::env::current_dir().context("failed to determine current directory")?]
    } else {
        config.runtime.read_roots.clone()
    };

    let store = SqliteStore::open(StoreLayout::from_project_paths(&paths))?;
    let provider = build_provider(&config);
    let runtime = InMemoryRuntime::with_store(
        provider,
        std::sync::Arc::new(StaticPolicy::allow_only(["read_file"])),
        vec![std::sync::Arc::new(ReadWorkspaceTool::new(read_roots))],
        store,
    )
    .await?;

    let connector = Box::new(StdinConnector::default());
    connector.run(std::sync::Arc::new(runtime)).await?;
    Ok(())
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
