use std::time::Duration;

use claude_core::config::mcp::{
    McpServerConfig, McpSseServerConfig, McpStdioServerConfig, McpWsServerConfig,
};

use crate::mcp::transport::sse::SseTransport;
use crate::mcp::transport::stdio::StdioTransport;
use crate::mcp::transport::ws::WebSocketTransport;

pub mod sse;
pub mod stdio;
pub mod ws;

pub enum Transport {
    Stdio(StdioTransport),
    Sse(SseTransport),
    Ws(WebSocketTransport),
}

impl Transport {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> anyhow::Result<Self> {
        match cfg {
            McpServerConfig::Stdio(s) => Ok(Self::Stdio(StdioTransport::connect(name, s).await?)),
            McpServerConfig::Sse(s) => Ok(Self::Sse(SseTransport::connect(name, s).await?)),
            McpServerConfig::Ws(s) => Ok(Self::Ws(WebSocketTransport::connect(name, s).await?)),
        }
    }

    pub fn set_protocol_version(&mut self, version: &str) {
        match self {
            Self::Stdio(_) => {}
            Self::Sse(s) => s.set_protocol_version(version),
            Self::Ws(_) => {}
        }
    }

    pub async fn send_json(&mut self, value: &serde_json::Value) -> anyhow::Result<()> {
        match self {
            Self::Stdio(s) => s.send_json(value).await,
            Self::Sse(s) => s.send_json(value).await,
            Self::Ws(s) => s.send_json(value).await,
        }
    }

    pub async fn next_json(&mut self) -> Option<anyhow::Result<serde_json::Value>> {
        match self {
            Self::Stdio(s) => s.next_json().await,
            Self::Sse(s) => s.next_json().await,
            Self::Ws(s) => s.next_json().await,
        }
    }

    pub async fn close(&mut self) {
        match self {
            Self::Stdio(s) => s.close().await,
            Self::Sse(s) => s.close().await,
            Self::Ws(s) => s.close().await,
        }
    }

    pub fn recommended_timeout(&self) -> Duration {
        match self {
            Self::Stdio(_) => Duration::from_secs(60),
            Self::Sse(_) => Duration::from_secs(60),
            Self::Ws(_) => Duration::from_secs(60),
        }
    }
}

// Re-export config structs for convenience in callers.
pub type StdioConfig = McpStdioServerConfig;
pub type SseConfig = McpSseServerConfig;
pub type WsConfig = McpWsServerConfig;
