use async_trait::async_trait;

use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TaskOutput";

#[derive(Debug, Default, Clone)]
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn aliases(&self) -> &[&'static str] {
        &["AgentOutputTool", "BashOutputTool"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "task_id": { "type": "string", "description": "The task ID to get output from" },
            "block": { "type": "boolean", "description": "Whether to wait for completion (no-op in Rust rewrite)", "default": true },
            "timeout": { "type": "integer", "minimum": 0, "maximum": 600000, "default": 30000, "description": "Max wait time in ms (no-op in Rust rewrite)" }
          },
          "required": ["task_id"]
        })
    }

    fn prompt(&self) -> String {
        "Retrieve output/logs from a task by ID (best-effort; headless Rust rewrite does not run background tasks yet).".to_string()
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let task_id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if task_id.is_empty() {
            return Ok(ToolResult::err_text("missing required field: task_id"));
        }

        let guard = ctx.session.tasks.lock().await;
        let Some(task) = guard.get(&task_id) else {
            return Ok(ToolResult::err_text(format!(
                "No task found with ID: {task_id}"
            )));
        };

        let mut out = String::new();
        out.push_str(&format!("Task #{}: {}\n", task.id, task.subject));
        out.push_str(&format!("Status: {}\n", task.status.as_str()));
        out.push_str(&format!("Description: {}\n", task.description));
        if !task.output.is_empty() {
            out.push_str("\nOutput:\n");
            out.push_str(&task.output.join("\n"));
            out.push('\n');
        }

        Ok(ToolResult::ok_text(out.trim_end().to_string()))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::TaskCreateTool;
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

    fn ctx_for(cwd: PathBuf) -> ToolUseContext {
        let store_dir = cwd.join(".claude-tools-test-results");
        ToolUseContext {
            cwd: cwd.clone(),
            allowed_roots: vec![cwd],
            permission_mode: PermissionMode::Default,
            session: Arc::new(crate::SessionState::default()),
            result_store: Arc::new(crate::ToolResultStore::new(store_dir).expect("store")),
            agent: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    async fn create_task(ctx: &mut ToolUseContext) -> String {
        let create = TaskCreateTool::default();
        let _ = create
            .call(
                serde_json::json!({
                    "subject": "Output task",
                    "description": "test",
                }),
                ctx,
            )
            .await
            .expect("create");

        let guard = ctx.session.tasks.lock().await;
        guard.keys().next().cloned().expect("task id")
    }

    #[tokio::test]
    async fn task_output_requires_task_id() {
        let cwd = temp_dir("task-output-missing");
        let mut ctx = ctx_for(cwd);
        let tool = TaskOutputTool::default();
        let res = tool
            .call(serde_json::json!({}), &mut ctx)
            .await
            .expect("call");
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn task_output_includes_output_lines_when_present() {
        let cwd = temp_dir("task-output");
        let mut ctx = ctx_for(cwd);
        let id = create_task(&mut ctx).await;

        {
            let mut guard = ctx.session.tasks.lock().await;
            let task = guard.get_mut(&id).expect("task");
            task.output.push("line one".to_string());
            task.output.push("line two".to_string());
        }

        let tool = TaskOutputTool::default();
        let res = tool
            .call(serde_json::json!({ "task_id": id }), &mut ctx)
            .await
            .expect("call");
        assert!(!res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("Output:"));
        assert!(out.contains("line one"));
        assert!(out.contains("line two"));
    }
}
