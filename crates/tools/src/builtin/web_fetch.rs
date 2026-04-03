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

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::types::permissions::PermissionMode;
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

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

    fn spawn_http_server_once(
        expected_path: &'static str,
        content_type: &'static str,
        body: String,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
        let addr = listener.local_addr().expect("server addr");

        let handle = thread::spawn(move || {
            let (mut stream, _peer) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut tmp).expect("read request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > 1_000_000 {
                    break;
                }
            }

            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or(buf.len());
            let header_str = String::from_utf8_lossy(&buf[..header_end]);

            let request_line = header_str.split("\r\n").next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();

            assert_eq!(method, "GET");
            assert_eq!(path, expected_path);

            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.as_bytes().len()
            );
            stream.write_all(resp.as_bytes()).expect("write response");
            stream.flush().ok();
        });

        (addr, handle)
    }

    #[tokio::test]
    async fn web_fetch_html_is_converted_to_text() {
        let (addr, handle) = spawn_http_server_once(
            "/html",
            "text/html; charset=utf-8",
            "<html><body>Hello <b>World</b></body></html>".to_string(),
        );

        let cwd = temp_dir("web-fetch-html");
        let mut ctx = ctx_for(cwd);
        let tool = WebFetchTool::default();

        let url = format!("http://{addr}/html");
        let input = serde_json::json!({ "url": url, "prompt": "extract text" });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);

        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("Hello"));
        assert!(out.contains("World"));

        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn web_fetch_json_is_pretty_printed() {
        let (addr, handle) =
            spawn_http_server_once("/json", "application/json", "{\"a\":1}".to_string());

        let cwd = temp_dir("web-fetch-json");
        let mut ctx = ctx_for(cwd);
        let tool = WebFetchTool::default();

        let url = format!("http://{addr}/json");
        let input = serde_json::json!({ "url": url, "prompt": "pretty print" });

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);

        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("\"a\": 1"));

        handle.join().expect("server thread");
    }
}
