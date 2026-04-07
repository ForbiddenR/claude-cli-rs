use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use claude_core::config::mcp::McpServerConfig;
use claude_core::types::message::{
    AssistantMessage, ContentBlock, Message, StopReason, TokenUsage, UserMessage,
};
use claude_core::types::permissions::PermissionMode;

use claude_services::ServicesError;
use claude_services::api::{AnthropicClient, MessagesRequest, ToolDefinition};
use claude_services::auth::AuthMode;

use claude_tools::registry::{ToolPoolOpts, assemble_tool_pool_with_extra};
use claude_tools::{
    AgentExecutor, PermissionResult, SessionState, ToolRegistry, ToolResultStore, ToolUseContext,
};

use crate::context::{ContextOpts, gather_context};
use crate::cost::calculate_usd_cost;
use crate::mcp_tools::connect_mcp_tools;
use crate::stream_parser::StreamParser;
use crate::system_prompt::{SystemPromptParts, build_system_prompt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    Deny,
    /// Allow this tool for the remainder of the current `QueryEngine::run*` call.
    /// (Persistence is handled by higher layers.)
    AlwaysAllowTool,
}

#[async_trait]
pub trait QueryObserver: Send + Sync {
    /// Called immediately before a tool is executed (after permission is granted).
    async fn on_tool_use_start(&self, _id: &str, _name: &str, _input: &serde_json::Value) {}

    /// Called after a tool completes (or is denied/invalid).
    async fn on_tool_use_result(
        &self,
        _id: &str,
        _name: &str,
        _input: &serde_json::Value,
        _result: &serde_json::Value,
        _is_error: bool,
    ) {
    }

    /// Request interactive permission for a tool call. Default is deny.
    async fn request_permission(
        &self,
        _id: &str,
        _name: &str,
        _input: &serde_json::Value,
    ) -> PermissionDecision {
        PermissionDecision::Deny
    }
}

#[derive(Debug, Clone)]
pub struct QueryEngineConfig {
    pub cwd: PathBuf,
    pub bare: bool,
    pub add_dirs: Vec<PathBuf>,

    /// Overrides the default system prompt entirely.
    pub system_prompt: Option<String>,
    /// Appends to the default/overridden system prompt.
    pub append_system_prompt: Option<String>,
    /// JSON Schema to enforce via instructions.
    pub json_schema: Option<String>,

    pub max_turns: u32,
    pub max_budget_usd: Option<f64>,

    // Week 4: tools + permissions
    pub permission_mode: PermissionMode,
    pub base_tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// Tools that should be auto-approved in `permissionMode=default`.
    ///
    /// Stored and matched case-insensitively.
    pub always_allow_tools: Vec<String>,

    // Week 5: MCP servers
    pub mcp_servers: HashMap<String, McpServerConfig>,

    // Internal: agent recursion tracking (used by the Agent tool).
    pub agent_depth: u32,
    pub max_agent_depth: u32,
}

pub struct QueryEngine {
    client: AnthropicClient,
    auth: AuthMode,
    model: String,
    max_tokens: u32,
    cfg: QueryEngineConfig,
    tool_opts: ToolPoolOpts,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub text: String,
    pub usage: TokenUsage,
    pub cost_usd: Option<f64>,
    pub turns: u32,
    pub model: String,
    pub stop_reason: Option<StopReason>,
    pub history: Vec<Message>,
    pub new_messages: Vec<Message>,
}

impl QueryEngine {
    pub fn new(
        client: AnthropicClient,
        auth: AuthMode,
        model: String,
        max_tokens: u32,
        cfg: QueryEngineConfig,
    ) -> anyhow::Result<Self> {
        let tool_opts = ToolPoolOpts {
            base_tools: cfg.base_tools.clone(),
            allowed_tools: cfg.allowed_tools.clone(),
            disallowed_tools: cfg.disallowed_tools.clone(),
        };

        Ok(Self {
            client,
            auth,
            model,
            max_tokens,
            cfg,
            tool_opts,
        })
    }

