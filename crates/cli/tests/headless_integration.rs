use predicates::prelude::*;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::thread;
use std::time::Duration;

fn spawn_mock_sse_server(sse_body: String) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("server addr");

    let handle = thread::spawn(move || {
        let (mut stream, _peer) = listener.accept().expect("accept");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

        // Read until the end of headers so we can parse Content-Length and drain the request body.
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

        // Drain the request body (best-effort) so the client can finish sending.
        let already_body = buf.len().saturating_sub(header_end);
        let mut remaining = content_length.saturating_sub(already_body);
        while remaining > 0 {
            let n = stream.read(&mut tmp).unwrap_or(0);
            if n == 0 {
                break;
            }
            remaining = remaining.saturating_sub(n);
        }

        let (status_line, body) = if method == "POST" && path == "/v1/messages" {
            ("HTTP/1.1 200 OK\r\n", sse_body)
        } else {
            ("HTTP/1.1 404 Not Found\r\n", "not found".to_string())
        };

        let resp = format!(
            "{status_line}Content-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.as_bytes().len(),
        );
        stream.write_all(resp.as_bytes()).expect("write response");
        stream.flush().ok();
    });

    (addr, handle)
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

    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(&ev.to_string());
        body.push('\n');
        body.push('\n');
    }
    body
}

#[test]
fn runs_with_positional_prompt_without_print_flag() {
    let cfg = tempfile::tempdir().expect("temp config dir");
    let work = tempfile::tempdir().expect("temp work dir");

    let (addr, handle) = spawn_mock_sse_server(mock_sse_ok_text("Hello from mock"));
    let base_url = format!("http://{addr}");

    let mut cmd = assert_cmd::Command::cargo_bin("claude-rs").expect("cargo_bin");
    cmd.current_dir(work.path());
    cmd.env("CLAUDE_CONFIG_DIR", cfg.path());
    cmd.env("ANTHROPIC_BASE_URL", base_url);
    cmd.env("ANTHROPIC_API_KEY", "test-key");
    cmd.env("CLAUDE_RS_EXTRACT_MEMORIES", "0");
    cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    cmd.arg("Say hello");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Hello from mock"));

    handle.join().expect("server thread");
}

#[test]
fn runs_with_piped_stdin_without_print_flag() {
    let cfg = tempfile::tempdir().expect("temp config dir");
    let work = tempfile::tempdir().expect("temp work dir");

    let (addr, handle) = spawn_mock_sse_server(mock_sse_ok_text("Hello from mock"));
    let base_url = format!("http://{addr}");

    let mut cmd = assert_cmd::Command::cargo_bin("claude-rs").expect("cargo_bin");
    cmd.current_dir(work.path());
    cmd.env("CLAUDE_CONFIG_DIR", cfg.path());
    cmd.env("ANTHROPIC_BASE_URL", base_url);
    cmd.env("ANTHROPIC_API_KEY", "test-key");
    cmd.env("CLAUDE_RS_EXTRACT_MEMORIES", "0");
    cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    cmd.write_stdin("prompt from stdin\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Hello from mock"));

    handle.join().expect("server thread");
}
