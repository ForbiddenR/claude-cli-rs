use std::path::PathBuf;

use crate::{CoreError, Result};

/// Claude Code config directory:
/// - `$CLAUDE_CONFIG_DIR` if set
/// - otherwise `~/.claude`
pub fn claude_config_home_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }

    let Some(home) = dirs::home_dir() else {
        return Err(CoreError::NoHomeDir);
    };

    Ok(home.join(".claude"))
}

