use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::util::{absolutize, expand_tilde, is_path_allowed, normalize_path};
use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Write";

#[derive(Debug, Default, Clone)]
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "file_path": {
              "type": "string",
              "description": "The absolute path to the file to write"
            },
            "content": {
              "type": "string",
              "description": "The full content to write to the file"
            }
          },
          "required": ["file_path", "content"]
        })
    }

    fn prompt(&self) -> String {
        "Write a file to the local filesystem (create or overwrite).".to_string()
    }

    async fn check_permissions(
        &self,
        input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        if !ctx.allows_dangerous_tools() {
            return PermissionResult::deny(
                "Write is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            );
        }

        let Some(raw) = input.get("file_path").and_then(|v| v.as_str()) else {
            return PermissionResult::deny("missing required field: file_path");
        };
        let path = resolve_path(ctx, raw);
        if is_path_allowed(ctx, &path) {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(format!(
                "Write is not allowed outside the working directory. Path: {}",
                path.display()
            ))
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if file_path.trim().is_empty() {
            return Ok(ToolResult::err_text("missing required field: file_path"));
        }

        let path = resolve_path(ctx, file_path);
        let content_owned = content.to_string();

        let summary =
            tokio::task::spawn_blocking(move || write_file(path, &content_owned)).await??;
        Ok(ToolResult::ok_text(summary))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        // Writes are order-sensitive and should not run concurrently with other
        // filesystem mutations.
        false
    }
}

fn resolve_path(ctx: &ToolUseContext, raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw.trim());
    let abs = absolutize(&ctx.cwd, &expanded);
    normalize_path(&abs)
}

fn write_file(path: PathBuf, content: &str) -> anyhow::Result<String> {
    let existed = path.exists();

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!("failed to create parent dirs for {}: {e}", path.display())
            })?;
        }
    }

    fs::write(&path, content.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;

    let bytes = content.as_bytes().len();
    let kind = if existed { "updated" } else { "created" };
    Ok(format!("Wrote {} ({kind}, {bytes} bytes)", path.display()))
}
