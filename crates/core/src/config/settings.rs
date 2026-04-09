use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result, config::mcp::McpServerConfig, types::permissions::PermissionMode};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EditorMode {
    Normal,
    Vim,
    #[serde(alias = "emacs")]
    Emacs,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(
        default,
        rename = "permissionMode",
        alias = "permission_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,

    /// Tools that should be auto-approved in `permissionMode=default`.
    ///
    /// This does not affect which tools are exposed to the model; it only
    /// bypasses the interactive permission prompt for those tool names.
    #[serde(
        default,
        rename = "alwaysAllowTools",
        alias = "always_allow_tools",
        skip_serializing_if = "Option::is_none"
    )]
    pub always_allow_tools: Option<Vec<String>>,

    #[serde(
        default,
        rename = "apiKeyHelper",
        alias = "api_key_helper",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key_helper: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    #[serde(
        default,
        rename = "mcpServers",
        alias = "mcp_servers",
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,

    #[serde(
        default,
        rename = "allowedTools",
        alias = "allowed_tools",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_tools: Option<Vec<String>>,

    #[serde(
        default,
        rename = "disallowedTools",
        alias = "disallowed_tools",
        skip_serializing_if = "Option::is_none"
    )]
    pub disallowed_tools: Option<Vec<String>>,

    #[serde(
        default,
        rename = "customSystemPrompt",
        alias = "custom_system_prompt",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_system_prompt: Option<String>,

    #[serde(
        default,
        rename = "editorMode",
        alias = "editor_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub editor_mode: Option<EditorMode>,

    /// Global TUI keybinding overrides. Keys are action names, values are key
    /// sequences like `ctrl+x ctrl+k` or `tab`.
    #[serde(
        default,
        rename = "tuiKeybindings",
        alias = "tui_keybindings",
        skip_serializing_if = "Option::is_none"
    )]
    pub tui_keybindings: Option<HashMap<String, String>>,

    #[serde(
        default,
        rename = "tuiTheme",
        alias = "tui_theme",
        skip_serializing_if = "Option::is_none"
    )]
    pub tui_theme: Option<String>,

    /// When true, render assistant `thinking` blocks in the TUI transcript.
    /// When false, show a short placeholder instead (collapsible via `/thinking on`).
    #[serde(
        default,
        rename = "tuiShowThinking",
        alias = "tui_show_thinking",
        skip_serializing_if = "Option::is_none"
    )]
    pub tui_show_thinking: Option<bool>,

    /// When true, collapse tool results in the TUI transcript (toggle via `/condensed`).
    #[serde(
        default,
        rename = "tuiCondensed",
        alias = "tui_condensed",
        skip_serializing_if = "Option::is_none"
    )]
    pub tui_condensed: Option<bool>,

    #[serde(
        default,
        rename = "tuiOnboardingSeen",
        alias = "tui_onboarding_seen",
        skip_serializing_if = "Option::is_none"
    )]
    pub tui_onboarding_seen: Option<bool>,
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
        let mut merged_always_allow: Vec<String> = Vec::new();
        let mut saw_always_allow = false;
        let mut always_allow_seen: HashSet<String> = HashSet::new();
        let mut merged_tui_keybindings: HashMap<String, String> = HashMap::new();
        let mut saw_tui_keybindings = false;

        for layer in layers {
            if layer.model.is_some() {
                out.model = layer.model.clone();
            }
            if layer.permission_mode.is_some() {
                out.permission_mode = layer.permission_mode;
            }
            if let Some(list) = &layer.always_allow_tools {
                saw_always_allow = true;
                for t in list {
                    let t = t.trim();
                    if t.is_empty() {
                        continue;
                    }
                    let key = t.to_ascii_lowercase();
                    if always_allow_seen.insert(key) {
                        merged_always_allow.push(t.to_string());
                    }
                }
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
            if layer.editor_mode.is_some() {
                out.editor_mode = layer.editor_mode;
            }
            if let Some(map) = &layer.tui_keybindings {
                saw_tui_keybindings = true;
                for (k, v) in map {
                    merged_tui_keybindings.insert(k.clone(), v.clone());
                }
            }
            if layer.tui_theme.is_some() {
                out.tui_theme = layer.tui_theme.clone();
            }
            if layer.tui_show_thinking.is_some() {
                out.tui_show_thinking = layer.tui_show_thinking;
            }
            if layer.tui_condensed.is_some() {
                out.tui_condensed = layer.tui_condensed;
            }
            if layer.tui_onboarding_seen.is_some() {
                out.tui_onboarding_seen = layer.tui_onboarding_seen;
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

        if saw_always_allow {
            out.always_allow_tools = Some(merged_always_allow);
        }

        if saw_tui_keybindings {
            out.tui_keybindings = Some(merged_tui_keybindings);
        }

        out
    }
}

pub fn load_settings_file(path: &Path) -> Result<Settings> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(_err) => {
            return Err(CoreError::ReadConfig {
                path: path.to_path_buf(),
            });
        }
    };

    if bytes.is_empty() {
        return Ok(Settings::default());
    }

    let cfg: Settings = serde_json::from_slice(&bytes)?;
    Ok(cfg)
}

pub fn save_settings_file(path: &Path, cfg: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let bytes = serde_json::to_vec_pretty(cfg)?;
    fs::write(path, bytes).map_err(|_source| CoreError::WriteConfig {
        path: path.to_path_buf(),
    })?;
    Ok(())
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
