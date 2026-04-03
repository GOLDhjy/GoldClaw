use thiserror::Error;

pub type Result<T> = std::result::Result<T, GoldClawError>;

#[derive(Debug, Error)]
pub enum GoldClawError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<std::io::Error> for GoldClawError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}
