use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use similar::TextDiff;

use crate::util::{absolutize, expand_tilde, is_path_allowed, normalize_path};
use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Edit";

#[derive(Debug, Default, Clone)]
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "file_path": {
              "type": "string",
              "description": "The absolute path to the file to modify"
            },
            "old_string": {
              "type": "string",
              "description": "The text to replace"
            },
            "new_string": {
              "type": "string",
              "description": "The replacement text"
            },
            "replace_all": {
              "type": "boolean",
              "description": "Replace all occurrences (default false)"
            }
          },
          "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn prompt(&self) -> String {
        "Edit a file by replacing `old_string` with `new_string` (first occurrence by default)."
            .to_string()
    }

    async fn check_permissions(
        &self,
        input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        if !ctx.allows_dangerous_tools() {
            return PermissionResult::deny(
                "Edit is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            );
        }

        let Some(raw) = input.get("file_path").and_then(|v| v.as_str()) else {
            return PermissionResult::deny("missing required field: file_path");
        };
        let path = resolve_path(ctx, raw);
        if is_path_allowed(ctx, &path) {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(format!(
                "Edit is not allowed outside the working directory. Path: {}",
                path.display()
            ))
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let old_string = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let new_string = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if file_path.trim().is_empty() {
            return Ok(ToolResult::err_text("missing required field: file_path"));
        }
        if old_string == new_string {
            return Ok(ToolResult::err_text(
                "old_string and new_string are identical; no edit to apply",
            ));
        }

        let path = resolve_path(ctx, file_path);
        let old = old_string.to_string();
        let new = new_string.to_string();

        let summary =
            tokio::task::spawn_blocking(move || edit_file(path, &old, &new, replace_all)).await??;

        Ok(ToolResult::ok_text(summary))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        // File edits are order-sensitive and should not run concurrently with other
        // filesystem mutations.
        false
    }
}

fn resolve_path(ctx: &ToolUseContext, raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw.trim());
    let abs = absolutize(&ctx.cwd, &expanded);
    normalize_path(&abs)
}

fn edit_file(
    path: PathBuf,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> anyhow::Result<String> {
    let existed = path.exists();

    if !existed {
        if old_string.is_empty() {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent).map_err(|e| {
                        anyhow::anyhow!("failed to create parent dirs for {}: {e}", path.display())
                    })?;
                }
            }
            fs::write(&path, new_string.as_bytes())
                .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;

            let header_a = format!("a/{}", path.display());
            let header_b = format!("b/{}", path.display());
            let diff = TextDiff::from_lines("", new_string)
                .unified_diff()
                .header(&header_a, &header_b)
                .to_string();

            let mut out = format!(
                "Created {} via Edit ({} bytes)\n",
                path.display(),
                new_string.len()
            );
            if !diff.trim().is_empty() {
                out.push('\n');
                out.push_str(&diff);
            }
            return Ok(out);
        }

        anyhow::bail!("file does not exist: {}", path.display());
    }

    if old_string.is_empty() {
        anyhow::bail!("old_string must be non-empty when editing an existing file");
    }

    let original = fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;

    let (updated, count) = if replace_all {
        let count = original.matches(old_string).count();
        (original.replace(old_string, new_string), count)
    } else {
        match original.find(old_string) {
            Some(idx) => {
                let mut s = String::with_capacity(
                    original
                        .len()
                        .saturating_sub(old_string.len())
                        .saturating_add(new_string.len()),
                );
                s.push_str(&original[..idx]);
                s.push_str(new_string);
                s.push_str(&original[idx + old_string.len()..]);
                (s, 1)
            }
            None => (original.clone(), 0),
        }
    };

    if count == 0 {
        anyhow::bail!("old_string not found in {}", path.display());
    }

    fs::write(&path, updated.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;

    let mode = if replace_all { "all" } else { "first" };
    let header_a = format!("a/{}", path.display());
    let header_b = format!("b/{}", path.display());
    let diff = TextDiff::from_lines(&original, &updated)
        .unified_diff()
        .header(&header_a, &header_b)
        .to_string();

    let mut out = format!(
        "Edited {} (replaced {mode} occurrence(s): {count})\n",
        path.display()
    );
    if !diff.trim().is_empty() {
        out.push('\n');
        out.push_str(&diff);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("claude-tools-{name}-{nanos}.txt"))
    }

    #[test]
    fn edit_file_includes_unified_diff() {
        let path = temp_path("edit");
        fs::write(&path, b"hello\nworld\n").expect("write test file");

        let out = edit_file(path.clone(), "world", "rust", false).expect("edit should succeed");

        let updated = fs::read_to_string(&path).expect("read updated file");
        assert_eq!(updated, "hello\nrust\n");

        assert!(out.contains("Edited "));
        assert!(out.contains("--- a/"));
        assert!(out.contains("+++ b/"));
        assert!(out.contains("-world"));
        assert!(out.contains("+rust"));

        let _ = fs::remove_file(&path);
    }
}
