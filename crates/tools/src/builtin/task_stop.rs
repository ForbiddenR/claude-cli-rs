use async_trait::async_trait;

use crate::session::TaskStatus;
use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TaskStop";

#[derive(Debug, Default, Clone)]
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn aliases(&self) -> &[&'static str] {
        &["KillShell"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "task_id": { "type": "string", "description": "The ID of the task to stop" },
            "shell_id": { "type": "string", "description": "Deprecated: use task_id instead" }
          }
        })
    }

    fn prompt(&self) -> String {
        "Stop a running task by ID.".to_string()
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .or_else(|| input.get("shell_id").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .trim()
            .to_string();

        if id.is_empty() {
            return Ok(ToolResult::err_text("missing required parameter: task_id"));
        }

        let mut guard = ctx.session.tasks.lock().await;
        let Some(task) = guard.get_mut(&id) else {
            return Ok(ToolResult::err_text(format!("No task found with ID: {id}")));
        };

        task.status = TaskStatus::Stopped;
        Ok(ToolResult::ok_text(format!("Stopped task: #{id}")))
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
            current_tool_use_id: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    async fn create_task(ctx: &mut ToolUseContext) -> String {
        let create = TaskCreateTool::default();
        let _ = create
            .call(
                serde_json::json!({
                    "subject": "Stop me",
                    "description": "Please",
                }),
                ctx,
            )
            .await
            .expect("create");

        let guard = ctx.session.tasks.lock().await;
        guard.keys().next().cloned().expect("task id")
    }

    #[tokio::test]
    async fn task_stop_requires_task_id() {
        let cwd = temp_dir("task-stop-missing");
        let mut ctx = ctx_for(cwd);
        let tool = TaskStopTool::default();
        let res = tool
            .call(serde_json::json!({}), &mut ctx)
            .await
            .expect("call");
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn task_stop_sets_status_stopped() {
        let cwd = temp_dir("task-stop");
        let mut ctx = ctx_for(cwd);
        let id = create_task(&mut ctx).await;

        let tool = TaskStopTool::default();
        let res = tool
            .call(serde_json::json!({ "task_id": id.clone() }), &mut ctx)
            .await
            .expect("call");
        assert!(!res.is_error);

        let guard = ctx.session.tasks.lock().await;
        let task = guard.get(&id).expect("task");
        assert_eq!(task.status, TaskStatus::Stopped);
    }
}
