use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::util::{absolutize, expand_tilde, is_path_allowed, normalize_path, truncate_chars};
use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Glob";
const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Default, Clone)]
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "pattern": {
              "type": "string",
              "description": "The glob pattern to match files against (e.g. **/*.rs)"
            },
            "path": {
              "type": "string",
              "description": "Directory to search in (defaults to current working directory)"
            }
          },
          "required": ["pattern"]
        })
    }

    fn prompt(&self) -> String {
        "Find files by glob pattern (fast, recursive). Returns matching file paths.".to_string()
    }

    async fn check_permissions(
        &self,
        input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        let root = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => resolve_path(ctx, p),
            _ => ctx.cwd.clone(),
        };

        if is_path_allowed(ctx, &root) {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(format!(
                "Glob is not allowed outside the working directory. Path: {}",
                root.display()
            ))
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if pattern.is_empty() {
            return Ok(ToolResult::err_text("missing required field: pattern"));
        }

        let root = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => resolve_path(ctx, p),
            _ => ctx.cwd.clone(),
        };

        let cwd = ctx.cwd.clone();
        let limit = DEFAULT_LIMIT;

        let out = tokio::task::spawn_blocking(move || glob_search(&cwd, &root, &pattern, limit))
            .await??;

        let (out, truncated_by_chars) = truncate_chars(&out, self.max_result_size_chars());
        let out = if truncated_by_chars {
            format!("{out}\n(output truncated)")
        } else {
            out
        };

        Ok(ToolResult::ok_text(out))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
}

fn resolve_path(ctx: &ToolUseContext, raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw.trim());
    let abs = absolutize(&ctx.cwd, &expanded);
    normalize_path(&abs)
}

fn to_relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .ok()
        .map(|p| {
            let s = p.to_string_lossy();
            // Keep relative paths explicit so the model understands they are under cwd.
            if s.is_empty() {
                ".".to_string()
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn glob_search(cwd: &Path, root: &Path, pattern: &str, limit: usize) -> anyhow::Result<String> {
    let mut matches: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();

    let walker = globwalk::GlobWalkerBuilder::from_patterns(root, &[pattern])
        .follow_links(false)
        .build()
        .map_err(|e| anyhow::anyhow!("invalid glob: {e}"))?;

    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        // Best-effort mtime; unknown times sort last.
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        matches.push((mtime, path));
    }

    matches.sort_by(|(a_time, a_path), (b_time, b_path)| {
        b_time
            .cmp(a_time)
            .then_with(|| a_path.as_os_str().cmp(b_path.as_os_str()))
    });

    let truncated = matches.len() > limit;
    let shown = matches.into_iter().take(limit);

    let mut out_paths: Vec<String> = Vec::new();
    for (_mtime, path) in shown {
        out_paths.push(to_relative(&path, cwd));
    }

    if out_paths.is_empty() {
        return Ok("No files found".to_string());
    }

    let mut out = out_paths.join("\n");
    if truncated {
        out.push_str("\n(Results are truncated. Use a more specific pattern.)");
    }
    Ok(out)
}
