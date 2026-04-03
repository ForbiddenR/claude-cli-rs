use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::util::{absolutize, expand_tilde, is_path_allowed, normalize_path};
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

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::types::permissions::PermissionMode;
    use std::sync::Arc;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("claude-tools-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn ctx_for(cwd: PathBuf, mode: PermissionMode) -> ToolUseContext {
        let store_dir = cwd.join(".claude-tools-test-results");
        ToolUseContext {
            cwd: cwd.clone(),
            allowed_roots: vec![cwd],
            permission_mode: mode,
            session: Arc::new(crate::SessionState::default()),
            result_store: Arc::new(crate::ToolResultStore::new(store_dir).expect("store")),
            agent: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let cwd = temp_dir("glob");
        std::fs::create_dir_all(cwd.join("sub")).expect("mkdir");
        std::fs::write(cwd.join("a.rs"), "fn a() {}\n").expect("write a.rs");
        std::fs::write(cwd.join("b.txt"), "hello\n").expect("write b.txt");
        std::fs::write(cwd.join("sub").join("c.rs"), "fn c() {}\n").expect("write c.rs");

        let mut ctx = ctx_for(cwd.clone(), PermissionMode::Default);
        let tool = GlobTool::default();
        let input = serde_json::json!({ "pattern": "**/*.rs" });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);

        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("a.rs"));
        assert!(out.contains("c.rs"));
        assert!(!out.contains("b.txt"));
    }

    #[tokio::test]
    async fn glob_is_denied_outside_allowed_roots() {
        let cwd = temp_dir("glob-cwd");
        let outside = temp_dir("glob-outside");

        let ctx = ctx_for(cwd.clone(), PermissionMode::Default);
        let tool = GlobTool::default();
        let input = serde_json::json!({
            "pattern": "**/*",
            "path": outside.to_string_lossy().to_string(),
        });

        assert!(!tool.check_permissions(&input, &ctx).await.is_allowed());
    }
}
