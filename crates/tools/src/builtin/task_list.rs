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

    #[tokio::test]
    async fn task_list_empty_returns_no_tasks() {
        let cwd = temp_dir("task-list-empty");
        let mut ctx = ctx_for(cwd);
        let tool = TaskListTool::default();
        let res = tool
            .call(serde_json::json!({}), &mut ctx)
            .await
            .expect("call");
        assert!(!res.is_error);
        assert_eq!(res.content.as_str().unwrap_or_default(), "No tasks found");
    }

    #[tokio::test]
    async fn task_list_includes_created_task() {
        let cwd = temp_dir("task-list");
        let mut ctx = ctx_for(cwd);

        let create = TaskCreateTool::default();
        let _ = create
            .call(
                serde_json::json!({
                    "subject": "List me",
                    "description": "Please",
                }),
                &mut ctx,
            )
            .await
            .expect("create");

        let tool = TaskListTool::default();
        let res = tool
            .call(serde_json::json!({}), &mut ctx)
            .await
            .expect("call");
        assert!(!res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("List me"));
        assert!(out.contains("[pending]"));
    }
}
