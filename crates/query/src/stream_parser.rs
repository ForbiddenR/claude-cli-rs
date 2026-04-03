use claude_core::types::message::{AssistantMessage, ContentBlock, StopReason, TokenUsage};

#[derive(Debug, Default)]
pub struct StreamParser {
    model: Option<String>,
    stop_reason: Option<StopReason>,
    usage: TokenUsage,
    blocks: Vec<PartialBlock>,
}

#[derive(Debug)]
enum PartialBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: Option<serde_json::Value>,
        input_json: String,
    },
    Unknown(serde_json::Value),
}

impl StreamParser {
    pub fn process_event(&mut self, event: &serde_json::Value) -> anyhow::Result<()> {
        let Some(ty) = event.get("type").and_then(|v| v.as_str()) else {
            return Ok(());
        };

        match ty {
            "ping" => Ok(()),
            "message_start" => {
                if let Some(message) = event.get("message") {
                    if let Some(model) = message.get("model").and_then(|v| v.as_str()) {
                        self.model = Some(model.to_string());
                    }
                    if let Some(usage) = message.get("usage") {
                        self.apply_usage(usage);
                    }
                }
                Ok(())
            }
            "content_block_start" => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let Some(cb) = event.get("content_block") else {
                    return Ok(());
                };
                let cb_ty = cb.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
                let block = match cb_ty {
                    "text" => {
                        let text = cb.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        PartialBlock::Text(text.to_string())
                    }
                    "thinking" => {
                        let thinking = cb.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        PartialBlock::Thinking(thinking.to_string())
                    }
                    "tool_use" => {
                        let id = cb
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = cb
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = cb.get("input").cloned();
                        PartialBlock::ToolUse {
                            id,
                            name,
                            input,
                            input_json: String::new(),
                        }
                    }
                    _ => PartialBlock::Unknown(cb.clone()),
                };

                self.set_block(index, block);
                Ok(())
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let Some(delta) = event.get("delta") else {
                    return Ok(());
                };
                let delta_ty = delta
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                match (delta_ty, self.blocks.get_mut(index)) {
                    ("text_delta", Some(PartialBlock::Text(text))) => {
                        if let Some(d) = delta.get("text").and_then(|v| v.as_str()) {
                            text.push_str(d);
                        }
                        Ok(())
                    }
                    ("thinking_delta", Some(PartialBlock::Thinking(thinking))) => {
                        if let Some(d) = delta.get("thinking").and_then(|v| v.as_str()) {
                            thinking.push_str(d);
                        }
                        Ok(())
                    }
                    ("input_json_delta", Some(PartialBlock::ToolUse { input_json, .. })) => {
                        if let Some(d) = delta.get("partial_json").and_then(|v| v.as_str()) {
                            input_json.push_str(d);
                        }
                        Ok(())
                    }
                    // Unknown delta types are ignored for now.
                    _ => Ok(()),
                }
            }
            "content_block_stop" => Ok(()),
            "message_delta" => {
                if let Some(delta) = event.get("delta") {
                    if let Some(stop) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                        self.stop_reason = Some(parse_stop_reason(stop));
                    }
                }
                if let Some(usage) = event.get("usage") {
                    self.apply_usage(usage);
                }
                Ok(())
            }
            "message_stop" => Ok(()),
            "error" => {
                let detail = event
                    .get("error")
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                anyhow::bail!("anthropic streaming error: {detail}");
            }
            _ => Ok(()),
        }
    }

    pub fn finish(self) -> ParsedAssistantMessage {
        let mut content: Vec<ContentBlock> = Vec::new();

        for block in self.blocks {
            match block {
                PartialBlock::Text(text) => content.push(ContentBlock::Text { text }),
                PartialBlock::Thinking(thinking) => {
                    content.push(ContentBlock::Thinking { thinking })
                }
                PartialBlock::ToolUse {
                    id,
                    name,
                    input,
                    input_json,
                } => {
                    let input = if !input_json.trim().is_empty() {
                        serde_json::from_str(&input_json).unwrap_or_else(|_| {
                            // Fall back to the best-effort input value if streaming JSON is invalid.
                            input.unwrap_or(serde_json::Value::String(input_json))
                        })
                    } else {
                        input.unwrap_or(serde_json::Value::Object(Default::default()))
                    };
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
                PartialBlock::Unknown(raw) => {
                    // Best-effort: preserve information as text.
                    let text = serde_json::to_string(&raw).unwrap_or_else(|_| raw.to_string());
                    content.push(ContentBlock::Text {
                        text: format!("[unsupported content block] {text}"),
                    });
                }
            }
        }

        let text = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();

        let message = AssistantMessage {
            content,
            model: self.model.clone(),
            stop_reason: self.stop_reason,
            usage: Some(self.usage),
        };

        ParsedAssistantMessage {
            message,
            text,
            model: self.model,
        }
    }

    fn set_block(&mut self, index: usize, block: PartialBlock) {
        if self.blocks.len() <= index {
            self.blocks.resize_with(
                index.saturating_add(1),
                || PartialBlock::Text(String::new()),
            );
        }
        self.blocks[index] = block;
    }

    fn apply_usage(&mut self, usage: &serde_json::Value) {
        if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            self.usage.input_tokens = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
            self.usage.output_tokens = v;
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.usage.cache_creation_input_tokens = v;
        }
        if let Some(v) = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.usage.cache_read_input_tokens = v;
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedAssistantMessage {
    pub message: AssistantMessage,
    pub text: String,
    pub model: Option<String>,
}

fn parse_stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_deltas_concatenate() {
        let mut p = StreamParser::default();

        p.process_event(&serde_json::json!({
            "type": "message_start",
            "message": { "model": "m", "usage": { "input_tokens": 1, "output_tokens": 0 } }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hello" }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": " world" }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "input_tokens": 1, "output_tokens": 2 }
        }))
        .unwrap();

        let parsed = p.finish();
        assert_eq!(parsed.text, "hello world");
        assert_eq!(parsed.message.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn parses_tool_use_with_input_json_deltas() {
        let mut p = StreamParser::default();

        p.process_event(&serde_json::json!({
            "type": "message_start",
            "message": { "model": "m", "usage": { "input_tokens": 1, "output_tokens": 0 } }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": "toolu_1", "name": "Write" }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{\"file_path\":\"a\"," }
        }))
        .unwrap();
        p.process_event(&serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "\"content\":\"b\"}" }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use" },
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }))
        .unwrap();

        let parsed = p.finish();
        assert_eq!(parsed.message.stop_reason, Some(StopReason::ToolUse));
        assert!(parsed.text.is_empty());

        match &parsed.message.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "Write");
                assert_eq!(input.get("file_path").and_then(|v| v.as_str()), Some("a"));
                assert_eq!(input.get("content").and_then(|v| v.as_str()), Some("b"));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }
    }

    #[test]
    fn invalid_input_json_falls_back_to_input_value() {
        let mut p = StreamParser::default();

        p.process_event(&serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "X",
                "input": { "x": 1 }
            }
        }))
        .unwrap();

        p.process_event(&serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{" }
        }))
        .unwrap();

        let parsed = p.finish();

        match &parsed.message.content[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert_eq!(input.get("x").and_then(|v| v.as_i64()), Some(1));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }
    }
}
