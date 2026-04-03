use async_trait::async_trait;

use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TaskGet";

#[derive(Debug, Default, Clone)]
pub struct TaskGetTool;

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "taskId": { "type": "string", "description": "The ID of the task to retrieve" }
          },
          "required": ["taskId"]
        })
    }

    fn prompt(&self) -> String {
        "Retrieve a task by ID from the session task list.".to_string()
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let task_id = input
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if task_id.is_empty() {
            return Ok(ToolResult::err_text("missing required field: taskId"));
        }

        let guard = ctx.session.tasks.lock().await;
        let Some(task) = guard.get(&task_id) else {
            return Ok(ToolResult::ok_text("Task not found"));
        };

        let mut lines = Vec::new();
        lines.push(format!("Task #{}: {}", task.id, task.subject));
        lines.push(format!("Status: {}", task.status.as_str()));
        lines.push(format!("Description: {}", task.description));
        if !task.blocked_by.is_empty() {
            lines.push(format!(
                "Blocked by: {}",
                task.blocked_by
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !task.blocks.is_empty() {
            lines.push(format!(
                "Blocks: {}",
                task.blocks
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        Ok(ToolResult::ok_text(lines.join("\n")))
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

    #[tokio::test]
    async fn task_get_returns_not_found_for_missing_task() {
        let cwd = temp_dir("task-get");
        let mut ctx = ctx_for(cwd);
        let tool = TaskGetTool::default();

        let input = serde_json::json!({ "taskId": "does-not-exist" });
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);
        assert_eq!(res.content.as_str().unwrap_or_default(), "Task not found");
    }

    #[tokio::test]
    async fn task_get_requires_task_id() {
        let cwd = temp_dir("task-get-missing");
        let mut ctx = ctx_for(cwd);
        let tool = TaskGetTool::default();

        let input = serde_json::json!({ "taskId": "" });
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
    }
}
