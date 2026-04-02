use async_trait::async_trait;

use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TaskList";

#[derive(Debug, Default, Clone)]
pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {}
        })
    }

    fn prompt(&self) -> String {
        "List tasks in the session task list.".to_string()
    }

    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let guard = ctx.session.tasks.lock().await;
        if guard.is_empty() {
            return Ok(ToolResult::ok_text("No tasks found"));
        }

        let mut lines: Vec<String> = Vec::with_capacity(guard.len());
        for task in guard.values() {
            let owner = task
                .owner
                .as_ref()
                .map(|o| format!(" ({o})"))
                .unwrap_or_default();
            let blocked = if task.blocked_by.is_empty() {
                String::new()
            } else {
                format!(
                    " [blocked by {}]",
                    task.blocked_by
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            lines.push(format!(
                "#{} [{}] {}{}{}",
                task.id,
                task.status.as_str(),
                task.subject,
                owner,
                blocked
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