    /// Runs a one-shot headless session. The callback receives raw SSE event JSON.
    pub async fn run<F>(&self, prompt: &str, on_event: F) -> anyhow::Result<RunResult>
    where
        F: FnMut(&serde_json::Value) -> anyhow::Result<()> + Send,
    {
        let history: Vec<Message> = vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        })];
        self.run_with_history(history, on_event).await
    }

    pub async fn run_with_history<F>(
        &self,
        history: Vec<Message>,
        on_event: F,
    ) -> anyhow::Result<RunResult>
    where
        F: FnMut(&serde_json::Value) -> anyhow::Result<()> + Send,
    {
        self.run_with_history_inner(history, on_event, None).await
    }

    pub async fn run_with_history_observed<F>(
        &self,
        history: Vec<Message>,
        on_event: F,
        observer: Arc<dyn QueryObserver>,
    ) -> anyhow::Result<RunResult>
    where
        F: FnMut(&serde_json::Value) -> anyhow::Result<()> + Send,
    {
        self.run_with_history_inner(history, on_event, Some(observer))
            .await
    }

    pub async fn compact_history_now(&self, history: Vec<Message>) -> anyhow::Result<Vec<Message>> {
        compact_history(
            self,
            &history,
            COMPACT_KEEP_TAIL_MESSAGES,
            COMPACT_SUMMARY_MAX_TOKENS,
        )
        .await
    }

    async fn run_with_history_inner<F>(
        &self,
        mut history: Vec<Message>,
        mut on_event: F,
        observer: Option<Arc<dyn QueryObserver>>,
    ) -> anyhow::Result<RunResult>
    where
        F: FnMut(&serde_json::Value) -> anyhow::Result<()> + Send,
    {
        let ctx = gather_context(
            self.cfg.cwd.clone(),
            ContextOpts {
                bare: self.cfg.bare,
                add_dirs: self.cfg.add_dirs.clone(),
            },
        )?;

        let mut system = build_system_prompt(
            &ctx,
            SystemPromptParts {
                base: self.cfg.system_prompt.as_deref(),
                append: self.cfg.append_system_prompt.as_deref(),
                json_schema: self.cfg.json_schema.as_deref(),
                include_context: !self.cfg.bare,
            },
        );

        let mcp = connect_mcp_tools(&self.cfg.mcp_servers).await;
        if !mcp.instructions.is_empty() {
            system.push_str("\n\n# MCP Server Instructions\n");
            for (name, instr) in &mcp.instructions {
                system.push_str(&format!("\n## {name}\n\n{}\n", instr.trim()));
            }
            system = system.trim_end().to_string();
        }

        let tools = assemble_tool_pool_with_extra(mcp.tools, self.tool_opts.clone())?;

        let tool_defs = tools
            .metadata()
            .into_iter()
            .map(|m| ToolDefinition {
                name: m.name,
                description: m.description,
                input_schema: m.input_schema,
            })
            .collect::<Vec<_>>();

        let tool_defs_json_len = serde_json::to_string(&tool_defs)
            .map(|s| s.len())
            .unwrap_or_default();

        let max_turns = self.cfg.max_turns.max(1);
        let mut turns: u32 = 0;
        let mut combined_text = String::new();
        let mut combined_usage = TokenUsage::default();
        let mut combined_cost: Option<f64> = Some(0.0);
        let mut resolved_model: Option<String> = None;
        let mut new_messages: Vec<Message> = Vec::new();
        let mut last_stop_reason: Option<StopReason> = None;

        let session = Arc::new(SessionState::default());
        let (result_store, result_dir) = create_result_store(&self.cfg.cwd);

        let mut allowed_roots = build_allowed_roots(&self.cfg.cwd, &self.cfg.add_dirs);
        if !allowed_roots.iter().any(|p| p == &result_dir) {
            allowed_roots.push(result_dir);
        }

        let agent_exec: Arc<dyn AgentExecutor> = Arc::new(QueryAgentExecutor::new(
            self.client.clone(),
            self.auth.clone(),
            self.model.clone(),
            self.max_tokens,
            self.cfg.clone(),
        ));

        let mut tool_ctx = ToolUseContext {
            cwd: self.cfg.cwd.clone(),
            allowed_roots,
            permission_mode: self.cfg.permission_mode,
            session,
            result_store,
            agent: Some(agent_exec),
            agent_depth: self.cfg.agent_depth,
            max_agent_depth: self.cfg.max_agent_depth,
        };

        let mut did_reactive_compact: bool = false;
        let always_allow_seed: HashSet<String> = self
            .cfg
            .always_allow_tools
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let mut always_allow_tools: HashSet<String> = always_allow_seed.clone();

        loop {
            turns += 1;

            maybe_proactive_compact(
                self,
                &mut history,
                &system,
                tool_defs_json_len,
                self.max_tokens,
            )
            .await;

            let req = MessagesRequest {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                system: Some(system.clone()),
                tools: Some(tool_defs.clone()),
                messages: history.clone(),
                stream: true,
            };

            let mut parser = StreamParser::default();

            let stream_res = self
                .client
                .stream_messages(&self.auth, &req, &mut |raw| {
                    on_event(&raw).map_err(|e| ServicesError::Callback {
                        detail: e.to_string(),
                    })?;

                    parser
                        .process_event(&raw)
                        .map_err(|e| ServicesError::Callback {
                            detail: e.to_string(),
                        })?;
                    Ok(())
                })
                .await;

            if let Err(err) = stream_res {
                if is_prompt_too_long_error(&err) && !did_reactive_compact {
                    did_reactive_compact = true;
                    maybe_reactive_compact(
                        self,
                        &mut history,
                        &system,
                        tool_defs_json_len,
                        self.max_tokens,
                    )
                    .await;
                    continue;
                }
                return Err(err.into());
            }

            let parsed = parser.finish();
            resolved_model = parsed.model.or(resolved_model);

            if let Some(usage) = parsed.message.usage.clone() {
                combined_usage.input_tokens = combined_usage
                    .input_tokens
                    .saturating_add(usage.input_tokens);
                combined_usage.output_tokens = combined_usage
                    .output_tokens
                    .saturating_add(usage.output_tokens);
                combined_usage.cache_creation_input_tokens = combined_usage
                    .cache_creation_input_tokens
                    .saturating_add(usage.cache_creation_input_tokens);
                combined_usage.cache_read_input_tokens = combined_usage
                    .cache_read_input_tokens
                    .saturating_add(usage.cache_read_input_tokens);

                if let Some(cost_acc) = &mut combined_cost {
                    // Try response model first, then requested model.
                    let model_for_cost = resolved_model.as_deref().unwrap_or(&self.model);
                    if let Some(turn_cost) = calculate_usd_cost(model_for_cost, &usage) {
                        *cost_acc += turn_cost;
                    } else {
                        combined_cost = None;
                    }
                }
            } else {
                combined_cost = None;
            }

            if let (Some(limit), Some(cost)) = (self.cfg.max_budget_usd, combined_cost) {
                if cost > limit {
                    anyhow::bail!("max budget exceeded: spent ${:.4} > ${:.4}", cost, limit);
                }
            }

            combined_text.push_str(&parsed.text);

            // Push assistant message (without response-only fields) into the request history.
            let assistant_msg = Message::Assistant(AssistantMessage {
                content: parsed.message.content.clone(),
                model: None,
                stop_reason: None,
                usage: None,
            });
            history.push(assistant_msg.clone());
            new_messages.push(assistant_msg);

            let stop_reason = parsed.message.stop_reason.or(last_stop_reason);
            last_stop_reason = stop_reason;

            if stop_reason == Some(StopReason::MaxTokens) && turns < max_turns {
                let continue_msg = Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "Continue.".to_string(),
                    }],
                });
                history.push(continue_msg.clone());
                new_messages.push(continue_msg);
                continue;
            }

            if stop_reason == Some(StopReason::ToolUse) {
                if turns >= max_turns {
                    anyhow::bail!("max turns reached while tools were requested");
                }

                let tool_calls = extract_tool_calls(&parsed.message.content);
                if tool_calls.is_empty() {
                    anyhow::bail!("stop_reason=tool_use but no tool_use blocks found");
                }

                // When an observer is present we may need interactive permission prompts,
                // so keep tool execution sequential for deterministic UI and safety.
                let can_parallelize = observer.is_none()
                    && tool_calls.len() > 1
                    && tool_calls.iter().all(|call| {
                        tools.get(&call.name).is_some_and(|t| {
                            t.is_concurrency_safe(&call.input) && t.is_read_only(&call.input)
                        })
                    });

                let observer_ref = observer.as_deref();

                let tool_results: Vec<ContentBlock> = if can_parallelize {
                    let ids: Vec<String> = tool_calls.iter().map(|c| c.id.clone()).collect();
                    let mut handles = Vec::with_capacity(tool_calls.len());

                    for call in tool_calls {
                        let tools = tools.clone();
                        let mut ctx = tool_ctx.clone();
                        let always_allow_seed = always_allow_seed.clone();
                        handles.push(tokio::spawn(async move {
                            let mut always_allow_tools: HashSet<String> = always_allow_seed;
                            execute_tool_call(&tools, &mut ctx, call, None, &mut always_allow_tools)
                                .await
                        }));
                    }

                    let mut out: Vec<ContentBlock> = Vec::with_capacity(handles.len());
                    for (idx, h) in handles.into_iter().enumerate() {
                        match h.await {
                            Ok(block) => out.push(block),
                            Err(err) => out.push(ContentBlock::ToolResult {
                                tool_use_id: ids
                                    .get(idx)
                                    .cloned()
                                    .unwrap_or_else(|| "unknown".to_string()),
                                content: serde_json::Value::String(format!(
                                    "tool task failed: {err}"
                                )),
                                is_error: true,
                            }),
                        }
                    }
                    out
                } else {
                    let mut tool_results: Vec<ContentBlock> = Vec::new();
                    for call in tool_calls {
                        let block = execute_tool_call(
                            &tools,
                            &mut tool_ctx,
                            call,
                            observer_ref,
                            &mut always_allow_tools,
                        )
                        .await;
                        tool_results.push(block);
                    }
                    tool_results
                };

                let tool_results_msg = Message::User(UserMessage {
                    content: tool_results,
                });
                history.push(tool_results_msg.clone());
                new_messages.push(tool_results_msg);
                continue;
            }

            break;
        }

        let model = resolved_model.unwrap_or_else(|| self.model.clone());

        Ok(RunResult {
            text: combined_text,
            usage: combined_usage,
            cost_usd: combined_cost,
            turns,
            model,
            stop_reason: last_stop_reason,
            history,
            new_messages,
        })
    }
}

