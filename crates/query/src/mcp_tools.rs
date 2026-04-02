use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use claude_core::config::mcp::McpServerConfig;
use claude_services::mcp::{McpClient, McpTool as McpToolDef};
use claude_tools::{PermissionResult, Tool, ToolRef, ToolResult, ToolUseContext};

pub struct McpConnectResult {
    pub tools: Vec<ToolRef>,
    pub instructions: Vec<(String, String)>, // (server_name, instructions)
}

pub async fn connect_mcp_tools(servers: &HashMap<String, McpServerConfig>) -> McpConnectResult {
    let mut out_tools: Vec<ToolRef> = Vec::new();
    let mut instructions: Vec<(String, String)> = Vec::new();

    for (name, cfg) in servers {
        match McpClient::connect(name, cfg).await {
            Ok(conn) => {
                if let Some(instr) = conn.instructions.clone().filter(|s| !s.trim().is_empty()) {
                    instructions.push((name.clone(), instr));
                }

                match conn.client().list_tools().await {
                    Ok(tools) => {
                        for t in tools {
                            out_tools.push(Arc::new(McpToolAdapter::new(
                                name.clone(),
                                t,
                                conn.client(),
                            )));
                        }
                    }
                    Err(err) => {
                        // Non-fatal: skip tool registration for this server.
                        eprintln!("mcp: failed to list tools for {name}: {err}");
                    }
                }
            }
            Err(err) => {
                eprintln!("mcp: failed to connect {name}: {err}");
            }
        }
    }

    McpConnectResult {
        tools: out_tools,
        instructions,
    }
}

#[derive(Clone)]
struct McpToolAdapter {
    /// Tool name exposed to the model (fully-qualified).
    name: String,
    server_name: String,
    tool_name: String,
    description: String,
    input_schema: serde_json::Value,
    read_only_hint: bool,
    destructive_hint: bool,
    open_world_hint: bool,
    client: McpClient,
}

impl McpToolAdapter {
    fn new(server_name: String, tool: McpToolDef, client: McpClient) -> Self {
        let name = build_mcp_tool_name(&server_name, &tool.name);
        let read_only_hint = tool
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false);
        let destructive_hint = tool
            .annotations
            .as_ref()
            .and_then(|a| a.destructive_hint)
            .unwrap_or(false);
        let open_world_hint = tool
            .annotations
            .as_ref()
            .and_then(|a| a.open_world_hint)
            .unwrap_or(false);

        Self {
            name,
            server_name,
            tool_name: tool.name,
            description: tool.description.unwrap_or_default(),
            input_schema: tool.input_schema,
            read_only_hint,
            destructive_hint,
            open_world_hint,
            client,
        }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn prompt(&self) -> String {
        if self.description.trim().is_empty() {
            format!(
                "MCP tool '{}' from server '{}'.",
                self.tool_name, self.server_name
            )
        } else {
            self.description.clone()
        }
    }

    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        // Heuristic: allow read-only tools in default mode; require an explicit
        // "dangerous" permission mode for anything else.
        if self.read_only_hint && !self.open_world_hint && !self.destructive_hint {
            return PermissionResult::Allow;
        }

        if ctx.allows_dangerous_tools() {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(
                "MCP tools are disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            )
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let result = self
            .client
            .call_tool(&self.tool_name, Some(input), None)
            .await?;

        let is_error = result.is_error.unwrap_or(false);

        // MCP CallToolResult.content is an array of content blocks per spec.
        // We pass it through as raw JSON for maximum fidelity.
        let content = if result.content.is_null() {
            result
                .structured_content
                .unwrap_or_else(|| serde_json::Value::String(String::new()))
        } else {
            result.content
        };

        Ok(ToolResult { content, is_error })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        self.read_only_hint
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        self.read_only_hint
    }

    fn max_result_size_chars(&self) -> usize {
        // MCP tools can return large payloads; prefer persistence.
        100_000
    }
}

fn build_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        normalize_name_for_mcp(server_name),
        normalize_name_for_mcp(tool_name)
    )
}

fn normalize_name_for_mcp(name: &str) -> String {
    // Mirror TS normalizeNameForMCP: replace invalid chars with underscores.
    // API pattern: ^[a-zA-Z0-9_-]{1,64}$
    let mut normalized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    const CLAUDEAI_PREFIX: &str = "claude.ai ";
    if name.starts_with(CLAUDEAI_PREFIX) {
        // Collapse underscores and trim to avoid interfering with `__` delimiter.
        while normalized.contains("__") {
            normalized = normalized.replace("__", "_");
        }
        normalized = normalized.trim_matches('_').to_string();
    }

    normalized
}
