use std::{
    env, fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("goldclaw project directories are unavailable on this platform")]
    ProjectDirsUnavailable,
    #[error("config path does not exist: {0}")]
    MissingConfig(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("invalid socket address `{0}`")]
    InvalidSocketAddress(String),
    #[error("gateway bind address must stay on loopback, got `{0}`")]
    NonLoopbackBind(String),
    #[error("origin `{0}` must use http or https")]
    InvalidOriginScheme(String),
    #[error("origin `{0}` must stay on localhost or loopback")]
    NonLocalOrigin(String),
    #[error("read root does not exist: {0}")]
    MissingReadRoot(PathBuf),
    #[error("read root is not a directory: {0}")]
    InvalidReadRoot(PathBuf),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub profile: Option<String>,
    pub gateway_bind: Option<String>,
    pub allowed_origins: Option<Vec<String>>,
    pub read_roots: Option<Vec<PathBuf>>,
}

impl ConfigOverrides {
    pub fn from_env() -> Self {
        Self {
            profile: env::var("GOLDCLAW_PROFILE")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            gateway_bind: env::var("GOLDCLAW_GATEWAY_BIND")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            allowed_origins: env::var("GOLDCLAW_ALLOWED_ORIGINS")
                .ok()
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|origins| !origins.is_empty()),
            read_roots: env::var_os("GOLDCLAW_READ_ROOTS")
                .map(|value| env::split_paths(&value).collect::<Vec<_>>())
                .filter(|roots| !roots.is_empty()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Display name of the agent.
    #[serde(default = "default_agent_name")]
    pub name: String,
    /// Personality description shown to the LLM as part of the system prompt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub personality: String,
    /// Speaking style instructions shown to the LLM as part of the system prompt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub style: String,
}

fn default_agent_name() -> String {
    "GoldClaw".into()
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            name: default_agent_name(),
            personality: String::new(),
            style: String::new(),
        }
    }
}

