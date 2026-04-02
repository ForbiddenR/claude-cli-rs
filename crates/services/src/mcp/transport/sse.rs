use std::collections::HashMap;

use futures_util::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest_eventsource::{Event, EventSource};
use url::Url;

use claude_core::config::mcp::McpSseServerConfig;

use crate::mcp::protocol::LATEST_PROTOCOL_VERSION;

pub struct SseTransport {
    name: String,
    http: reqwest::Client,
    base_url: Url,
    endpoint: Url,
    es: EventSource,
    extra_headers: HashMap<String, String>,
    protocol_version: String,
}

impl SseTransport {
    pub async fn connect(name: &str, cfg: &McpSseServerConfig) -> anyhow::Result<Self> {
        let base_url = Url::parse(cfg.url.trim())
            .map_err(|e| anyhow::anyhow!("mcp server {name}: invalid url {}: {e}", cfg.url))?;

        let http = reqwest::Client::new();
        let extra_headers = cfg.headers.clone().unwrap_or_default();

        let protocol_version = LATEST_PROTOCOL_VERSION.to_string();
        let builder = http
            .get(base_url.clone())
            .headers(build_headers(&extra_headers, &protocol_version)?);

        let mut es = EventSource::new(builder)
            .map_err(|e| anyhow::anyhow!("mcp server {name}: cannot open SSE stream: {e}"))?;

        let endpoint = wait_for_endpoint(name, &base_url, &mut es).await?;

        Ok(Self {
            name: name.to_string(),
            http,
            base_url,
            endpoint,
            es,
            extra_headers,
            protocol_version,
        })
    }

    pub fn set_protocol_version(&mut self, version: &str) {
        if !version.trim().is_empty() {
            self.protocol_version = version.trim().to_string();
        }
    }

    pub async fn send_json(&mut self, value: &serde_json::Value) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(self.endpoint.clone())
            .headers(build_headers(&self.extra_headers, &self.protocol_version)?)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(value)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "mcp server {}: POST {} failed (HTTP {}): {}",
                self.name,
                self.endpoint,
                status.as_u16(),
                body
            );
        }

        Ok(())
    }

    pub async fn next_json(&mut self) -> Option<anyhow::Result<serde_json::Value>> {
        while let Some(next) = self.es.next().await {
            match next {
                Ok(Event::Open) => continue,
                Ok(Event::Message(msg)) => {
                    if msg.event == "endpoint" {
                        // Server can re-issue endpoints; update if valid.
                        match parse_endpoint(&self.name, &self.base_url, &msg.data) {
                            Ok(url) => self.endpoint = url,
                            Err(err) => return Some(Err(err)),
                        }
                        continue;
                    }

                    let parsed: serde_json::Value = match serde_json::from_str(&msg.data) {
                        Ok(v) => v,
                        Err(err) => {
                            return Some(Err(anyhow::anyhow!(
                                "mcp server {}: invalid JSON message: {err}",
                                self.name
                            )));
                        }
                    };
                    return Some(Ok(parsed));
                }
                Err(err) => return Some(Err(err.into())),
            }
        }
        None
    }

    pub async fn close(&mut self) {
        self.es.close();
    }
}

async fn wait_for_endpoint(name: &str, base: &Url, es: &mut EventSource) -> anyhow::Result<Url> {
    while let Some(next) = es.next().await {
        match next {
            Ok(Event::Open) => {}
            Ok(Event::Message(msg)) => {
                if msg.event == "endpoint" {
                    return parse_endpoint(name, base, &msg.data);
                }
                // Ignore other messages until the endpoint is known.
            }
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("mcp server {name}: SSE stream ended before endpoint was received");
}

fn parse_endpoint(name: &str, base: &Url, raw: &str) -> anyhow::Result<Url> {
    let endpoint = Url::parse(raw)
        .or_else(|_| base.join(raw))
        .map_err(|e| anyhow::anyhow!("mcp server {name}: invalid endpoint URL {raw}: {e}"))?;

    if endpoint.origin() != base.origin() {
        anyhow::bail!(
            "mcp server {name}: endpoint origin mismatch (base={:?}, endpoint={:?})",
            base.origin(),
            endpoint.origin()
        );
    }

    Ok(endpoint)
}

fn build_headers(
    extra: &HashMap<String, String>,
    protocol_version: &str,
) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    // Mirrors MCP SDK header name.
    headers.insert(
        HeaderName::from_static("mcp-protocol-version"),
        HeaderValue::from_str(protocol_version)?,
    );

    for (k, v) in extra {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid header name {k}: {e}"))?;
        let value = HeaderValue::from_str(v)
            .map_err(|e| anyhow::anyhow!("invalid header value for {k}: {e}"))?;
        headers.insert(name, value);
    }

    Ok(headers)
}
