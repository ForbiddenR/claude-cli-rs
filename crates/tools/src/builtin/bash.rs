use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Bash";

#[derive(Debug, Default, Clone)]
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "command": {
              "type": "string",
              "description": "The command to execute"
            },
            "timeout": {
              "type": "integer",
              "minimum": 1,
              "description": "Optional timeout in milliseconds"
            },
            "description": {
              "type": "string",
              "description": "Optional human-readable description"
            },
            "run_in_background": {
              "type": "boolean",
              "description": "Run this command in the background (not implemented in Rust rewrite yet)"
            },
            "dangerouslyDisableSandbox": {
              "type": "boolean",
              "description": "Unused in the Rust rewrite"
            }
          },
          "required": ["command"]
        })
    }

    fn prompt(&self) -> String {
        "Execute a shell command on the local machine. Use this for git, builds, and running programs. Prefer Read/Write/Edit/Glob/Grep for filesystem/search operations when possible.".to_string()
    }

    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        if ctx.allows_dangerous_tools() {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(
                "Bash tool is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            )
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if command.is_empty() {
            return Ok(ToolResult::err_text("missing required field: command"));
        }

        if input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Ok(ToolResult::err_text(
                "run_in_background is not implemented in the Rust rewrite",
            ));
        }

        let timeout_ms = input
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(60_000)
            .min(600_000);

        let mut cmd = build_shell_command(&command);
        cmd.current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let capture_limit = self.max_result_size_chars().saturating_mul(4).max(16_384);

        let stdout_task = tokio::spawn(async move {
            let mut out = Vec::new();
            if let Some(mut r) = stdout {
                read_stream_limited(&mut r, capture_limit, &mut out).await;
            }
            out
        });
        let stderr_task = tokio::spawn(async move {
            let mut out = Vec::new();
            if let Some(mut r) = stderr {
                read_stream_limited(&mut r, capture_limit, &mut out).await;
            }
            out
        });

        let status =
            match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                Ok(res) => res?,
                Err(_elapsed) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
                    return Ok(ToolResult::err_text(format!(
                        "command timed out after {timeout_ms}ms"
                    )));
                }
            };

        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();

        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);

        let code = status.code().unwrap_or(-1);
        let mut content = String::new();
        content.push_str("[stdout]\n");
        content.push_str(stdout.trim_end());
        content.push('\n');
        content.push_str("\n[stderr]\n");
        content.push_str(stderr.trim_end());
        content.push('\n');
        content.push_str(&format!("\n[exit_code]\n{code}\n"));

        Ok(ToolResult {
            content: serde_json::Value::String(content),
            is_error: code != 0,
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        // Shell commands can have broad side effects; keep execution ordered.
        false
    }
}

fn build_shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("bash");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

async fn read_stream_limited<R: AsyncReadExt + Unpin>(r: &mut R, limit: usize, out: &mut Vec<u8>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let remaining = limit.saturating_sub(out.len());
                if remaining > 0 {
                    let take = remaining.min(n);
                    out.extend_from_slice(&buf[..take]);
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::types::permissions::PermissionMode;
    use std::path::PathBuf;
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
    async fn bash_echo_includes_stdout() {
        let cwd = temp_dir("bash-echo");
        let mut ctx = ctx_for(cwd, PermissionMode::BypassPermissions);

        let tool = BashTool::default();
        let input = serde_json::json!({ "command": "echo hello" });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);

        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("[stdout]"));
        assert!(out.to_ascii_lowercase().contains("hello"));
        assert!(out.contains("[exit_code]"));
        assert!(out.contains("\n0\n"));
    }

    #[tokio::test]
    async fn bash_run_in_background_is_not_implemented() {
        let cwd = temp_dir("bash-bg");
        let mut ctx = ctx_for(cwd, PermissionMode::BypassPermissions);

        let tool = BashTool::default();
        let input = serde_json::json!({
            "command": "echo hello",
            "run_in_background": true,
        });

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("run_in_background"));
    }
}