impl AgentSettings {
    /// Build a system-prompt string from the agent settings.
    /// Returns `None` if no meaningful content is set.
    pub fn system_prompt(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if !self.name.is_empty() {
            parts.push(format!("你是{}。", self.name));
        }
        if !self.personality.is_empty() {
            parts.push(self.personality.clone());
        }
        if !self.style.is_empty() {
            parts.push(self.style.clone());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldClawConfig {
    pub version: u32,
    pub profile: String,
    #[serde(default)]
    pub agent: AgentSettings,
    pub gateway: GatewaySettings,
    pub runtime: RuntimeSettings,
    #[serde(default)]
    pub provider: ProviderSettings,
    #[serde(default)]
    pub connectors: ConnectorSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewaySettings {
    pub bind: String,
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeSettings {
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderSettings {
    /// BigModel (Zhipu AI) API key. Overridden by the `BIGMODEL_API_KEY` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Model name, e.g. `GLM-5.1`. Overridden by the `BIGMODEL_MODEL` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectorSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wecom: Option<WeComSettings>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WeComSettings {
    #[serde(default)]
    pub enabled: bool,
    pub bot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plug_version: Option<String>,
}

fn default_allowed_origins() -> Vec<String> {
    vec!["http://127.0.0.1".into(), "http://localhost".into()]
}

impl Default for GoldClawConfig {
    fn default() -> Self {
        Self {
            version: 1,
            profile: "default".into(),
            agent: AgentSettings::default(),
            gateway: GatewaySettings {
                bind: "127.0.0.1:4263".into(),
                allowed_origins: default_allowed_origins(),
            },
            runtime: RuntimeSettings::default(),
            provider: ProviderSettings::default(),
            connectors: ConnectorSettings::default(),
        }
    }
}

impl GoldClawConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(ConfigError::MissingConfig(path.to_path_buf()));
        }

        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn load_resolved(path: &Path) -> Result<Self> {
        let config = Self::load(path)?;
        config
            .apply_overrides(ConfigOverrides::from_env())
            .normalize()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let raw = toml::to_string_pretty(self)?;
        fs::write(path, raw)?;
        Ok(())
    }

    pub fn bind_addr(&self) -> Result<SocketAddr> {
        self.gateway
            .bind
            .parse()
            .map_err(|_| ConfigError::InvalidSocketAddress(self.gateway.bind.clone()))
    }

    pub fn validate_loopback_bind(&self) -> Result<()> {
        let addr = self.bind_addr()?;
        if !addr.ip().is_loopback() {
            return Err(ConfigError::NonLoopbackBind(self.gateway.bind.clone()));
        }
        Ok(())
    }

    pub fn validate_allowed_origins(&self) -> Result<Vec<String>> {
        normalize_origins(&self.gateway.allowed_origins)
    }

    pub fn resolve_read_roots(&self) -> Result<Vec<PathBuf>> {
        normalize_read_roots(&self.runtime.read_roots)
    }

    pub fn apply_overrides(mut self, overrides: ConfigOverrides) -> Self {
        if let Some(profile) = overrides.profile {
            self.profile = profile;
        }
        if let Some(bind) = overrides.gateway_bind {
            self.gateway.bind = bind;
        }
        if let Some(origins) = overrides.allowed_origins {
            self.gateway.allowed_origins = origins;
        }
        if let Some(read_roots) = overrides.read_roots {
            self.runtime.read_roots = read_roots;
        }
        self
    }

    pub fn normalize(mut self) -> Result<Self> {
        self.validate_loopback_bind()?;
        self.gateway.allowed_origins = self.validate_allowed_origins()?;
        self.runtime.read_roots = self.resolve_read_roots()?;
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct ProjectPaths {
    base: PathBuf,
}

impl ProjectPaths {
    pub fn discover() -> Result<Self> {
        let home = BaseDirs::new()
            .ok_or(ConfigError::ProjectDirsUnavailable)?
            .home_dir()
            .to_path_buf();
        Ok(Self {
            base: home.join(".goldclaw"),
        })
    }

    pub fn ensure_all(&self) -> Result<()> {
        fs::create_dir_all(&self.base)?;
        fs::create_dir_all(self.log_dir())?;
        fs::create_dir_all(self.temp_dir())?;
        fs::create_dir_all(self.backup_dir())?;
        Ok(())
    }

    pub fn base_dir(&self) -> &Path {
        &self.base
    }

    /// Config, data, and cache all live in the same base directory.
    pub fn config_dir(&self) -> PathBuf {
        self.base.clone()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.base.clone()
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.base.clone()
    }

    pub fn log_dir(&self) -> PathBuf {
        self.base.join("logs")
    }

    pub fn temp_dir(&self) -> PathBuf {
        self.base.join("tmp")
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.base.join("backups")
    }

    pub fn database_dir(&self) -> PathBuf {
        self.base.clone()
    }

    pub fn database_file(&self) -> PathBuf {
        self.base.join("goldclaw.sqlite3")
    }

    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.toml")
    }

    pub fn runtime_state_file(&self) -> PathBuf {
        self.base.join("gateway-state.json")
    }

    pub fn soul_path(&self) -> PathBuf {
        self.base.join("soul.md")
    }

    pub fn gateway_log_file(&self) -> PathBuf {
        self.base.join("logs").join("gateway.log")
    }
}

fn normalize_origins(origins: &[String]) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(origins.len());
    for origin in origins {
        let parsed = Url::parse(origin).map_err(|_| ConfigError::NonLocalOrigin(origin.clone()))?;
        match parsed.scheme() {
            "http" | "https" => {}
            _ => return Err(ConfigError::InvalidOriginScheme(origin.clone())),
        }

        let is_local = match parsed.host_str() {
            Some("localhost") => true,
            Some(host) => host
                .parse::<IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false),
            None => false,
        };

        if !is_local {
            return Err(ConfigError::NonLocalOrigin(origin.clone()));
        }

        normalized.push(origin.clone());
    }
    Ok(normalized)
}

fn normalize_read_roots(read_roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut normalized = Vec::with_capacity(read_roots.len());
    for root in read_roots {
        if !root.exists() {
            return Err(ConfigError::MissingReadRoot(root.clone()));
        }
        if !root.is_dir() {
            return Err(ConfigError::InvalidReadRoot(root.clone()));
        }
        let canonical = fs::canonicalize(root)?;
        if !normalized.contains(&canonical) {
            normalized.push(canonical);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests;
