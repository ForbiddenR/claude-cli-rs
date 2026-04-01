use std::{path::PathBuf, time::Duration};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ServicesError>;

#[derive(Debug, Error)]
pub enum ServicesError {
    #[error("I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("http error: {source}")]
    Http {
        #[from]
        source: reqwest::Error,
    },

    #[error("event stream error: {source}")]
    EventStream {
        #[from]
        source: reqwest_eventsource::Error,
    },

    #[error("failed to parse JSON: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error(transparent)]
    Core {
        #[from]
        source: claude_core::CoreError,
    },

    #[error("missing authentication: {detail}")]
    MissingAuth { detail: &'static str },

    #[error("api request failed with status {status}: {body}")]
    ApiStatus { status: u16, body: String },

    #[error("api key helper failed: {detail}")]
    ApiKeyHelper { detail: String },

    #[error("oauth token expired and no refresh token is available")]
    OAuthExpired,

    #[error("oauth token exchange failed: {detail}")]
    OAuthTokenExchange { detail: String },

    #[error("timeout acquiring lock {path:?} after {timeout:?}")]
    LockTimeout { path: PathBuf, timeout: Duration },

    #[error("invalid oauth redirect URL: {detail}")]
    InvalidOAuthRedirectUrl { detail: String },
}
