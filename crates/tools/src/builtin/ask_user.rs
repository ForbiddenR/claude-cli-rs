use async_trait::async_trait;
use tokio::io::AsyncBufReadExt as _;

use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "AskUserQuestion";

#[derive(Debug, Default, Clone)]
pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "questions": {
              "type": "array",
              "minItems": 1,
              "maxItems": 4,
              "description": "Questions to ask the user (1-4)",
              "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                  "id": { "type": "string", "description": "Stable identifier for mapping answers" },
                  "question": { "type": "string", "description": "Question prompt shown to the user" },
                  "multiSelect": { "type": "boolean", "description": "Allow selecting multiple options" },
                  "options": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                      "type": "object",
                      "additionalProperties": false,
                      "properties": {
                        "label": { "type": "string" },
                        "description": { "type": "string" }
                      },
                      "required": ["label", "description"]
                    }
                  }
                },
                "required": ["id", "question", "options"]
              }
            }
          },
          "required": ["questions"]
        })
    }

    fn prompt(&self) -> String {
        "Ask the user one or more multiple-choice questions and return their answers. This tool reads from stdin.".to_string()
    }

    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        // In the TS CLI this triggers an interactive prompt; for the headless rewrite
        // we allow it, and the tool itself will block waiting for stdin.
        PermissionResult::Allow
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(questions) = input.get("questions").and_then(|v| v.as_array()) else {
            return Ok(ToolResult::err_text("missing required field: questions"));
        };
        if questions.is_empty() {
            return Ok(ToolResult::err_text("questions must be non-empty"));
        }

        let stdin = tokio::io::stdin();
        let mut reader = tokio::io::BufReader::new(stdin);

        let mut answers: Vec<(String, String)> = Vec::new();

        for (idx, q) in questions.iter().enumerate() {
            let Some(qobj) = q.as_object() else {
                return Ok(ToolResult::err_text("questions items must be objects"));
            };
            let id = qobj
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let question = qobj
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let multi = qobj
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let Some(options) = qobj.get("options").and_then(|v| v.as_array()) else {
                return Ok(ToolResult::err_text("each question requires options"));
            };

            if id.is_empty() || question.is_empty() || options.is_empty() {
                return Ok(ToolResult::err_text(
                    "each question requires id, question, and non-empty options",
                ));
            }

            eprintln!();
            eprintln!("Question {} ({id}): {question}", idx + 1);
            for (i, opt) in options.iter().enumerate() {
                let label = opt
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let desc = opt
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                eprintln!("  {}) {} — {}", i + 1, label, desc);
            }
            eprintln!("  0) Other — type a custom answer");
            eprint!(
                "Enter choice{}: ",
                if multi { " (comma-separated)" } else { "" }
            );
            use std::io::Write as _;
            std::io::stderr().flush().ok();

            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(ToolResult::err_text(
                    "stdin ended; AskUserQuestion requires interactive input. Pass the main prompt as an argument (not via stdin) so stdin remains available.",
                ));
            }
            let line = line.trim();
            if line.is_empty() {
                return Ok(ToolResult::err_text("no answer provided"));
            }

            let answer = if line == "0" {
                eprint!("Other: ");
                std::io::stderr().flush().ok();
                let mut other = String::new();
                let n = reader.read_line(&mut other).await?;
                if n == 0 {
                    return Ok(ToolResult::err_text(
                        "stdin ended while reading 'Other' answer",
                    ));
                }
                other.trim().to_string()
            } else {
                // Resolve numeric choices to labels; otherwise accept free-form.
                let mut labels: Vec<String> = Vec::new();
                let parts = if multi {
                    line.split(',').map(|s| s.trim()).collect::<Vec<_>>()
                } else {
                    vec![line]
                };

                for p in parts {
                    if let Ok(n) = p.parse::<usize>() {
                        if n == 0 {
                            continue;
                        }
                        if let Some(opt) = options.get(n.saturating_sub(1)) {
                            if let Some(label) = opt.get("label").and_then(|v| v.as_str()) {
                                labels.push(label.to_string());
                                continue;
                            }
                        }
                    }
                    labels.push(p.to_string());
                }

                labels.join(", ")
            };

            answers.push((question, answer));
        }

        let answers_text = answers
            .into_iter()
            .map(|(q, a)| format!("\"{q}\"=\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ");

        Ok(ToolResult::ok_text(format!(
            "User has answered your questions: {answers_text}. You can now continue with the user's answers in mind."
        )))
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
            current_tool_use_id: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    #[tokio::test]
    async fn ask_user_requires_non_empty_questions() {
        let cwd = temp_dir("ask-user");
        let mut ctx = ctx_for(cwd);
        let tool = AskUserQuestionTool::default();

        let input = serde_json::json!({ "questions": [] });
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("questions must be non-empty"));
    }

    #[tokio::test]
    async fn ask_user_requires_questions_field() {
        let cwd = temp_dir("ask-user-missing");
        let mut ctx = ctx_for(cwd);
        let tool = AskUserQuestionTool::default();

        let input = serde_json::json!({});
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("questions"));
    }
}
