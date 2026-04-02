use serde::Serialize;

use claude_core::types::message::Message;

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,

    pub messages: Vec<Message>,

    #[serde(default)]
    pub stream: bool,
}