#[derive(Debug, Clone)]
struct ToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
}

fn extract_tool_calls(blocks: &[ContentBlock]) -> Vec<ToolCall> {
    let mut out = Vec::new();
    for b in blocks {
        if let ContentBlock::ToolUse { id, name, input } = b {
            out.push(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }
    }
    out
}

async fn execute_tool_call(
    tools: &ToolRegistry,
    ctx: &mut ToolUseContext,
    call: ToolCall,
    observer: Option<&dyn QueryObserver>,
    always_allow_tools: &mut HashSet<String>,
) -> ContentBlock {
    let ToolCall { id, name, input } = call;
    let input_snapshot = input.clone();

    let Some(tool) = tools.get(&name) else {
        let content = serde_json::Value::String(format!("unknown tool: {name}"));
        if let Some(obs) = observer {
            obs.on_tool_use_result(&id, &name, &input, &content, true)
                .await;
        }
        return ContentBlock::ToolResult {
            tool_use_id: id,
            content,
            is_error: true,
        };
    };

    if let Err(err) = tool.validate_input(&input, ctx).await {
        let content = serde_json::Value::String(format!("invalid tool input: {err}"));
        if let Some(obs) = observer {
            obs.on_tool_use_result(&id, &name, &input, &content, true)
                .await;
        }
        return ContentBlock::ToolResult {
            tool_use_id: id,
            content,
            is_error: true,
        };
    }

    // If the tool was previously always-allowed (within this run or via config),
    // temporarily lift the permission-mode gate.
    let mut permission_override: Option<PermissionMode> = None;
    let name_key = name.trim().to_ascii_lowercase();
    let bypass_prompt =
        ctx.permission_mode == PermissionMode::Default && always_allow_tools.contains(&name_key);
    if bypass_prompt {
        permission_override = Some(PermissionMode::AcceptEdits);
    }

    // First check permissions in the configured mode.
    let perm = tool.check_permissions(&input, ctx).await;

    // If the tool is blocked only due to the Default-mode permission gate,
    // ask the observer for interactive approval.
    if let PermissionResult::Deny { reason } = perm {
        let wants_prompt = !bypass_prompt
            && ctx.permission_mode == PermissionMode::Default
            && observer.is_some()
            && is_permission_mode_denial(&reason);

        if wants_prompt {
            let decision = observer
                .expect("checked observer.is_some()")
                .request_permission(&id, &name, &input_snapshot)
                .await;

            match decision {
                PermissionDecision::Deny => {
                    let content = serde_json::Value::String(format!("permission denied: {reason}"));
                    if let Some(obs) = observer {
                        obs.on_tool_use_result(&id, &name, &input_snapshot, &content, true)
                            .await;
                    }
                    return ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error: true,
                    };
                }
                PermissionDecision::AllowOnce => {
                    permission_override = Some(PermissionMode::AcceptEdits);
                }
                PermissionDecision::AlwaysAllowTool => {
                    always_allow_tools.insert(name_key);
                    permission_override = Some(PermissionMode::AcceptEdits);
                }
            }
        } else if permission_override.is_none() {
            let content = serde_json::Value::String(format!("permission denied: {reason}"));
            if let Some(obs) = observer {
                obs.on_tool_use_result(&id, &name, &input_snapshot, &content, true)
                    .await;
            }
            return ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error: true,
            };
        }
    }

    if let Some(mode) = permission_override {
        let old = ctx.permission_mode;
        ctx.permission_mode = mode;
        let checked = tool.check_permissions(&input, ctx).await;
        ctx.permission_mode = old;
        match checked {
            PermissionResult::Allow => {}
            PermissionResult::Deny { reason } => {
                let content = serde_json::Value::String(format!("permission denied: {reason}"));
                if let Some(obs) = observer {
                    obs.on_tool_use_result(&id, &name, &input_snapshot, &content, true)
                        .await;
                }
                return ContentBlock::ToolResult {
                    tool_use_id: id,
                    content,
                    is_error: true,
                };
            }
        }
    }

    if let Some(obs) = observer {
        obs.on_tool_use_start(&id, &name, &input_snapshot).await;
    }

    match tool.call(input, ctx).await {
        Ok(mut result) => {
            persist_large_tool_result(tool.as_ref(), &mut result, ctx);
            if let Some(obs) = observer {
                obs.on_tool_use_result(
                    &id,
                    &name,
                    &input_snapshot,
                    &result.content,
                    result.is_error,
                )
                .await;
            }

            ContentBlock::ToolResult {
                tool_use_id: id,
                content: result.content,
                is_error: result.is_error,
            }
        }
        Err(err) => {
            let content = serde_json::Value::String(format!("tool failed: {err}"));
            if let Some(obs) = observer {
                obs.on_tool_use_result(&id, &name, &input_snapshot, &content, true)
                    .await;
            }
            ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error: true,
            }
        }
    }
}

