use std::collections::BTreeMap;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon is locked because the system keyring is unavailable")]
    Locked,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("database schema version {0} is newer than this hsind supports")]
    UnsupportedDatabaseVersion(i64),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cryptographic operation failed")]
    Crypto,
    #[error("system keyring error: {0}")]
    Keyring(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl DaemonError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Locked => "security_locked",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Invalid(_) => "invalid_input",
            Self::Config(_) => "config_error",
            Self::Protocol(_) => "protocol_error",
            Self::Database(_) => "database_error",
            Self::UnsupportedDatabaseVersion(_) => "database_version_unsupported",
            Self::Io(_) => "io_error",
            Self::Json(_) => "serialization_error",
            Self::Crypto => "crypto_error",
            Self::Keyring(_) => "keyring_error",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::Conflict(_) | Self::Io(_) | Self::Database(_))
    }

    pub fn wire(&self) -> WireError {
        WireError {
            code: self.code().to_owned(),
            args: BTreeMap::from([("message".to_owned(), self.to_string())]),
            retryable: self.retryable(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WireError {
    pub code: String,
    pub args: BTreeMap<String, String>,
    pub retryable: bool,
}

pub type Result<T> = std::result::Result<T, DaemonError>;
