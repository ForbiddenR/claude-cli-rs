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
        let tool = TaskCreateTool::default();
        let input = serde_json::json!({
            "subject": "Task A",
            "description": "Do it",
        });
        let res = tool.call(input, ctx).await.expect("create call");
        assert!(!res.is_error);

        let guard = ctx.session.tasks.lock().await;
        guard.keys().next().cloned().expect("task id")
    }

    #[tokio::test]
    async fn task_update_changes_status_and_subject() {
        let cwd = temp_dir("task-update");
        let mut ctx = ctx_for(cwd);
        let id = create_task(&mut ctx).await;

        let tool = TaskUpdateTool::default();
        let input = serde_json::json!({
            "taskId": id,
            "status": "in_progress",
            "subject": "Task A (updated)",
        });
        let res = tool.call(input, &mut ctx).await.expect("update call");
        assert!(!res.is_error);

        let guard = ctx.session.tasks.lock().await;
        let task = guard.values().next().expect("task");
        assert_eq!(task.subject, "Task A (updated)");
        assert_eq!(task.status.as_str(), "in_progress");
    }

    #[tokio::test]
    async fn task_update_delete_removes_task() {
        let cwd = temp_dir("task-delete");
        let mut ctx = ctx_for(cwd);
        let id = create_task(&mut ctx).await;

        let tool = TaskUpdateTool::default();
        let input = serde_json::json!({
            "taskId": id,
            "status": "deleted",
        });
        let res = tool.call(input, &mut ctx).await.expect("delete call");
        assert!(!res.is_error);

        let guard = ctx.session.tasks.lock().await;
        assert!(guard.is_empty());
    }
}