fn is_permission_mode_denial(reason: &str) -> bool {
    // Built-in tools use this phrasing for the "needs interactive approval" path in Default mode.
    // We only prompt the observer in this case, not for e.g. invalid inputs or path restrictions.
    reason.contains("disabled in this permission mode")
        || reason.contains("Re-run with --permission-mode")
}

fn persist_large_tool_result(
    tool: &dyn claude_tools::Tool,
    result: &mut claude_tools::ToolResult,
    ctx: &ToolUseContext,
) {
    let max = tool.max_result_size_chars();

    match &result.content {
        serde_json::Value::String(s) => {
            let len = s.chars().count();
            if len <= max {
                return;
            }

            let path = ctx.result_store.store_text(tool.name(), s);
            match path {
                Ok(path) => {
                    let preview = truncate_chars(s, max);
                    let msg = format!(
                        "Tool output was {len} chars and was saved to {}\nUse the Read tool on that path to view the full output.\n\nPreview (truncated to {max} chars):\n{preview}\n\n(full output in file)",
                        path.display()
                    );
                    result.content = serde_json::Value::String(msg);
                }
                Err(_err) => {
                    let preview = truncate_chars(s, max);
                    result.content = serde_json::Value::String(format!(
                        "{preview}\n\n(output truncated; failed to persist full output)"
                    ));
                }
            }
        }
        other => {
            // For non-string results, only persist if the JSON rendering is large.
            let rendered = match serde_json::to_string_pretty(other) {
                Ok(s) => s,
                Err(_) => return,
            };
            let len = rendered.chars().count();
            if len <= max {
                return;
            }

            let path = ctx.result_store.store_json(tool.name(), other);
            match path {
                Ok(path) => {
                    let preview = truncate_chars(&rendered, max);
                    let msg = format!(
                        "Tool output was {len} chars and was saved to {}\nUse the Read tool on that path to view the full output.\n\nPreview (truncated to {max} chars):\n{preview}\n\n(full output in file)",
                        path.display()
                    );
                    result.content = serde_json::Value::String(msg);
                }
                Err(_err) => {
                    let preview = truncate_chars(&rendered, max);
                    result.content = serde_json::Value::String(format!(
                        "{preview}\n\n(output truncated; failed to persist full output)"
                    ));
                }
            }
        }
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = String::new();
    out.extend(s.chars().take(max_chars));
    out
}

fn build_allowed_roots(cwd: &PathBuf, add_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    out.push(cwd.clone());

    for d in add_dirs {
        let abs = if d.is_absolute() {
            d.clone()
        } else {
            cwd.join(d)
        };
        out.push(abs);
    }

    out
}

fn create_result_store(cwd: &PathBuf) -> (Arc<ToolResultStore>, PathBuf) {
    let preferred = cwd.join(".claude-rs").join("tool-results");
    match ToolResultStore::new(preferred.clone()) {
        Ok(store) => (Arc::new(store), preferred),
        Err(_) => {
            let fallback = std::env::temp_dir().join("claude-rs").join("tool-results");
            match ToolResultStore::new(fallback.clone()) {
                Ok(store) => (Arc::new(store), fallback),
                Err(_) => {
                    // Last resort: store into cwd directly.
                    let store = ToolResultStore::new(cwd.clone())
                        .expect("cwd should be a valid directory for result storage");
                    (Arc::new(store), cwd.clone())
                }
            }
        }
    }
}

const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const ONE_M_CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;

const AUTOCOMPACT_BUFFER_TOKENS: u64 = 13_000;
const REACTIVE_COMPACT_BUFFER_TOKENS: u64 = 20_000;

const COMPACT_KEEP_TAIL_MESSAGES: usize = 20;
const COMPACT_SUMMARY_MAX_TOKENS: u32 = 2048;
const COMPACT_MAX_TRANSCRIPT_CHARS: usize = 180_000;

async fn maybe_proactive_compact(
    engine: &QueryEngine,
    history: &mut Vec<Message>,
    system: &str,
    tool_defs_json_len: usize,
    max_tokens: u32,
) {
    if is_env_truthy("DISABLE_COMPACT") || is_env_truthy("DISABLE_AUTO_COMPACT") {
        return;
    }

    let context_window = context_window_for_model(&engine.model);
    let threshold = context_window
        .saturating_sub(AUTOCOMPACT_BUFFER_TOKENS)
        .saturating_sub(max_tokens as u64);

    let est = estimate_request_tokens(system, tool_defs_json_len, history);
    if est <= threshold {
        return;
    }

    match compact_history(
        engine,
        history,
        COMPACT_KEEP_TAIL_MESSAGES,
        COMPACT_SUMMARY_MAX_TOKENS,
    )
    .await
    {
        Ok(new_hist) => *history = new_hist,
        Err(_err) => hard_truncate_history(history, COMPACT_KEEP_TAIL_MESSAGES),
    }

    // If we're still estimated over budget, truncate more aggressively.
    let est2 = estimate_request_tokens(system, tool_defs_json_len, history);
    if est2 > threshold {
        hard_truncate_history(history, 8);
    }
}

async fn maybe_reactive_compact(
    engine: &QueryEngine,
    history: &mut Vec<Message>,
    system: &str,
    tool_defs_json_len: usize,
    max_tokens: u32,
) {
    if is_env_truthy("DISABLE_COMPACT") {
        return;
    }

    let context_window = context_window_for_model(&engine.model);
    let threshold = context_window
        .saturating_sub(REACTIVE_COMPACT_BUFFER_TOKENS)
        .saturating_sub(max_tokens as u64);

    // First try a real summary-based compaction.
    match compact_history(
        engine,
        history,
        COMPACT_KEEP_TAIL_MESSAGES,
        COMPACT_SUMMARY_MAX_TOKENS,
    )
    .await
    {
        Ok(new_hist) => *history = new_hist,
        Err(_err) => hard_truncate_history(history, COMPACT_KEEP_TAIL_MESSAGES),
    }

    // If still too large, keep trimming until we're under the threshold or out of history.
    let mut est = estimate_request_tokens(system, tool_defs_json_len, history);
    let mut keep = 12usize;
    while est > threshold && history.len() > keep && keep > 1 {
        hard_truncate_history(history, keep);
        keep = keep.saturating_sub(2);
        est = estimate_request_tokens(system, tool_defs_json_len, history);
    }
}

fn hard_truncate_history(history: &mut Vec<Message>, keep_tail: usize) {
    if history.len() <= keep_tail {
        return;
    }

    let tail_start = history.len().saturating_sub(keep_tail);
    let tail = history.split_off(tail_start);
    history.clear();

    history.push(Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "[Conversation history truncated due to length.]".to_string(),
        }],
    }));

    history.extend(tail);
}

