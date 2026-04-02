use std::path::PathBuf;

use claude_core::types::message::{
    AssistantMessage, ContentBlock, Message, StopReason, TokenUsage, UserMessage,
};
use claude_core::types::permissions::PermissionMode;

use claude_services::ServicesError;
use claude_services::api::{AnthropicClient, MessagesRequest, ToolDefinition};
use claude_services::auth::AuthMode;

use claude_tools::registry::{ToolPoolOpts, assemble_tool_pool};
use claude_tools::{PermissionResult, ToolRegistry, ToolUseContext};

use crate::context::{ContextOpts, gather_context};
use crate::cost::calculate_usd_cost;
use crate::stream_parser::StreamParser;
use crate::system_prompt::{SystemPromptParts, build_system_prompt};

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
}

pub struct QueryEngine {
    client: AnthropicClient,
    auth: AuthMode,
    model: String,
    max_tokens: u32,
    cfg: QueryEngineConfig,
    tools: ToolRegistry,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub text: String,
    pub usage: TokenUsage,
    pub cost_usd: Option<f64>,
    pub turns: u32,
    pub model: String,
}

impl QueryEngine {
    pub fn new(
        client: AnthropicClient,
        auth: AuthMode,
        model: String,
        max_tokens: u32,
        cfg: QueryEngineConfig,
    ) -> anyhow::Result<Self> {
        let tools = assemble_tool_pool(ToolPoolOpts {
            base_tools: cfg.base_tools.clone(),
            allowed_tools: cfg.allowed_tools.clone(),
            disallowed_tools: cfg.disallowed_tools.clone(),
        })?;

        Ok(Self {
            client,
            auth,
            model,
            max_tokens,
            cfg,
            tools,
        })
    }

    /// Runs a one-shot headless session. The callback receives raw SSE event JSON.
    pub async fn run<F>(&self, prompt: &str, mut on_event: F) -> anyhow::Result<RunResult>
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

        let system = build_system_prompt(
            &ctx,
            SystemPromptParts {
                base: self.cfg.system_prompt.as_deref(),
                append: self.cfg.append_system_prompt.as_deref(),
                json_schema: self.cfg.json_schema.as_deref(),
                include_context: !self.cfg.bare,
            },
        );

        let tool_defs = self
            .tools
            .metadata()
            .into_iter()
            .map(|m| ToolDefinition {
                name: m.name,
                description: m.description,
                input_schema: m.input_schema,
            })
            .collect::<Vec<_>>();

        let mut history: Vec<Message> = vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        })];

        let max_turns = self.cfg.max_turns.max(1);
        let mut turns: u32 = 0;
        let mut combined_text = String::new();
        let mut combined_usage = TokenUsage::default();
        let mut combined_cost: Option<f64> = Some(0.0);
        let mut resolved_model: Option<String> = None;

        let mut tool_ctx = ToolUseContext {
            cwd: self.cfg.cwd.clone(),
            allowed_roots: build_allowed_roots(&self.cfg.cwd, &self.cfg.add_dirs),
            permission_mode: self.cfg.permission_mode,
        };

        loop {
            turns += 1;

            let req = MessagesRequest {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                system: Some(system.clone()),
                tools: Some(tool_defs.clone()),
                messages: history.clone(),
                stream: true,
            };

            let mut parser = StreamParser::default();

            self.client
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
                .await?;

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
            history.push(Message::Assistant(AssistantMessage {
                content: parsed.message.content.clone(),
                model: None,
                stop_reason: None,
                usage: None,
            }));

            let stop_reason = parsed.message.stop_reason;

            if stop_reason == Some(StopReason::MaxTokens) && turns < max_turns {
                history.push(Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "Continue.".to_string(),
                    }],
                }));
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

                let can_parallelize = tool_calls.len() > 1
                    && tool_calls.iter().all(|call| {
                        self.tools
                            .get(&call.name)
                            .is_some_and(|t| t.is_concurrency_safe(&call.input) && t.is_read_only(&call.input))
                    });

                let tool_results: Vec<ContentBlock> = if can_parallelize {
                    let ids: Vec<String> = tool_calls.iter().map(|c| c.id.clone()).collect();
                    let mut handles = Vec::with_capacity(tool_calls.len());

                    for call in tool_calls {
                        let tools = self.tools.clone();
                        let mut ctx = tool_ctx.clone();
                        handles.push(tokio::spawn(async move {
                            execute_tool_call(&tools, &mut ctx, call).await
                        }));
                    }

                    let mut out: Vec<ContentBlock> = Vec::with_capacity(handles.len());
                    for (idx, h) in handles.into_iter().enumerate() {
                        match h.await {
                            Ok(block) => out.push(block),
                            Err(err) => out.push(ContentBlock::ToolResult {
                                tool_use_id: ids.get(idx).cloned().unwrap_or_else(|| "unknown".to_string()),
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
                        let block = execute_tool_call(&self.tools, &mut tool_ctx, call).await;
                        tool_results.push(block);
                    }
                    tool_results
                };

                history.push(Message::User(UserMessage {
                    content: tool_results,
                }));
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
) -> ContentBlock {
    let ToolCall { id, name, input } = call;

    let Some(tool) = tools.get(&name) else {
        return ContentBlock::ToolResult {
            tool_use_id: id,
            content: serde_json::Value::String(format!("unknown tool: {name}")),
            is_error: true,
        };
    };

    if let Err(err) = tool.validate_input(&input, ctx).await {
        return ContentBlock::ToolResult {
            tool_use_id: id,
            content: serde_json::Value::String(format!("invalid tool input: {err}")),
            is_error: true,
        };
    }

    match tool.check_permissions(&input, ctx).await {
        PermissionResult::Allow => {}
        PermissionResult::Deny { reason } => {
            return ContentBlock::ToolResult {
                tool_use_id: id,
                content: serde_json::Value::String(format!("permission denied: {reason}")),
                is_error: true,
            };
        }
    }

    match tool.call(input, ctx).await {
        Ok(mut result) => {
            // Enforce a max size in case a tool returns a huge inline string.
            if let serde_json::Value::String(s) = &result.content {
                if s.chars().count() > tool.max_result_size_chars() {
                    let mut truncated = String::new();
                    truncated.extend(s.chars().take(tool.max_result_size_chars()));
                    truncated.push_str("\n(output truncated)");
                    result.content = serde_json::Value::String(truncated);
                }
            }

            ContentBlock::ToolResult {
                tool_use_id: id,
                content: result.content,
                is_error: result.is_error,
            }
        }
        Err(err) => ContentBlock::ToolResult {
            tool_use_id: id,
            content: serde_json::Value::String(format!("tool failed: {err}")),
            is_error: true,
        },
    }
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
