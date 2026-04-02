mod api_key;
mod auth_token;
mod lockfile;
mod oauth;

use std::path::Path;

use claude_core::{
    Result as CoreResult,
    config::{global::GlobalConfig, settings::Settings},
};

use crate::{Result, ServicesError};

pub use oauth::{
    OAuthStart, OAuthTokenResponse, ParsedOAuthRedirect, build_manual_oauth_authorize_url,
    exchange_code_for_tokens, parse_oauth_redirect_url,
};

#[derive(Debug, Clone)]
pub enum AuthMode {
    ApiKey(String),
    AuthToken(String),
    OAuthToken(String),
}

impl AuthMode {
    pub fn apply_headers(&self, headers: &mut reqwest::header::HeaderMap) -> Result<()> {
        match self {
            AuthMode::ApiKey(key) => {
                headers.insert(
                    "x-api-key",
                    reqwest::header::HeaderValue::from_str(key).map_err(|_source| {
                        ServicesError::MissingAuth {
                            detail: "api key contains invalid characters for an HTTP header",
                        }
                    })?,
                );
            }
            AuthMode::AuthToken(token) => {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(
                        |_source| ServicesError::MissingAuth {
                            detail: "auth token contains invalid characters for an HTTP header",
                        },
                    )?,
                );
            }
            AuthMode::OAuthToken(token) => {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(
                        |_source| ServicesError::MissingAuth {
                            detail: "oauth token contains invalid characters for an HTTP header",
                        },
                    )?,
                );
                headers.insert(
                    "anthropic-beta",
                    reqwest::header::HeaderValue::from_static(oauth::OAUTH_BETA_HEADER),
                );
            }
        }
        Ok(())
    }
}

pub struct ResolveAuthOpts<'a> {
    pub cli_api_key: Option<&'a str>,
    pub bare: bool,
}

pub async fn resolve_auth(
    global_path: &Path,
    global_cfg: &mut GlobalConfig,
    settings: &Settings,
    opts: ResolveAuthOpts<'_>,
) -> Result<AuthMode> {
    // --bare: hermetic auth. No OAuth.
    if opts.bare {
        let key = resolve_api_key(global_cfg, settings, opts.cli_api_key).await?;
        let Some(key) = key else {
            return Err(ServicesError::MissingAuth {
                detail: "set ANTHROPIC_API_KEY or configure settings.api_key_helper",
            });
        };
        return Ok(AuthMode::ApiKey(key));
    }

    if let Some(token) = auth_token::auth_token_from_env() {
        return Ok(AuthMode::AuthToken(token));
    }

    if let Ok(token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(AuthMode::OAuthToken(token));
        }
    }

    // Prefer explicit API keys over implicit OAuth state.
    if opts.cli_api_key.is_some() || api_key::api_key_from_env().is_some() {
        let key = resolve_api_key(global_cfg, settings, opts.cli_api_key).await?;
        if let Some(key) = key {
            return Ok(AuthMode::ApiKey(key));
        }
    }

    if global_cfg.oauth_access_token.is_some() {
        let token = oauth::ensure_valid_oauth_token(global_path, global_cfg).await?;
        if let Some(token) = token {
            return Ok(AuthMode::OAuthToken(token));
        }
    }

    if let Some(key) = resolve_api_key(global_cfg, settings, opts.cli_api_key).await? {
        return Ok(AuthMode::ApiKey(key));
    }

    Err(ServicesError::MissingAuth {
        detail: "no auth token, oauth token, or api key available",
    })
}

async fn resolve_api_key(
    global_cfg: &GlobalConfig,
    settings: &Settings,
    cli_api_key: Option<&str>,
) -> Result<Option<String>> {
    if let Some(k) = cli_api_key {
        let k = k.trim();
        if !k.is_empty() {
            return Ok(Some(k.to_string()));
        }
    }

    if let Some(k) = api_key::api_key_from_env() {
        return Ok(Some(k));
    }

    if let Some(helper) = settings.api_key_helper.as_deref() {
        let key = api_key::api_key_from_helper(helper).await?;
        return Ok(Some(key));
    }

    if let Some(k) = global_cfg.api_key.as_deref() {
        let k = k.trim();
        if !k.is_empty() {
            return Ok(Some(k.to_string()));
        }
    }

    Ok(None)
}

pub fn clear_oauth_tokens(cfg: &mut GlobalConfig) {
    cfg.oauth_access_token = None;
    cfg.oauth_refresh_token = None;
    cfg.oauth_expires_at = None;
}

pub fn clear_api_key(cfg: &mut GlobalConfig) {
    cfg.api_key = None;
}

pub fn save_global_config(path: &Path, cfg: &GlobalConfig) -> CoreResult<()> {
    claude_core::config::global::save_global_config(path, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn prefers_anthropic_auth_token_over_other_sources() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_AUTH_TOKEN", "  tok_123  ");
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "oauth_456");
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-abc");
        }

        let mut global_cfg = GlobalConfig::default();
        let settings = Settings::default();

        let auth = resolve_auth(
            Path::new("/tmp/.claude.json"),
            &mut global_cfg,
            &settings,
            ResolveAuthOpts {
                cli_api_key: None,
                bare: false,
            },
        )
        .await
        .expect("should resolve");

        match auth {
            AuthMode::AuthToken(tok) => assert_eq!(tok, "tok_123"),
            other => panic!("expected AuthToken, got {other:?}"),
        }

        unsafe {
            std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
            std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
    }

    #[tokio::test]
    async fn bare_mode_ignores_auth_token_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_AUTH_TOKEN", "tok_123");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let mut global_cfg = GlobalConfig::default();
        let settings = Settings::default();

        let err = resolve_auth(
            Path::new("/tmp/.claude.json"),
            &mut global_cfg,
            &settings,
            ResolveAuthOpts {
                cli_api_key: None,
                bare: true,
            },
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("ANTHROPIC_API_KEY"), "unexpected error: {msg}");

        unsafe {
            std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
        }
    }

    #[test]
    fn api_key_env_is_trimmed() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "  sk-ant-abc  ");
        }

        assert_eq!(api_key::api_key_from_env().as_deref(), Some("sk-ant-abc"));

        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
    }
}