async fn compact_history(
    engine: &QueryEngine,
    history: &[Message],
    keep_tail: usize,
    summary_max_tokens: u32,
) -> anyhow::Result<Vec<Message>> {
    if history.len() <= keep_tail + 2 {
        return Ok(history.to_vec());
    }

    let split = history.len().saturating_sub(keep_tail.max(1));
    let (head, tail) = history.split_at(split);

    let mut transcript = render_messages_for_summary(head);
    if transcript.chars().count() > COMPACT_MAX_TRANSCRIPT_CHARS {
        transcript = truncate_tail_chars(&transcript, COMPACT_MAX_TRANSCRIPT_CHARS);
    }

    let mut prompt = String::new();
    prompt.push_str("Summarize the conversation so far for future context.\n");
    prompt.push_str(
        "- Focus on key decisions, constraints, file changes, commands, errors, and TODOs.\n",
    );
    prompt.push_str("- Be concise but include details needed to continue.\n");
    prompt.push_str("- Output plain text (no Markdown code fences).\n\n");
    prompt.push_str("# Conversation\n\n");
    prompt.push_str(&transcript);

    let req = MessagesRequest {
        model: engine.model.clone(),
        max_tokens: summary_max_tokens.max(256),
        system: Some("You are a summarizer. Return only the summary text.".to_string()),
        tools: None,
        messages: vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: prompt }],
        })],
        stream: true,
    };

    let mut parser = StreamParser::default();

    // If the summary request itself hits prompt-too-long, progressively truncate input and retry.
    let mut attempt: u32 = 0;
    let mut req = req;
    loop {
        attempt += 1;
        let res = engine
            .client
            .stream_messages(&engine.auth, &req, &mut |raw| {
                parser
                    .process_event(&raw)
                    .map_err(|e| ServicesError::Callback {
                        detail: e.to_string(),
                    })?;
                Ok(())
            })
            .await;

        match res {
            Ok(()) => break,
            Err(err) if is_prompt_too_long_error(&err) && attempt < 4 => {
                // Chop transcript further and retry.
                let truncated = truncate_tail_chars(
                    &transcript,
                    (COMPACT_MAX_TRANSCRIPT_CHARS / (attempt as usize + 1)).max(20_000),
                );

                let mut prompt = String::new();
                prompt.push_str("Summarize the conversation so far for future context.\n");
                prompt.push_str("- Focus on key decisions, constraints, file changes, commands, errors, and TODOs.\n");
                prompt.push_str("- Be concise but include details needed to continue.\n");
                prompt.push_str("- Output plain text (no Markdown code fences).\n\n");
                prompt.push_str("# Conversation (truncated)\n\n");
                prompt.push_str(&truncated);

                req.messages = vec![Message::User(UserMessage {
                    content: vec![ContentBlock::Text { text: prompt }],
                })];

                parser = StreamParser::default();
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }

    let parsed = parser.finish();
    let summary = parsed.text.trim();
    if summary.is_empty() {
        anyhow::bail!("compaction summary was empty");
    }

    let summary_msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: format!("[Conversation summary]\n{summary}"),
        }],
    });

    let mut new_history: Vec<Message> = Vec::new();
    new_history.push(summary_msg);
    new_history.extend_from_slice(tail);
    Ok(new_history)
}

