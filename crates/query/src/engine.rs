use std::path::PathBuf;

use claude_core::types::message::{ContentBlock, Message, TokenUsage, UserMessage};

use claude_services::ServicesError;
use claude_services::api::{AnthropicClient, MessagesRequest};
use claude_services::auth::AuthMode;

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
    /// JSON Schema to enforce via instructions (Week 4 will implement tool-based enforcement).
    pub json_schema: Option<String>,

    pub max_turns: u32,
    pub max_budget_usd: Option<f64>,
}

pub struct QueryEngine {
    client: AnthropicClient,
    auth: AuthMode,
    model: String,
    max_tokens: u32,
    cfg: QueryEngineConfig,
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
    ) -> Self {
        Self {
            client,
            auth,
            model,
            max_tokens,
            cfg,
        }
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

        loop {
            turns += 1;

            let req = MessagesRequest {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                system: Some(system.clone()),
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
            history.push(Message::Assistant(
                claude_core::types::message::AssistantMessage {
                    content: parsed.message.content.clone(),
                    model: None,
                    stop_reason: None,
                    usage: None,
                },
            ));

            let stop_reason = parsed.message.stop_reason;
            if stop_reason == Some(claude_core::types::message::StopReason::MaxTokens)
                && turns < max_turns
            {
                history.push(Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "Continue.".to_string(),
                    }],
                }));
                continue;
            }

            if stop_reason == Some(claude_core::types::message::StopReason::ToolUse) {
                anyhow::bail!(
                    "model requested tool use, but tools are not implemented yet (Week 4)"
                );
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
