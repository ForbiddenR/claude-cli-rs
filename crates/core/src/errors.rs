use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("failed to parse JSON: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("invalid settings input: {detail}")]
    InvalidSettingsInput { detail: String },

    #[error("config directory not found (could not determine $HOME)")]
    NoHomeDir,

    #[error("failed to read config file: {path}")]
    ReadConfig { path: PathBuf },

    #[error("failed to write config file: {path}")]
    WriteConfig { path: PathBuf },

    #[error("timeout acquiring lock {path} after {timeout:?}")]
    LockTimeout { path: PathBuf, timeout: Duration },
}
