use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldClawConfig {
    pub version: u32,
    pub profile: String,
    pub gateway: GatewaySettings,
    pub runtime: RuntimeSettings,
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

fn default_allowed_origins() -> Vec<String> {
    vec!["http://127.0.0.1".into(), "http://localhost".into()]
}

impl Default for GoldClawConfig {
    fn default() -> Self {
        Self {
            version: 1,
            profile: "default".into(),
            gateway: GatewaySettings {
                bind: "127.0.0.1:4263".into(),
                allowed_origins: default_allowed_origins(),
            },
            runtime: RuntimeSettings::default(),
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
}

#[derive(Clone, Debug)]
pub struct ProjectPaths {
    dirs: ProjectDirs,
}

impl ProjectPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "GoldClaw", "GoldClaw")
            .ok_or(ConfigError::ProjectDirsUnavailable)?;
        Ok(Self { dirs })
    }

    pub fn ensure_all(&self) -> Result<()> {
        fs::create_dir_all(self.config_dir())?;
        fs::create_dir_all(self.data_dir())?;
        fs::create_dir_all(self.log_dir())?;
        Ok(())
    }

    pub fn config_dir(&self) -> &Path {
        self.dirs.config_dir()
    }

    pub fn data_dir(&self) -> &Path {
        self.dirs.data_local_dir()
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir().join("logs")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    pub fn runtime_state_file(&self) -> PathBuf {
        self.data_dir().join("gateway-state.json")
    }

    pub fn gateway_log_file(&self) -> PathBuf {
        self.log_dir().join("gateway.log")
    }
}
