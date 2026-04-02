use async_trait::async_trait;

use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "WebFetch";
const MAX_FETCH_BYTES: usize = 2_000_000;

#[derive(Debug, Default, Clone)]
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        // Matches the TS CLI schema (url + prompt), but this Rust rewrite returns
        // the fetched content verbatim (best-effort) rather than applying the prompt.
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "url": { "type": "string", "description": "The URL to fetch content from" },
            "prompt": { "type": "string", "description": "Prompt to apply to the fetched content (ignored in Rust rewrite; content is returned)" }
          },
          "required": ["url", "prompt"]
        })
    }

    fn prompt(&self) -> String {
        "Fetch content from a URL. In the Rust rewrite, this returns the fetched page text (best-effort).".to_string()
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
                "WebFetch is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            )
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if url.is_empty() {
            return Ok(ToolResult::err_text("missing required field: url"));
        }

        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();

        let http = reqwest::Client::new();
        let resp = http
            .get(&url)
            .header(reqwest::header::USER_AGENT, "claude-rs/0.1 (web_fetch)")
            .send()
            .await?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let headers = resp.headers().clone();

        if let Some(len) = headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        {
            if len > MAX_FETCH_BYTES {
                return Ok(ToolResult::err_text(format!(
                    "response too large ({len} bytes > {MAX_FETCH_BYTES} limit)"
                )));
            }
        }

        let bytes = resp.bytes().await?;
        let bytes_len = bytes.len();
        let bytes = if bytes_len > MAX_FETCH_BYTES {
            bytes.slice(..MAX_FETCH_BYTES)
        } else {
            bytes
        };

        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let mut body_text = if content_type.contains("text/html") {
            html2text::from_read(bytes.as_ref(), 100)
        } else if content_type.contains("application/json") {
            match serde_json::from_slice::<serde_json::Value>(bytes.as_ref()) {
                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| String::new()),
                Err(_) => String::from_utf8_lossy(bytes.as_ref()).to_string(),
            }
        } else {
            String::from_utf8_lossy(bytes.as_ref()).to_string()
        };

        body_text = body_text.trim().to_string();

        let mut out = String::new();
        out.push_str(&format!(
            "Fetched {bytes_len} bytes from {final_url} (HTTP {})\n",
            status.as_u16()
        ));
        if !prompt.is_empty() {
            out.push_str("\nPrompt (not applied in Rust rewrite):\n");
            out.push_str(&prompt);
            out.push('\n');
        }
        out.push_str("\nContent:\n");
        out.push_str(&body_text);

        Ok(ToolResult {
            content: serde_json::Value::String(out),
            is_error: !status.is_success(),
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // WebFetch results can be large; prefer result persistence over truncation.
        100_000
    }
}
