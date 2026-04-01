use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{Result, errors::CoreError};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub allowed_tools: Vec<String>,

    #[serde(default)]
    pub mcp_context_uris: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_access_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_refresh_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_expires_at: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    #[serde(default)]
    pub num_startups: u64,

    #[serde(default)]
    pub migration_version: u32,

    #[serde(default)]
    pub projects: HashMap<String, ProjectConfig>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            user_id: None,
            oauth_access_token: None,
            oauth_refresh_token: None,
            oauth_expires_at: None,
            api_key: None,
            num_startups: 0,
            migration_version: 0,
            projects: HashMap::new(),
        }
    }
}

/// Default global config path: `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`.
pub fn default_global_config_path() -> Result<PathBuf> {
    let filename = ".claude.json";

    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join(filename));
    }

    let Some(home) = dirs::home_dir() else {
        return Err(CoreError::NoHomeDir);
    };

    Ok(home.join(filename))
}

pub fn load_global_config(path: &Path) -> Result<GlobalConfig> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GlobalConfig::default());
        }
        Err(source) => return Err(CoreError::Io { source }),
    };

    if bytes.is_empty() {
        return Ok(GlobalConfig::default());
    }

    let cfg: GlobalConfig = serde_json::from_slice(&bytes)?;
    Ok(cfg)
}

pub fn save_global_config(path: &Path, cfg: &GlobalConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let bytes = serde_json::to_vec_pretty(cfg)?;
    fs::write(path, bytes)?;
    Ok(())
}
