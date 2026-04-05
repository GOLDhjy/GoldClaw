use std::{
    fs,
    io::ErrorKind,
    net::TcpStream,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use chrono::{DateTime, Utc};
use goldclaw_config::{ConfigOverrides, GoldClawConfig, ProjectPaths};
use goldclaw_store::{SqliteStore, StoreLayout, current_schema_version};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthSeverity {
    Info,
    Warning,
    Fatal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthCheckResult {
    pub id: String,
    pub status: HealthStatus,
    pub severity: HealthSeverity,
    pub summary: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoctorReport {
    pub generated_at: DateTime<Utc>,
    pub healthy: bool,
    pub checks: Vec<HealthCheckResult>,
}

impl DoctorReport {
    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == HealthStatus::Fail)
    }
}

#[derive(Debug, Deserialize)]
struct RuntimeState {
    pid: u32,
    bind: String,
    profile: String,
    started_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct ConnectorState {
    pid: u32,
    name: String,
    started_at: DateTime<Utc>,
}

pub fn run_doctor(paths: &ProjectPaths) -> DoctorReport {
    let mut checks = Vec::new();
    let config_path = paths.config_file();
    let config_exists = config_path.exists();

    checks.push(if config_exists {
        pass(
            "config_file",
            format!("配置文件存在: {}", config_path.display()),
            "GoldClaw 已初始化，配置文件可读。".into(),
        )
    } else {
        fail(
            "config_file",
            format!("缺少配置文件: {}", config_path.display()),
            "请先运行 `goldclaw init`。".into(),
        )
    });

    let raw_config = if config_exists {
        match GoldClawConfig::load(&config_path) {
            Ok(config) => {
                checks.push(pass(
                    "config_parse",
                    "配置文件语法有效".into(),
                    format!("已成功解析 `{}`。", config_path.display()),
                ));
                Some(config)
            }
            Err(error) => {
                checks.push(fail(
                    "config_parse",
                    "配置文件无法解析".into(),
                    error.to_string(),
                ));
                None
            }
        }
    } else {
        None
    };

    let store = StoreLayout::from_project_paths(paths);
    let required_dirs = vec![
        paths.config_dir(),
        paths.data_dir(),
        paths.log_dir(),
        paths.cache_dir(),
        paths.temp_dir(),
        paths.backup_dir(),
        paths.database_dir(),
    ];
    let missing_dirs = required_dirs
        .iter()
        .filter(|path| !path.exists())
        .cloned()
        .collect::<Vec<_>>();

    checks.push(if missing_dirs.is_empty() {
        pass(
            "local_paths",
            "本地目录结构已就绪".into(),
            format!(
                "数据库路径: {} | 备份目录: {}",
                store.paths().database_file.display(),
                store.paths().backup_dir.display()
            ),
        )
    } else {
        warn(
            "local_paths",
            "部分本地目录尚未创建".into(),
            format!("缺失目录: {}", join_paths(&missing_dirs)),
        )
    });

    let store_inspection = SqliteStore::inspect(&store);
    checks.push(match &store_inspection {
        Ok(inspection) if inspection.database_exists => pass(
            "database_file",
            "SQLite 数据库文件已存在".into(),
            format!(
                "数据库文件位于 {}，已应用 schema v{} / {}。",
                store.paths().database_file.display(),
                inspection.applied_schema_version,
                inspection.target_schema_version
            ),
        ),
        Ok(_) => warn(
            "database_file",
            "SQLite 数据库文件尚未创建".into(),
            format!(
                "预期路径: {}，目标 schema 版本 v{}。",
                store.paths().database_file.display(),
                current_schema_version()
            ),
        ),
        Err(error) => fail(
            "database_file",
            "SQLite 数据库状态不可读".into(),
            error.to_string(),
        ),
    });

    if let Ok(inspection) = &store_inspection {
        checks.push(if inspection.has_pending_migrations() {
            fail(
                "database_schema",
                "数据库存在未应用迁移".into(),
                format!(
                    "当前 schema v{}，目标 v{}。请重新运行 `goldclaw start` 或 `goldclaw init --force`。",
                    inspection.applied_schema_version, inspection.target_schema_version
                ),
            )
        } else if inspection.database_exists {
            pass(
                "database_schema",
                "数据库迁移已同步".into(),
                format!(
                    "当前 schema v{}，与目标版本一致。",
                    inspection.applied_schema_version
                ),
            )
        } else {
            warn(
                "database_schema",
                "数据库尚未初始化".into(),
                format!("目标 schema 版本 v{}。", inspection.target_schema_version),
            )
        });
    }

    if let Some(config) = raw_config {
        let config = config.apply_overrides(ConfigOverrides::from_env());

        checks.push(match config.validate_loopback_bind() {
            Ok(()) => pass(
                "gateway_bind",
                "gateway 绑定地址符合本地限制".into(),
                format!("当前绑定地址: {}", config.gateway.bind),
            ),
            Err(error) => fail(
                "gateway_bind",
                "gateway 绑定地址不合法".into(),
                error.to_string(),
            ),
        });

        checks.push(match config.validate_allowed_origins() {
            Ok(origins) => pass(
                "allowed_origins",
                "allowed origins 限制为本地来源".into(),
                format!("origin 列表: {}", origins.join(", ")),
            ),
            Err(error) => fail(
                "allowed_origins",
                "allowed origins 配置不合法".into(),
                error.to_string(),
            ),
        });

        checks.push(match config.resolve_read_roots() {
            Ok(roots) if roots.is_empty() => warn(
                "read_roots",
                "尚未配置 read roots".into(),
                "当前未配置受限读取根目录，`read_file` 工具将回退到运行目录。".into(),
            ),
            Ok(roots) => pass(
                "read_roots",
                "read roots 校验通过".into(),
                format!("受限读取目录: {}", join_paths(&roots)),
            ),
            Err(error) => fail(
                "read_roots",
                "read roots 配置不合法".into(),
                error.to_string(),
            ),
        });

        let gateway_reachable = port_open(&config.gateway.bind);
        checks.push(wecom_connector_config_check(&config));
        checks.push(wecom_connector_runtime_check(
            paths,
            &config,
            gateway_reachable,
        ));
        checks.push(gateway_runtime_check(paths, &config.gateway.bind));
    } else {
        checks.push(warn(
            "gateway_runtime",
            "跳过 gateway 运行状态检查".into(),
            "配置未通过解析，无法确定目标绑定地址。".into(),
        ));
    }

    DoctorReport {
        generated_at: Utc::now(),
        healthy: !checks
            .iter()
            .any(|check| check.status == HealthStatus::Fail),
        checks,
    }
}

fn gateway_runtime_check(paths: &ProjectPaths, bind: &str) -> HealthCheckResult {
    let runtime_state_path = paths.runtime_state_file();
    let runtime_state = load_runtime_state(&runtime_state_path);
    let port_reachable = port_open(bind);

    match (runtime_state, port_reachable) {
        (Some(Ok(state)), true) => pass(
            "gateway_runtime",
            "gateway 正在运行".into(),
            format!(
                "pid {} 正监听 {}，profile `{}`，启动于 {}。",
                state.pid, state.bind, state.profile, state.started_at
            ),
        ),
        (Some(Ok(state)), false) => warn(
            "gateway_runtime",
            "gateway 状态文件存在，但端口不可达".into(),
            format!(
                "记录的 pid 为 {}，绑定地址为 {}。建议运行 `goldclaw stop` 清理后重新 `goldclaw start`。",
                state.pid, state.bind
            ),
        ),
        (Some(Err(error)), _) => warn(
            "gateway_runtime",
            "gateway 状态文件损坏".into(),
            format!("无法解析 {}: {}", runtime_state_path.display(), error),
        ),
        (None, true) => warn(
            "gateway_runtime",
            "gateway 端口可达，但缺少状态文件".into(),
            format!(
                "端口 {} 可访问，但 {} 不存在。",
                bind,
                runtime_state_path.display()
            ),
        ),
        (None, false) => warn(
            "gateway_runtime",
            "gateway 当前未运行".into(),
            format!("绑定地址 {} 当前不可达。", bind),
        ),
    }
}

fn wecom_connector_config_check(config: &GoldClawConfig) -> HealthCheckResult {
    let Some(settings) = config.connectors.wecom.as_ref() else {
        return pass(
            "wecom_connector_config",
            "企微 connector 未配置".into(),
            "当前未保存企微 long-link connector 配置。".into(),
        );
    };

    if !settings.enabled {
        return pass(
            "wecom_connector_config",
            "企微 connector 已配置但未启用".into(),
            "如需在 `goldclaw start` 时自动启动，请将 `connectors.wecom.enabled` 设为 `true`。"
                .into(),
        );
    }

    let mut missing_fields = Vec::new();
    if settings.bot_id.trim().is_empty() {
        missing_fields.push("bot_id");
    }
    if settings
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        missing_fields.push("secret");
    }

    if !missing_fields.is_empty() {
        return fail(
            "wecom_connector_config",
            "企微 connector 已启用，但配置不完整".into(),
            format!(
                "缺少字段: {}。请重新运行 `goldclaw init` 或直接补全 `config.toml`。",
                missing_fields.join(", ")
            ),
        );
    }

    let mut detail = vec!["企微 long-link connector 已启用。".to_string()];
    if let Some(ws_url) = settings
        .ws_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        detail.push(format!("ws_url: {ws_url}"));
    }
    if let Some(scene) = settings.scene {
        detail.push(format!("scene: {scene}"));
    }
    if let Some(version) = settings
        .plug_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        detail.push(format!("plug_version: {version}"));
    }

    pass(
        "wecom_connector_config",
        "企微 connector 配置完整且已启用".into(),
        detail.join(" | "),
    )
}

