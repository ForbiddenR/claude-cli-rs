use claude_core::config::mcp::McpWsServerConfig;
use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::header::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[derive(Debug)]
pub struct WebSocketTransport {
    name: String,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WebSocketTransport {
    pub async fn connect(name: &str, cfg: &McpWsServerConfig) -> anyhow::Result<Self> {
        let mut request = cfg
            .url
            .trim()
            .into_client_request()
            .map_err(|e| anyhow::anyhow!("mcp server {name}: invalid url {}: {e}", cfg.url))?;

        // MCP SDK uses this subprotocol.
        request.headers_mut().insert(
            HeaderName::from_static("sec-websocket-protocol"),
            HeaderValue::from_static("mcp"),
        );

        if let Some(headers) = &cfg.headers {
            for (k, v) in headers {
                let name = HeaderName::from_bytes(k.as_bytes())
                    .map_err(|e| anyhow::anyhow!("invalid header name {k}: {e}"))?;
                let value = HeaderValue::from_str(v)
                    .map_err(|e| anyhow::anyhow!("invalid header value for {k}: {e}"))?;
                request.headers_mut().insert(name, value);
            }
        }

        let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;

        Ok(Self {
            name: name.to_string(),
            ws,
        })
    }

    pub async fn send_json(&mut self, value: &serde_json::Value) -> anyhow::Result<()> {
        let s = serde_json::to_string(value)?;
        self.ws.send(Message::Text(s)).await?;
        Ok(())
    }

    pub async fn next_json(&mut self) -> Option<anyhow::Result<serde_json::Value>> {
        while let Some(next) = self.ws.next().await {
            match next {
                Ok(Message::Text(text)) => {
                    let parsed: serde_json::Value = match serde_json::from_str(&text) {
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
                Ok(Message::Binary(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let parsed: serde_json::Value = match serde_json::from_str(&text) {
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
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => return None,
                Err(err) => return Some(Err(err.into())),
                _ => continue,
            }
        }
        None
    }

    pub async fn close(&mut self) {
        let _ = self.ws.close(None).await;
    }
}
