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
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
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
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

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
