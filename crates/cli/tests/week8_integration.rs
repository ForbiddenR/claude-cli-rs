use predicates::prelude::*;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::thread;
use std::time::Duration;

struct ExpectedRequest {
    must_contain: Option<String>,
    sse_body: String,
}

fn spawn_mock_sse_server_sequence(
    responses: Vec<ExpectedRequest>,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("server addr");

    let handle = thread::spawn(move || {
        for (idx, exp) in responses.into_iter().enumerate() {
            let (mut stream, _peer) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

            // Read until the end of headers.
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut tmp).expect("read request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > 2_000_000 {
                    break;
                }
            }

            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or(buf.len());

            let header_str = String::from_utf8_lossy(&buf[..header_end]);
            let mut lines = header_str.split("\r\n");
            let request_line = lines.next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();

            let mut content_length: usize = 0;
            for line in lines {
                if line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse::<usize>().unwrap_or(0);
                }
            }

            // Read request body so we can assert on it.
            let mut body: Vec<u8> = Vec::new();
            let already_body = buf.len().saturating_sub(header_end);
            if already_body > 0 {
                body.extend_from_slice(&buf[header_end..]);
            }
            let mut remaining = content_length.saturating_sub(already_body);
            while remaining > 0 {
                let n = stream.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                let take = remaining.min(n);
                body.extend_from_slice(&tmp[..take]);
                remaining = remaining.saturating_sub(take);
            }

            if let Some(needle) = exp.must_contain.as_deref() {
                let body_str = String::from_utf8_lossy(&body);
                assert!(
                    body_str.contains(needle),
                    "request {idx} body did not contain {needle}\nbody={body_str}"
                );
            }

            let (status_line, body) = if method == "POST" && path == "/v1/messages" {
                ("HTTP/1.1 200 OK\r\n", exp.sse_body)
            } else {
                ("HTTP/1.1 404 Not Found\r\n", "not found".to_string())
            };

            let resp = format!(
                "{status_line}Content-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.as_bytes().len()
            );
            stream.write_all(resp.as_bytes()).expect("write response");
            stream.flush().ok();
        }
    });

    (addr, handle)
}

fn sse_events(events: Vec<serde_json::Value>) -> String {
    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(&ev.to_string());
        body.push('\n');
        body.push('\n');
    }
    body
}

fn mock_sse_ok_text(text: &str) -> String {
    let events = vec![
        serde_json::json!({
          "type": "message_start",
          "message": { "model": "claude-sonnet-4-6", "usage": { "input_tokens": 1, "output_tokens": 0 } }
        }),
        serde_json::json!({
          "type": "content_block_start",
          "index": 0,
          "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
          "type": "content_block_delta",
          "index": 0,
          "delta": { "type": "text_delta", "text": text }
        }),
        serde_json::json!({
          "type": "message_delta",
          "delta": { "stop_reason": "end_turn" },
          "usage": { "input_tokens": 1, "output_tokens": 1 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ];
    sse_events(events)
}

fn mock_sse_tool_use_write(file_path: &str, content: &str) -> String {
    let events = vec![
        serde_json::json!({
          "type": "message_start",
          "message": { "model": "claude-sonnet-4-6", "usage": { "input_tokens": 1, "output_tokens": 0 } }
        }),
        serde_json::json!({
          "type": "content_block_start",
          "index": 0,
          "content_block": {
            "type": "tool_use",
            "id": "toolu_1",
            "name": "Write",
            "input": {
              "file_path": file_path,
              "content": content
            }
          }
        }),
        serde_json::json!({
          "type": "message_delta",
          "delta": { "stop_reason": "tool_use" },
          "usage": { "input_tokens": 1, "output_tokens": 1 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ];
    sse_events(events)
}

#[test]
fn runs_tool_use_write_and_creates_file() {
    let cfg = tempfile::tempdir().expect("temp config dir");
    let work = tempfile::tempdir().expect("temp work dir");

    let file_path = work.path().join("hello.txt");
    let sse1 = mock_sse_tool_use_write(&file_path.to_string_lossy(), "hi from tool");
    let sse2 = mock_sse_ok_text("All done");

    let (addr, handle) = spawn_mock_sse_server_sequence(vec![
        ExpectedRequest {
            must_contain: None,
            sse_body: sse1,
        },
        ExpectedRequest {
            must_contain: Some("\"tool_result\"".to_string()),
            sse_body: sse2,
        },
    ]);
    let base_url = format!("http://{addr}");

    let mut cmd = assert_cmd::Command::cargo_bin("claude-rs").expect("cargo_bin");
    cmd.current_dir(work.path());
    cmd.env("CLAUDE_CONFIG_DIR", cfg.path());
    cmd.env("ANTHROPIC_BASE_URL", base_url);
    cmd.env("ANTHROPIC_API_KEY", "test-key");
    cmd.env("CLAUDE_RS_EXTRACT_MEMORIES", "0");
    cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    cmd.args(["--permission-mode", "bypassPermissions"]);
    cmd.arg("Write a file");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("All done"));

    let written = std::fs::read_to_string(&file_path).expect("read written file");
    assert_eq!(written, "hi from tool");

    handle.join().expect("server thread");
}

#[test]
fn continue_session_includes_previous_messages() {
    let cfg = tempfile::tempdir().expect("temp config dir");
    let work = tempfile::tempdir().expect("temp work dir");

    // First run: create a session.
    let (addr1, handle1) = spawn_mock_sse_server_sequence(vec![ExpectedRequest {
        must_contain: None,
        sse_body: mock_sse_ok_text("First response"),
    }]);
    let base_url1 = format!("http://{addr1}");

    let mut cmd1 = assert_cmd::Command::cargo_bin("claude-rs").expect("cargo_bin");
    cmd1.current_dir(work.path());
    cmd1.env("CLAUDE_CONFIG_DIR", cfg.path());
    cmd1.env("ANTHROPIC_BASE_URL", base_url1);
    cmd1.env("ANTHROPIC_API_KEY", "test-key");
    cmd1.env("CLAUDE_RS_EXTRACT_MEMORIES", "0");
    cmd1.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd1.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    cmd1.arg("first prompt");

    cmd1.assert()
        .success()
        .stdout(predicate::str::contains("First response"));
    handle1.join().expect("server thread");

    // Second run: continue and ensure the request contains the previous prompt.
    let (addr2, handle2) = spawn_mock_sse_server_sequence(vec![ExpectedRequest {
        must_contain: Some("first prompt".to_string()),
        sse_body: mock_sse_ok_text("Second response"),
    }]);
    let base_url2 = format!("http://{addr2}");

    let mut cmd2 = assert_cmd::Command::cargo_bin("claude-rs").expect("cargo_bin");
    cmd2.current_dir(work.path());
    cmd2.env("CLAUDE_CONFIG_DIR", cfg.path());
    cmd2.env("ANTHROPIC_BASE_URL", base_url2);
    cmd2.env("ANTHROPIC_API_KEY", "test-key");
    cmd2.env("CLAUDE_RS_EXTRACT_MEMORIES", "0");
    cmd2.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd2.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    cmd2.args(["--continue", "second prompt"]);

    cmd2.assert()
        .success()
        .stdout(predicate::str::contains("Second response"));

    handle2.join().expect("server thread");
}