fn wecom_connector_runtime_check(
    paths: &ProjectPaths,
    config: &GoldClawConfig,
    gateway_reachable: bool,
) -> HealthCheckResult {
    let state_path = paths.base_dir().join("connector-wecom-state.json");
    let enabled = config
        .connectors
        .wecom
        .as_ref()
        .map(|settings| settings.enabled)
        .unwrap_or(false);

    wecom_connector_runtime_check_with_state(
        &state_path,
        load_connector_state(&state_path),
        enabled,
        gateway_reachable,
    )
}

fn wecom_connector_runtime_check_with_state(
    state_path: &Path,
    state: Option<Result<ConnectorState, String>>,
    enabled: bool,
    gateway_reachable: bool,
) -> HealthCheckResult {
    if !enabled {
        return match state {
            Some(Ok(state)) if process_alive(state.pid) => warn(
                "wecom_connector_runtime",
                "企微 connector 已禁用，但进程仍在运行".into(),
                format!(
                    "检测到 pid {} 仍在运行。建议执行 `goldclaw stop`，或手动停止该 connector 进程。",
                    state.pid
                ),
            ),
            Some(Err(error)) => warn(
                "wecom_connector_runtime",
                "企微 connector 已禁用，但状态文件损坏".into(),
                format!("无法解析 {}: {}", state_path.display(), error),
            ),
            _ => pass(
                "wecom_connector_runtime",
                "企微 connector 未启用，跳过运行检查".into(),
                "当前不会在 `goldclaw start` 时自动拉起企微 connector。".into(),
            ),
        };
    }

    match state {
        Some(Ok(state)) if process_alive(state.pid) => pass(
            "wecom_connector_runtime",
            "企微 connector 正在运行".into(),
            format!(
                "connector `{}` 正以 pid {} 运行，启动于 {}。",
                state.name, state.pid, state.started_at
            ),
        ),
        Some(Ok(state)) => warn(
            "wecom_connector_runtime",
            "企微 connector 状态文件存在，但进程不可达".into(),
            format!(
                "记录的 pid 为 {}。建议运行 `goldclaw stop` 清理后重新 `goldclaw start`。",
                state.pid
            ),
        ),
        Some(Err(error)) => warn(
            "wecom_connector_runtime",
            "企微 connector 状态文件损坏".into(),
            format!("无法解析 {}: {}", state_path.display(), error),
        ),
        None if gateway_reachable => warn(
            "wecom_connector_runtime",
            "企微 connector 已启用，但尚未启动".into(),
            format!(
                "gateway 端口已可达，但 {} 不存在。请检查启动日志或重新执行 `goldclaw start`。",
                state_path.display()
            ),
        ),
        None => warn(
            "wecom_connector_runtime",
            "企微 connector 已启用，但 gateway 未运行".into(),
            "该 connector 会在 `goldclaw start` 时自动拉起；当前 gateway 不可达。".into(),
        ),
    }
}

