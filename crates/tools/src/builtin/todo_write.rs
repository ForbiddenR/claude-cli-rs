use async_trait::async_trait;

use crate::session::{TodoItem, TodoStatus};
use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TodoWrite";

#[derive(Debug, Default, Clone)]
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "todos": {
              "type": "array",
              "description": "The updated todo list",
              "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                  "content": { "type": "string" },
                  "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                  "activeForm": { "type": "string" }
                },
                "required": ["content", "status", "activeForm"]
              }
            }
          },
          "required": ["todos"]
        })
    }

    fn prompt(&self) -> String {
        "Update the todo list for the current session. Use this to track multi-step work and update statuses as you progress.".to_string()
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(todos) = input.get("todos").and_then(|v| v.as_array()) else {
            return Ok(ToolResult::err_text("missing required field: todos"));
        };

        let mut parsed: Vec<TodoItem> = Vec::with_capacity(todos.len());
        for t in todos {
            let Some(obj) = t.as_object() else {
                return Ok(ToolResult::err_text("todos items must be objects"));
            };

            let content = obj
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .and_then(TodoStatus::from_str);
            let active_form = obj
                .get("activeForm")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();

            if content.is_empty() || active_form.is_empty() || status.is_none() {
                return Ok(ToolResult::err_text(
                    "each todo requires content, status, and activeForm",
                ));
            }

            parsed.push(TodoItem {
                content,
                active_form,
                status: status.unwrap(),
            });
        }

        let all_done = !parsed.is_empty()
            && parsed
                .iter()
                .all(|t| matches!(t.status, TodoStatus::Completed));

        let mut guard = ctx.session.todos.lock().await;
        let old_len = guard.len();
        if all_done {
            guard.clear();
        } else {
            *guard = parsed;
        }

        Ok(ToolResult::ok_text(format!(
            "Todos updated (previously {old_len} items). Continue using the todo list to track progress."
        )))
    }
}
