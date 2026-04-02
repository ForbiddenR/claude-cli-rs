use async_trait::async_trait;

use crate::session::Task;
use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TaskCreate";

#[derive(Debug, Default, Clone)]
pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "subject": { "type": "string", "description": "A brief title for the task" },
            "description": { "type": "string", "description": "What needs to be done" },
            "activeForm": {
              "type": "string",
              "description": "Present continuous form shown while in_progress (e.g., \"Running tests\")"
            },
            "metadata": {
              "type": "object",
              "description": "Arbitrary metadata to attach to the task",
              "additionalProperties": true
            }
          },
          "required": ["subject", "description"]
        })
    }

    fn prompt(&self) -> String {
        "Create a task in the session task list. Use TaskUpdate/TaskList/TaskGet to manage tasks."
            .to_string()
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let subject = input
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if subject.is_empty() || description.is_empty() {
            return Ok(ToolResult::err_text("subject and description are required"));
        }

        let active_form = input
            .get("activeForm")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let metadata = input.get("metadata").cloned();

        let task = Task::new(subject.clone(), description, active_form, metadata);
        let id = task.id.clone();

        let mut guard = ctx.session.tasks.lock().await;
        guard.insert(id.clone(), task);

        Ok(ToolResult::ok_text(format!(
            "Task #{id} created successfully: {subject}"
        )))
    }
}
