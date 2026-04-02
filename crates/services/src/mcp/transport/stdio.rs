use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin};

use claude_core::config::mcp::McpStdioServerConfig;

#[derive(Debug)]
pub struct StdioTransport {
    name: String,
    child: Child,
    stdin: ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl StdioTransport {
    pub async fn connect(name: &str, cfg: &McpStdioServerConfig) -> anyhow::Result<Self> {
        if cfg.command.trim().is_empty() {
            anyhow::bail!("mcp server {name}: missing command");
        }

        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        // Inherit current env, apply overrides.
        if let Some(env) = &cfg.env {
            cmd.envs(env);
        }

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("mcp server {name}: failed to open stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("mcp server {name}: failed to open stdout pipe"))?;

        let stdout_lines = BufReader::new(stdout).lines();

        Ok(Self {
            name: name.to_string(),
            child,
            stdin,
            stdout_lines,
        })
    }

    pub async fn send_json(&mut self, value: &serde_json::Value) -> anyhow::Result<()> {
        let s = serde_json::to_string(value)?;
        self.stdin.write_all(s.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn next_json(&mut self) -> Option<anyhow::Result<serde_json::Value>> {
        match self.stdout_lines.next_line().await {
            Ok(Some(line)) => Some(parse_line(&self.name, &line)),
            Ok(None) => None,
            Err(err) => Some(Err(err.into())),
        }
    }

    pub async fn close(&mut self) {
        // Best-effort graceful shutdown.
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

fn parse_line(name: &str, line: &str) -> anyhow::Result<serde_json::Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        anyhow::bail!("mcp server {name}: empty JSON-RPC line");
    }
    let v: serde_json::Value = serde_json::from_str(trimmed)?;
    Ok(v)
}
