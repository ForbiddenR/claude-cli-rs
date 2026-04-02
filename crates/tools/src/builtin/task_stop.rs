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
