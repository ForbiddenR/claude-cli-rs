use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result, config::mcp::McpServerConfig, types::permissions::PermissionMode};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(
        default,
        alias = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,

    #[serde(
        default,
        alias = "apiKeyHelper",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key_helper: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    #[serde(default, alias = "mcpServers", skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,

    #[serde(
        default,
        alias = "allowedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_tools: Option<Vec<String>>,

    #[serde(
        default,
        alias = "disallowedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub disallowed_tools: Option<Vec<String>>,

    #[serde(
        default,
        alias = "customSystemPrompt",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_system_prompt: Option<String>,
}

impl Settings {
    /// Merge layers: earlier layers provide defaults, later layers override.
    ///
    /// Intended precedence: user < project < local < flag < policy.
    pub fn merge(layers: &[Settings]) -> Settings {
        let mut out = Settings::default();
        let mut merged_env: HashMap<String, String> = HashMap::new();
        let mut saw_env = false;
        let mut merged_mcp: HashMap<String, McpServerConfig> = HashMap::new();
        let mut saw_mcp = false;

        for layer in layers {
            if layer.model.is_some() {
                out.model = layer.model.clone();
            }
            if layer.permission_mode.is_some() {
                out.permission_mode = layer.permission_mode;
            }
            if layer.api_key_helper.is_some() {
                out.api_key_helper = layer.api_key_helper.clone();
            }
            if layer.allowed_tools.is_some() {
                out.allowed_tools = layer.allowed_tools.clone();
            }
            if layer.disallowed_tools.is_some() {
                out.disallowed_tools = layer.disallowed_tools.clone();
            }
            if layer.custom_system_prompt.is_some() {
                out.custom_system_prompt = layer.custom_system_prompt.clone();
            }

            if let Some(env) = &layer.env {
                saw_env = true;
                for (k, v) in env {
                    merged_env.insert(k.clone(), v.clone());
                }
            }

            if let Some(servers) = &layer.mcp_servers {
                saw_mcp = true;
                for (k, v) in servers {
                    merged_mcp.insert(k.clone(), v.clone());
                }
            }
        }

        if saw_env {
            out.env = Some(merged_env);
        }

        if saw_mcp {
            out.mcp_servers = Some(merged_mcp);
        }

        out
    }
}

pub fn load_settings_file(path: &Path) -> Result<Settings> {
    let bytes = fs::read(path).map_err(|_source| CoreError::ReadConfig {
        path: path.to_path_buf(),
    })?;

    if bytes.is_empty() {
        return Ok(Settings::default());
    }

    let cfg: Settings = serde_json::from_slice(&bytes)?;
    Ok(cfg)
}

/// Load a settings argument, which can be either:
/// - a filesystem path to a JSON file, or
/// - an inline JSON string.
pub fn load_settings_arg(raw: &str) -> Result<Settings> {
    let as_path = PathBuf::from(raw);
    if as_path.exists() {
        return load_settings_file(&as_path);
    }

    serde_json::from_str(raw).map_err(|source| CoreError::InvalidSettingsInput {
        detail: source.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_env_overrides_by_key() {
        let a = Settings {
            env: Some(HashMap::from([
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "1".to_string()),
            ])),
            ..Settings::default()
        };
        let b = Settings {
            env: Some(HashMap::from([
                ("B".to_string(), "2".to_string()),
                ("C".to_string(), "2".to_string()),
            ])),
            ..Settings::default()
        };

        let merged = Settings::merge(&[a, b]);
        let env = merged.env.expect("env should be present");
        assert_eq!(env.get("A").map(String::as_str), Some("1"));
        assert_eq!(env.get("B").map(String::as_str), Some("2"));
        assert_eq!(env.get("C").map(String::as_str), Some("2"));
    }

    #[test]
    fn load_settings_arg_supports_camel_case_aliases() {
        let s = load_settings_arg(r#"{ "apiKeyHelper": "echo hi", "allowedTools": ["bash"] }"#)
            .expect("should parse");
        assert_eq!(s.api_key_helper.as_deref(), Some("echo hi"));
        assert_eq!(s.allowed_tools.unwrap(), vec!["bash".to_string()]);
    }
}
