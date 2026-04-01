use serde::Serialize;

use claude_core::types::message::Message;

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
}

