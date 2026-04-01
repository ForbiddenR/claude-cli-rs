use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest_eventsource::{Event, EventSource};

use claude_core::types::message::{ContentBlock, Message, UserMessage};

use crate::api::types::MessagesRequest;
use crate::auth::AuthMode;
use crate::{Result, ServicesError};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(base_url: Option<String>) -> Self {
        let base_url = base_url
            .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    fn messages_url(&self) -> String {
        // Be forgiving if the base URL already includes `/v1`.
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    pub async fn stream_prompt<F>(
        &self,
        auth: &AuthMode,
        model: &str,
        max_tokens: u32,
        prompt: &str,
        mut on_event_json: F,
    ) -> Result<()>
    where
        F: FnMut(serde_json::Value) -> Result<()> + Send,
    {
        let user = Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        });

        let req = MessagesRequest {
            model: model.to_string(),
            max_tokens,
            system: None,
            messages: vec![user],
            stream: true,
        };

        self.stream_messages(auth, &req, &mut on_event_json).await
    }

    pub async fn stream_messages<F>(
        &self,
        auth: &AuthMode,
        req: &MessagesRequest,
        on_event_json: &mut F,
    ) -> Result<()>
    where
        F: FnMut(serde_json::Value) -> Result<()> + Send,
    {
        let url = self.messages_url();

        let mut attempt: usize = 0;
        loop {
            attempt += 1;

            let mut headers = HeaderMap::new();
            headers.insert(
                "anthropic-version",
                HeaderValue::from_static(ANTHROPIC_VERSION),
            );
            auth.apply_headers(&mut headers)?;

            let builder = self.http.post(&url).headers(headers).json(req);

            let mut es = EventSource::new(builder).map_err(|_err| ServicesError::ApiStatus {
                status: 0,
                body: "request body could not be cloned".to_string(),
            })?;

            while let Some(next) = es.next().await {
                match next {
                    Ok(Event::Open) => {
                        // Connection established.
                    }
                    Ok(Event::Message(msg)) => {
                        let raw: serde_json::Value = serde_json::from_str(&msg.data)?;
                        let ty = raw
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();

                        on_event_json(raw)?;

                        if ty == "message_stop" {
                            es.close();
                            return Ok(());
                        }
                    }
                    Err(err) => {
                        // Convert invalid status codes into actionable errors, and retry 429/529.
                        match err {
                            reqwest_eventsource::Error::InvalidStatusCode(status, resp) => {
                                let status_u16 = status.as_u16();

                                let retry_after_secs = resp
                                    .headers()
                                    .get(reqwest::header::RETRY_AFTER)
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|s| s.parse::<u64>().ok());

                                // Best-effort body read for diagnostics.
                                let body = resp.text().await.unwrap_or_default();

                                if (status_u16 == 429 || status_u16 == 529) && attempt < 6 {
                                    let backoff = retry_after_secs
                                        .map(Duration::from_secs)
                                        .unwrap_or_else(|| retry_backoff(attempt));
                                    tokio::time::sleep(backoff).await;
                                    break; // restart outer loop
                                }

                                return Err(ServicesError::ApiStatus {
                                    status: status_u16,
                                    body,
                                });
                            }
                            other => return Err(ServicesError::EventStream { source: other }),
                        }
                    }
                }
            }

            // If the stream ended without a message_stop, treat it as a failure.
            if attempt >= 6 {
                return Err(ServicesError::ApiStatus {
                    status: 0,
                    body: "stream ended unexpectedly".to_string(),
                });
            }
        }
    }
}

fn retry_backoff(attempt: usize) -> Duration {
    // Exponential-ish backoff with a cap.
    // attempt=1 => 250ms, 2 => 500ms, 3 => 1s, 4 => 2s, 5 => 4s
    let exp = (attempt.saturating_sub(1)).min(4) as u32;
    let base_ms = 250u64.saturating_mul(1u64 << exp);
    Duration::from_millis(base_ms.min(5_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn default_base_url_is_used_when_env_unset() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("ANTHROPIC_BASE_URL");
        }
        let client = AnthropicClient::new(None);
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
        assert_eq!(client.messages_url(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn env_base_url_is_used_when_provided() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", " https://example.com ");
        }
        let client = AnthropicClient::new(None);
        assert_eq!(client.base_url, "https://example.com");
        assert_eq!(client.messages_url(), "https://example.com/v1/messages");
        unsafe {
            std::env::remove_var("ANTHROPIC_BASE_URL");
        }
    }

    #[test]
    fn explicit_base_url_overrides_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", "https://example.com");
        }
        let client = AnthropicClient::new(Some("https://override.local".to_string()));
        assert_eq!(client.base_url, "https://override.local");
        assert_eq!(client.messages_url(), "https://override.local/v1/messages");
        unsafe {
            std::env::remove_var("ANTHROPIC_BASE_URL");
        }
    }

    #[test]
    fn base_url_ending_in_v1_does_not_duplicate_path() {
        let client = AnthropicClient::new(Some("https://proxy.local/v1/".to_string()));
        assert_eq!(client.messages_url(), "https://proxy.local/v1/messages");
    }
}
