use async_trait::async_trait;

use crate::session::TaskStatus;
use crate::{Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "TaskUpdate";

#[derive(Debug, Default, Clone)]
pub struct TaskUpdateTool;

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "taskId": { "type": "string", "description": "The ID of the task to update" },
            "subject": { "type": "string", "description": "New subject for the task" },
            "description": { "type": "string", "description": "New description for the task" },
            "activeForm": { "type": "string", "description": "Present continuous form shown while in_progress" },
            "status": { "type": "string", "description": "New status for the task", "enum": ["pending","in_progress","completed","stopped","deleted"] },
            "addBlocks": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that this task blocks" },
            "addBlockedBy": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that block this task" },
            "owner": { "type": "string", "description": "New owner for the task" },
            "metadata": { "type": "object", "description": "Metadata to attach/merge", "additionalProperties": true }
          },
          "required": ["taskId"]
        })
    }

    fn prompt(&self) -> String {
        "Update fields on an existing task.".to_string()
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

        let mut guard = ctx.session.tasks.lock().await;
        let Some(task) = guard.get_mut(&task_id) else {
            return Ok(ToolResult::err_text("Task not found"));
        };

        let mut updated: Vec<&'static str> = Vec::new();

        if let Some(subject) = input.get("subject").and_then(|v| v.as_str()) {
            let subject = subject.trim();
            if !subject.is_empty() && subject != task.subject {
                task.subject = subject.to_string();
                updated.push("subject");
            }
        }

        if let Some(description) = input.get("description").and_then(|v| v.as_str()) {
            let description = description.trim();
            if !description.is_empty() && description != task.description {
                task.description = description.to_string();
                updated.push("description");
            }
        }

        if let Some(active_form) = input.get("activeForm").and_then(|v| v.as_str()) {
            let active_form = active_form.trim();
            if !active_form.is_empty() && task.active_form.as_deref() != Some(active_form) {
                task.active_form = Some(active_form.to_string());
                updated.push("activeForm");
            }
        }

        if let Some(owner) = input.get("owner").and_then(|v| v.as_str()) {
            let owner = owner.trim();
            if !owner.is_empty() && task.owner.as_deref() != Some(owner) {
                task.owner = Some(owner.to_string());
                updated.push("owner");
            }
        }

        if let Some(add) = input.get("addBlocks").and_then(|v| v.as_array()) {
            for id in add.iter().filter_map(|v| v.as_str()) {
                let id = id.trim();
                if !id.is_empty() && !task.blocks.iter().any(|x| x == id) {
                    task.blocks.push(id.to_string());
                }
            }
            updated.push("blocks");
        }

        if let Some(add) = input.get("addBlockedBy").and_then(|v| v.as_array()) {
            for id in add.iter().filter_map(|v| v.as_str()) {
                let id = id.trim();
                if !id.is_empty() && !task.blocked_by.iter().any(|x| x == id) {
                    task.blocked_by.push(id.to_string());
                }
            }
            updated.push("blockedBy");
        }

        if let Some(meta) = input.get("metadata") {
            task.metadata = Some(meta.clone());
            updated.push("metadata");
        }

        if let Some(status) = input.get("status").and_then(|v| v.as_str()) {
            if status == "deleted" {
                let subject = task.subject.clone();
                guard.remove(&task_id);
                return Ok(ToolResult::ok_text(format!(
                    "Task #{task_id} deleted successfully: {subject}"
                )));
            }

            if let Some(st) = TaskStatus::from_str(status) {
                if st != task.status {
                    task.status = st;
                    updated.push("status");
                }
            } else {
                return Ok(ToolResult::err_text(format!("invalid status: {status}")));
            }
        }

        let msg = if updated.is_empty() {
            format!("Task #{task_id} unchanged")
        } else {
            format!("Task #{task_id} updated ({})", updated.join(", "))
        };

        Ok(ToolResult::ok_text(msg))
    }
}
