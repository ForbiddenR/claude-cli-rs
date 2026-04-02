use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// JSON shape consumed by `--mcp-config` and Claude Code-compatible config files.
///
/// Example:
/// ```json
/// {
///   "mcpServers": {
///     "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"] }
///   }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpJsonConfig {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Stdio(McpStdioServerConfig),
    Sse(McpSseServerConfig),
    Ws(McpWsServerConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpStdioType {
    Stdio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpStdioServerConfig {
    /// Optional for backwards compatibility with older configs.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<McpStdioType>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpSseType {
    Sse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSseServerConfig {
    #[serde(rename = "type")]
    pub ty: McpSseType,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(
        default,
        rename = "headersHelper",
        skip_serializing_if = "Option::is_none"
    )]
    pub headers_helper: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpWsType {
    Ws,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpWsServerConfig {
    #[serde(rename = "type")]
    pub ty: McpWsType,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(
        default,
        rename = "headersHelper",
        skip_serializing_if = "Option::is_none"
    )]
    pub headers_helper: Option<String>,
}