fn is_prompt_too_long_error(err: &ServicesError) -> bool {
    match err {
        ServicesError::ApiStatus { status, body } => {
            // 413 is commonly used for payload too large; 400 is also seen.
            if *status != 400 && *status != 413 {
                return false;
            }
            let b = body.to_ascii_lowercase();
            b.contains("prompt_too_long")
                || b.contains("prompt too long")
                || b.contains("context_length_exceeded")
                || b.contains("too many tokens")
        }
        _ => false,
    }
}

fn is_env_truthy(key: &str) -> bool {
    let Ok(v) = std::env::var(key) else {
        return false;
    };
    let v = v.trim().to_ascii_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

fn context_window_for_model(model: &str) -> u64 {
    if model.to_ascii_lowercase().contains("[1m]") {
        return ONE_M_CONTEXT_WINDOW_TOKENS;
    }
    DEFAULT_CONTEXT_WINDOW_TOKENS
}

fn estimate_request_tokens(system: &str, tool_defs_json_len: usize, messages: &[Message]) -> u64 {
    let mut chars: u64 = 0;
    chars = chars.saturating_add(system.chars().count() as u64);
    chars = chars.saturating_add(tool_defs_json_len as u64);

    for m in messages {
        chars = chars.saturating_add(estimate_message_chars(m));
    }

    // Heuristic: ~4 chars/token for English-ish text.
    chars / 4
}

fn estimate_message_chars(m: &Message) -> u64 {
    let blocks = match m {
        Message::User(u) => &u.content,
        Message::Assistant(a) => &a.content,
    };

    let mut chars: u64 = 20; // message wrapper overhead
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                chars = chars.saturating_add(text.chars().count() as u64);
            }
            ContentBlock::Thinking { thinking } => {
                chars = chars.saturating_add(thinking.chars().count() as u64);
            }
            ContentBlock::ToolUse { name, input, .. } => {
                chars = chars.saturating_add(name.len() as u64);
                let input_len = serde_json::to_string(input).map(|s| s.len()).unwrap_or(0);
                chars = chars.saturating_add(input_len as u64);
                chars = chars.saturating_add(50);
            }
            ContentBlock::ToolResult { content, .. } => {
                let content_len = serde_json::to_string(content).map(|s| s.len()).unwrap_or(0);
                chars = chars.saturating_add(content_len as u64);
                chars = chars.saturating_add(50);
            }
        }
    }
    chars
}

