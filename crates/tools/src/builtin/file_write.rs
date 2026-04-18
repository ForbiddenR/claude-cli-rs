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

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::types::permissions::PermissionMode;
    use std::sync::Arc;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("claude-tools-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn ctx_for(cwd: PathBuf, mode: PermissionMode) -> ToolUseContext {
        let store_dir = cwd.join(".claude-tools-test-results");
        ToolUseContext {
            cwd: cwd.clone(),
            allowed_roots: vec![cwd],
            permission_mode: mode,
            session: Arc::new(crate::SessionState::default()),
            result_store: Arc::new(crate::ToolResultStore::new(store_dir).expect("store")),
            agent: None,
            current_tool_use_id: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    #[tokio::test]
    async fn write_creates_file_under_cwd() {
        let cwd = temp_dir("write-cwd");
        let mut ctx = ctx_for(cwd.clone(), PermissionMode::AcceptEdits);

        let tool = WriteTool::default();
        let abs_path = cwd.join("sub").join("hello.txt");

        let input = serde_json::json!({
            "file_path": "sub/hello.txt",
            "content": "hi",
        });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);
        assert_eq!(
            std::fs::read_to_string(&abs_path).expect("read written file"),
            "hi"
        );
    }

    #[tokio::test]
    async fn write_is_denied_outside_cwd_in_non_bypass() {
        let cwd = temp_dir("write-deny");
        let ctx = ctx_for(cwd.clone(), PermissionMode::AcceptEdits);

        let tool = WriteTool::default();
        let outside_dir = temp_dir("write-outside");
        let outside_file = outside_dir.join("outside.txt");
        let input = serde_json::json!({
            "file_path": outside_file.to_string_lossy().to_string(),
            "content": "x",
        });

        assert!(!tool.check_permissions(&input, &ctx).await.is_allowed());
    }

    #[tokio::test]
    async fn bypass_permissions_allows_write_outside_cwd() {
        let cwd = temp_dir("write-bypass");
        let mut ctx = ctx_for(cwd.clone(), PermissionMode::BypassPermissions);

        let tool = WriteTool::default();
        let outside_dir = temp_dir("write-bypass-outside");
        let outside_file = outside_dir.join("outside.txt");
        let input = serde_json::json!({
            "file_path": outside_file.to_string_lossy().to_string(),
            "content": "ok",
        });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);
        assert_eq!(
            std::fs::read_to_string(&outside_file).expect("read written file"),
            "ok"
        );
    }
}
