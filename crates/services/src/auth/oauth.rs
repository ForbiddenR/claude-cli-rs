use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use claude_core::config::global::GlobalConfig;
use oauth2::{CsrfToken, PkceCodeChallenge};
use url::Url;

use crate::{
    Result,
    ServicesError::{self, InvalidOAuthRedirectUrl, OAuthTokenExchange},
    auth::lockfile,
};

pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

const AUTHORIZE_URL: &str = "https://platform.claude.com/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";

// Union of the Console + Claude.ai scopes used by Claude Code.
const DEFAULT_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

#[derive(Debug, Clone)]
pub struct OAuthStart {
    pub authorize_url: String,
    pub code_verifier: String,
    pub state: String,
}

pub fn build_manual_oauth_authorize_url() -> OAuthStart {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let state = CsrfToken::new_random();

    let mut url = Url::parse(AUTHORIZE_URL).expect("authorize URL should be valid");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", MANUAL_REDIRECT_URL)
        .append_pair("scope", &DEFAULT_SCOPES.join(" "))
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state.secret());

    OAuthStart {
        authorize_url: url.to_string(),
        code_verifier: verifier.secret().to_string(),
        state: state.secret().to_string(),
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedOAuthRedirect {
    pub authorization_code: String,
    pub state: String,
}

pub fn parse_oauth_redirect_url(raw: &str) -> Result<ParsedOAuthRedirect> {
    let url = Url::parse(raw).map_err(|source| InvalidOAuthRedirectUrl {
        detail: source.to_string(),
    })?;

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.to_string()),
            "state" => state = Some(v.to_string()),
            _ => {}
        }
    }

    let Some(authorization_code) = code.filter(|s| !s.trim().is_empty()) else {
        return Err(InvalidOAuthRedirectUrl {
            detail: "missing ?code=... query parameter".to_string(),
        });
    };
    let Some(state) = state.filter(|s| !s.trim().is_empty()) else {
        return Err(InvalidOAuthRedirectUrl {
            detail: "missing ?state=... query parameter".to_string(),
        });
    };

    Ok(ParsedOAuthRedirect {
        authorization_code,
        state,
    })
}

pub async fn exchange_code_for_tokens(
    authorization_code: &str,
    state: &str,
    code_verifier: &str,
) -> Result<OAuthTokenResponse> {
    let http = reqwest::Client::new();

    let req = serde_json::json!({
        "grant_type": "authorization_code",
        "code": authorization_code,
        "redirect_uri": MANUAL_REDIRECT_URL,
        "client_id": CLIENT_ID,
        "code_verifier": code_verifier,
        "state": state,
    });

    let resp = http
        .post(TOKEN_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&req)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(OAuthTokenExchange {
            detail: format!("status={status} body={body}"),
        });
    }

    let data: OAuthTokenResponse = resp.json().await?;
    Ok(data)
}

pub async fn refresh_oauth_token(
    refresh_token: &str,
    scopes: Option<&[&str]>,
) -> Result<OAuthTokenResponse> {
    let http = reqwest::Client::new();

    let scope = (scopes.unwrap_or(DEFAULT_SCOPES)).join(" ");

    let req = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": scope,
    });

    let resp = http
        .post(TOKEN_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&req)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(OAuthTokenExchange {
            detail: format!("status={status} body={body}"),
        });
    }

    let data: OAuthTokenResponse = resp.json().await?;
    Ok(data)
}

pub fn is_oauth_token_expired(expires_at_ms: Option<u64>) -> bool {
    let Some(expires_at_ms) = expires_at_ms else {
        return false;
    };

    // 5 minute buffer to avoid clock drift.
    let buffer_ms = 5 * 60 * 1000u64;
    now_ms().saturating_add(buffer_ms) >= expires_at_ms
}

pub async fn ensure_valid_oauth_token(
    global_path: &Path,
    global_cfg: &mut GlobalConfig,
) -> Result<Option<String>> {
    let Some(token) = global_cfg.oauth_access_token.clone() else {
        return Ok(None);
    };

    if !is_oauth_token_expired(global_cfg.oauth_expires_at) {
        return Ok(Some(token));
    }

    let Some(refresh_token) = global_cfg.oauth_refresh_token.clone() else {
        return Err(ServicesError::OAuthExpired);
    };

    let lock_path = std::path::PathBuf::from(format!("{}.lock", global_path.display()));
    let _guard = lockfile::acquire_lock(&lock_path, Duration::from_secs(30)).await?;

    // Re-load inside the lock so we don't overwrite other refresh results.
    let mut fresh = claude_core::config::global::load_global_config(global_path)?;

    if let Some(tok) = fresh.oauth_access_token.clone() {
        if !is_oauth_token_expired(fresh.oauth_expires_at) {
            *global_cfg = fresh;
            return Ok(Some(tok));
        }
    }

    let refresh_token = fresh.oauth_refresh_token.clone().unwrap_or(refresh_token);

    let resp = refresh_oauth_token(&refresh_token, None).await?;

    fresh.oauth_access_token = Some(resp.access_token.clone());
    if let Some(rt) = resp.refresh_token.clone() {
        fresh.oauth_refresh_token = Some(rt);
    }
    fresh.oauth_expires_at = Some(now_ms().saturating_add(resp.expires_in.saturating_mul(1000)));

    claude_core::config::global::save_global_config(global_path, &fresh)?;
    *global_cfg = fresh;
    Ok(Some(resp.access_token))
}

fn now_ms() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_redirect_url_extracts_code_and_state() {
        let parsed = parse_oauth_redirect_url(
            "https://platform.claude.com/oauth/code/callback?code=abc123&state=st_456",
        )
        .expect("should parse");
        assert_eq!(parsed.authorization_code, "abc123");
        assert_eq!(parsed.state, "st_456");
    }

    #[test]
    fn parse_redirect_url_requires_code() {
        let err = parse_oauth_redirect_url(
            "https://platform.claude.com/oauth/code/callback?state=st_456",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("code"), "unexpected error: {msg}");
    }

    #[test]
    fn oauth_expiry_none_is_not_expired() {
        assert!(!is_oauth_token_expired(None));
    }
}