fn render_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            Message::User(u) => {
                out.push_str("[user]\n");
                render_blocks_for_summary(&mut out, &u.content);
            }
            Message::Assistant(a) => {
                out.push_str("[assistant]\n");
                render_blocks_for_summary(&mut out, &a.content);
            }
        }
        out.push('\n');
    }
    out
}

fn render_blocks_for_summary(out: &mut String, blocks: &[ContentBlock]) {
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                out.push_str(text.trim());
                out.push('\n');
            }
            ContentBlock::Thinking { .. } => {
                // Omit thinking blocks to reduce noise.
            }
            ContentBlock::ToolUse { name, .. } => {
                out.push_str(&format!("[tool_use name={name}]\n"));
            }
            ContentBlock::ToolResult { is_error, .. } => {
                out.push_str(&format!("[tool_result is_error={is_error}]\n"));
            }
        }
    }
}

fn truncate_tail_chars(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }

    let skip = len.saturating_sub(max_chars);
    s.chars().skip(skip).collect()
}

#[derive(Clone)]
struct QueryAgentExecutor {
    client: AnthropicClient,
    auth: AuthMode,
    model: String,
    max_tokens: u32,
    cfg: QueryEngineConfig,
}

impl QueryAgentExecutor {
    fn new(
        client: AnthropicClient,
        auth: AuthMode,
        model: String,
        max_tokens: u32,
        cfg: QueryEngineConfig,
    ) -> Self {
        Self {
            client,
            auth,
            model,
            max_tokens,
            cfg,
        }
    }
}

#[async_trait]
impl AgentExecutor for QueryAgentExecutor {
    async fn run_agent(
        &self,
        description: Option<String>,
        prompt: String,
        depth: u32,
    ) -> anyhow::Result<String> {
        let mut cfg = self.cfg.clone();

        // Limit sub-agent autonomy; it should return a report, not spin indefinitely.
        cfg.max_turns = cfg.max_turns.min(4).max(1);
        cfg.agent_depth = depth;

        // Annotate the sub-agent prompt to preserve user intent.
        let prompt = match description {
            Some(d) if !d.trim().is_empty() => format!("[Task] {d}\n\n{prompt}"),
            _ => prompt,
        };

        let engine = QueryEngine::new(
            self.client.clone(),
            self.auth.clone(),
            self.model.clone(),
            self.max_tokens,
            cfg,
        )?;

        let result = engine.run(&prompt, |_event| Ok(())).await?;
        Ok(result.text)
    }
}
