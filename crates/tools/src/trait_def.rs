use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use claude_core::types::permissions::PermissionMode;

use crate::{SessionState, ToolResultStore};

#[async_trait]
pub trait AgentExecutor: Send + Sync {
    async fn run_agent(
        &self,
        tool_use_id: Option<String>,
        description: Option<String>,
        prompt: String,
        depth: u32,
    ) -> anyhow::Result<String>;
}

#[derive(Clone)]
pub struct ToolUseContext {
    /// The session working directory.
    pub cwd: PathBuf,

    /// Directories the model is allowed to access without additional permission.
    ///
    /// The headless Rust rewrite does not implement interactive permission prompts,
    /// so this is a coarse safety boundary.
    pub allowed_roots: Vec<PathBuf>,

    pub permission_mode: PermissionMode,

    /// Mutable per-session state (todos, tasks, etc).
    pub session: Arc<SessionState>,

    /// Where to persist large tool outputs so they remain accessible without
    /// blowing out the model context window.
    pub result_store: Arc<ToolResultStore>,

    /// Allows tools (Agent) to spawn sub-agents.
    pub agent: Option<Arc<dyn AgentExecutor>>,

    /// The currently-executing tool call id (Anthropic tool_use_id) if available.
    ///
    /// This is set by the query engine right before `Tool::call` and cleared
    /// afterwards. Tools can use it to correlate nested operations with the
    /// parent tool call (e.g., agent progress display).
    pub current_tool_use_id: Option<String>,

    /// Sub-agent recursion depth; 0 for the main session.
    pub agent_depth: u32,

    /// Safety cap on recursive agent spawning.
    pub max_agent_depth: u32,
}

impl ToolUseContext {
    pub fn allows_dangerous_tools(&self) -> bool {
        matches!(
            self.permission_mode,
            PermissionMode::BypassPermissions
                | PermissionMode::AcceptEdits
                | PermissionMode::DontAsk
        )
    }

    pub fn is_bypass_permissions(&self) -> bool {
        matches!(self.permission_mode, PermissionMode::BypassPermissions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    Allow,
    Deny { reason: String },
}

impl PermissionResult {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: serde_json::Value,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok_text(s: impl Into<String>) -> Self {
        Self {
            content: serde_json::Value::String(s.into()),
            is_error: false,
        }
    }

    pub fn err_text(s: impl Into<String>) -> Self {
        Self {
            content: serde_json::Value::String(s.into()),
            is_error: true,
        }
    }
}

pub type ToolRef = Arc<dyn Tool>;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    fn input_schema(&self) -> serde_json::Value;

    /// Description shown to the model in the tool definition.
    fn prompt(&self) -> String;

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult>;

    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn max_result_size_chars(&self) -> usize {
        50_000
    }
}
