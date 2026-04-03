use async_trait::async_trait;

use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Agent";

#[derive(Debug, Default, Clone)]
pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn aliases(&self) -> &[&'static str] {
        // Legacy name in Claude Code.
        &["Task"]
    }

    fn input_schema(&self) -> serde_json::Value {
        // Subset of the TS CLI Agent tool schema.
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "description": { "type": "string", "description": "A short (3-5 word) description of the task" },
            "prompt": { "type": "string", "description": "The task for the agent to perform" },
            "subagent_type": { "type": "string", "description": "Specialized agent type (ignored in Rust rewrite)" },
            "model": { "type": "string", "enum": ["sonnet","opus","haiku"], "description": "Model override (ignored in Rust rewrite)" },
            "run_in_background": { "type": "boolean", "description": "Run this agent in the background (not implemented in Rust rewrite)" },
            "name": { "type": "string", "description": "Name for the spawned agent (ignored)" },
            "team_name": { "type": "string", "description": "Team name (ignored)" },
            "mode": { "type": "string", "description": "Permission mode for spawned agent (ignored)" },
            "isolation": { "type": "string", "description": "Isolation mode (ignored)" },
            "cwd": { "type": "string", "description": "Override working directory for the agent (ignored)" }
          },
          "required": ["description", "prompt"]
        })
    }

    fn prompt(&self) -> String {
        "Spawn a sub-agent to handle a task. The Rust rewrite runs a nested headless query engine instance (synchronous).".to_string()
    }

    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        if ctx.allows_dangerous_tools() {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(
                "Agent is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            )
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        if input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Ok(ToolResult::err_text(
                "run_in_background is not implemented in the Rust rewrite",
            ));
        }

        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();

        if description.is_empty() || prompt.is_empty() {
            return Ok(ToolResult::err_text("description and prompt are required"));
        }

        if ctx.agent_depth >= ctx.max_agent_depth {
            return Ok(ToolResult::err_text(format!(
                "agent recursion depth exceeded (depth={} max={})",
                ctx.agent_depth, ctx.max_agent_depth
            )));
        }

        let Some(executor) = ctx.agent.clone() else {
            return Ok(ToolResult::err_text(
                "Agent tool is unavailable (no agent executor configured)",
            ));
        };

        let depth = ctx.agent_depth.saturating_add(1);
        let text = executor
            .run_agent(Some(description), prompt, depth)
            .await
            .unwrap_or_else(|e| format!("agent failed: {e}"));

        Ok(ToolResult::ok_text(text))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
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
            permission_mode: PermissionMode::BypassPermissions,
            session: Arc::new(crate::SessionState::default()),
            result_store: Arc::new(crate::ToolResultStore::new(store_dir).expect("store")),
            agent: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    #[tokio::test]
    async fn agent_tool_errors_when_no_executor_configured() {
        let cwd = temp_dir("agent-none");
        let mut ctx = ctx_for(cwd);
        let tool = AgentTool::default();

        let input = serde_json::json!({
            "description": "Test",
            "prompt": "Do something",
        });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("unavailable"));
    }

    #[tokio::test]
    async fn agent_tool_enforces_recursion_depth_cap() {
        let cwd = temp_dir("agent-depth");
        let mut ctx = ctx_for(cwd);
        ctx.agent_depth = 2;
        ctx.max_agent_depth = 2;

        let tool = AgentTool::default();
        let input = serde_json::json!({
            "description": "Test",
            "prompt": "Do something",
        });

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("recursion depth"));
    }

    #[tokio::test]
    async fn agent_tool_run_in_background_is_not_implemented() {
        let cwd = temp_dir("agent-bg");
        let mut ctx = ctx_for(cwd);
        let tool = AgentTool::default();

        let input = serde_json::json!({
            "description": "Test",
            "prompt": "Do something",
            "run_in_background": true,
        });

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("run_in_background"));
    }

    struct FakeAgent;

    #[async_trait]
    impl crate::AgentExecutor for FakeAgent {
        async fn run_agent(
            &self,
            description: Option<String>,
            prompt: String,
            depth: u32,
        ) -> anyhow::Result<String> {
            Ok(format!(
                "fake agent ok: desc={:?} prompt={} depth={}",
                description, prompt, depth
            ))
        }
    }

    #[tokio::test]
    async fn agent_tool_delegates_to_executor() {
        let cwd = temp_dir("agent-ok");
        let mut ctx = ctx_for(cwd);
        ctx.agent = Some(Arc::new(FakeAgent));

        let tool = AgentTool::default();
        let input = serde_json::json!({
            "description": "Unit test",
            "prompt": "Hello",
        });

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("fake agent ok"));
        assert!(out.contains("Hello"));
        assert!(out.contains("depth=1"));
    }
}