fn load_runtime_state(path: &PathBuf) -> Option<Result<RuntimeState, String>> {
    match fs::read_to_string(path) {
        Ok(raw) => Some(serde_json::from_str(&raw).map_err(|error| error.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => Some(Err(error.to_string())),
    }
}

fn load_connector_state(path: &PathBuf) -> Option<Result<ConnectorState, String>> {
    match fs::read_to_string(path) {
        Ok(raw) => Some(serde_json::from_str(&raw).map_err(|error| error.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => Some(Err(error.to_string())),
    }
}

fn port_open(bind: &str) -> bool {
    let Ok(address) = bind.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .any(|line| !line.trim().is_empty() && !line.contains("No tasks"))
            })
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn pass(id: &str, summary: String, detail: String) -> HealthCheckResult {
    HealthCheckResult {
        id: id.into(),
        status: HealthStatus::Pass,
        severity: HealthSeverity::Info,
        summary,
        detail,
    }
}

fn warn(id: &str, summary: String, detail: String) -> HealthCheckResult {
    HealthCheckResult {
        id: id.into(),
        status: HealthStatus::Warn,
        severity: HealthSeverity::Warning,
        summary,
        detail,
    }
}

fn fail(id: &str, summary: String, detail: String) -> HealthCheckResult {
    HealthCheckResult {
        id: id.into(),
        status: HealthStatus::Fail,
        severity: HealthSeverity::Fatal,
        summary,
        detail,
    }
}

#[cfg(test)]
mod tests;
