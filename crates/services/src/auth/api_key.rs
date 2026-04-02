use tokio::process::Command;

use crate::{Result, ServicesError};

pub fn api_key_from_env() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn api_key_from_helper(helper: &str) -> Result<String> {
    // Mirrors Node's `exec` behavior reasonably well: user config is a shell snippet.
    let output = Command::new("sh")
        .arg("-lc")
        .arg(helper)
        .output()
        .await
        .map_err(|source| ServicesError::Io { source })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServicesError::ApiKeyHelper {
            detail: format!("command failed: {stderr}"),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let key = stdout.trim();
    if key.is_empty() {
        return Err(ServicesError::ApiKeyHelper {
            detail: "command returned empty stdout".to_string(),
        });
    }

    Ok(key.to_string())
}
