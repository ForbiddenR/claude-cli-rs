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
