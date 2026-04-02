use std::path::{Path, PathBuf};

use async_trait::async_trait;
use grep::matcher::Matcher as _;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, sinks::Bytes};

use crate::util::{absolutize, expand_tilde, is_path_allowed, normalize_path};
use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Grep";
const DEFAULT_HEAD_LIMIT: usize = 250;

#[derive(Debug, Default, Clone)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
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
              "description": "The regex pattern to search for"
            },
            "path": {
              "type": "string",
              "description": "File or directory to search in (defaults to cwd)"
            },
            "glob": {
              "type": "string",
              "description": "Optional glob filter for files (e.g. *.rs, **/*.ts)"
            },
            "output_mode": {
              "type": "string",
              "enum": ["content", "files_with_matches", "count"],
              "description": "Output mode (default files_with_matches)"
            },
            "-i": {
              "type": "boolean",
              "description": "Case-insensitive search"
            },
            "head_limit": {
              "type": "integer",
              "minimum": 0,
              "description": "Limit output entries. 0 means unlimited. Default 250."
            },
            "offset": {
              "type": "integer",
              "minimum": 0,
              "description": "Skip first N entries before applying head_limit"
            },
            "multiline": {
              "type": "boolean",
              "description": "Enable multiline mode (best-effort)"
            }
          },
          "required": ["pattern"]
        })
    }

    fn prompt(&self) -> String {
        "Search file contents using a regex (ripgrep-like). Use output_mode=files_with_matches to list files, content to show lines, or count to show counts.".to_string()
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
                "Grep is not allowed outside the working directory. Path: {}",
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

        let glob = input
            .get("glob")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let output_mode = input
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches")
            .to_string();

        let case_insensitive = input.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);

        let head_limit = input
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_HEAD_LIMIT);

        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0);

        let multiline = input
            .get("multiline")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let cwd = ctx.cwd.clone();

        let out = tokio::task::spawn_blocking(move || {
            grep_search(
                &cwd,
                &root,
                &pattern,
                glob.as_deref(),
                &output_mode,
                case_insensitive,
                multiline,
                head_limit,
                offset,
            )
        })
        .await??;

        Ok(ToolResult::ok_text(out))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        20_000
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
            if s.is_empty() {
                ".".to_string()
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn normalize_glob_filter(glob: &str) -> String {
    let g = glob.trim();
    if g.is_empty() {
        return "**/*".to_string();
    }

    // Ripgrep's --glob applies recursively; mimic that for simple patterns.
    if g.contains('/') || g.contains("**") {
        g.to_string()
    } else {
        format!("**/{g}")
    }
}

fn grep_search(
    cwd: &Path,
    root: &Path,
    pattern: &str,
    glob: Option<&str>,
    output_mode: &str,
    case_insensitive: bool,
    multiline: bool,
    head_limit: usize,
    offset: usize,
) -> anyhow::Result<String> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(case_insensitive);
    if multiline {
        // Best-effort parity with the TS CLI: enable ^/$ per-line and allow `.`
        // to match newlines when multi-line search is enabled.
        builder.multi_line(true);
        builder.dot_matches_new_line(true);
    }
    let matcher = builder
        .build(pattern)
        .map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;

    let mut files: Vec<PathBuf> = Vec::new();

    if root.is_file() {
        files.push(root.to_path_buf());
    } else {
        let pat = glob
            .map(normalize_glob_filter)
            .unwrap_or_else(|| "**/*".to_string());
        let walker = globwalk::GlobWalkerBuilder::from_patterns(root, &[pat.as_str()])
            .follow_links(false)
            .build()
            .map_err(|e| anyhow::anyhow!("invalid glob: {e}"))?;

        for entry in walker.into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            if path.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            files.push(path);
        }

        files.sort();
    }

    // If head_limit=0 then output is unlimited; otherwise stop after collecting
    // enough entries to fill (offset + head_limit).
    let wanted_total = if head_limit == 0 {
        None
    } else {
        Some(offset.saturating_add(head_limit))
    };

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder.binary_detection(BinaryDetection::quit(0));
    searcher_builder.multi_line(multiline);
    let mut searcher = searcher_builder.build();

    match output_mode {
        "content" => {
            let mut lines: Vec<String> = Vec::new();
            let mut stop_all = false;
            for path in files {
                let rel = to_relative(&path, cwd);
                let wanted_total = wanted_total;

                let res = searcher.search_path(
                    &matcher,
                    &path,
                    Bytes(|lnum, bytes| {
                        let bytes = strip_line_term(bytes);
                        let mut line = String::from_utf8_lossy(bytes)
                            .trim_end_matches(['\n', '\r'])
                            .to_string();
                        if multiline {
                            line = line.replace('\r', "\\r").replace('\n', "\\n");
                        }

                        lines.push(format!("{rel}:{lnum}:{line}"));

                        if let Some(want) = wanted_total {
                            if lines.len() >= want {
                                stop_all = true;
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }),
                );

                if res.is_err() {
                    continue;
                }

                if stop_all {
                    break;
                }
            }

            let (shown, truncated) = apply_limit(lines, head_limit, offset);
            if shown.is_empty() {
                return Ok("No matches found".to_string());
            }

            let mut out = shown.join("\n");
            if truncated {
                out.push_str("\n(Results are truncated. Use offset/head_limit to paginate.)");
            }
            Ok(out)
        }
        "count" => {
            let mut entries: Vec<String> = Vec::new();
            let mut stop_all = false;
            for path in files {
                let rel = to_relative(&path, cwd);
                let mut count: usize = 0;

                let res = searcher.search_path(
                    &matcher,
                    &path,
                    Bytes(|_lnum, bytes| {
                        let bytes = strip_line_term(bytes);
                        matcher
                            .find_iter(bytes, |m| {
                                let _ = m;
                                count += 1;
                                true
                            })
                            .unwrap();
                        Ok(true)
                    }),
                );

                if res.is_err() {
                    continue;
                }

                if count > 0 {
                    entries.push(format!("{rel}: {count}"));
                }

                if let Some(want) = wanted_total {
                    if entries.len() >= want {
                        stop_all = true;
                    }
                }
                if stop_all {
                    break;
                }
            }

            let (shown, truncated) = apply_limit(entries, head_limit, offset);
            if shown.is_empty() {
                return Ok("No matches found".to_string());
            }

            let mut out = shown.join("\n");
            if truncated {
                out.push_str("\n(Results are truncated. Use offset/head_limit to paginate.)");
            }
            Ok(out)
        }
        // files_with_matches (default)
        _ => {
            let mut matched: Vec<String> = Vec::new();
            let mut stop_all = false;
            for path in files {
                let rel = to_relative(&path, cwd);
                let mut any = false;
                let res = searcher.search_path(
                    &matcher,
                    &path,
                    Bytes(|_lnum, _bytes| {
                        any = true;
                        Ok(false)
                    }),
                );

                if res.is_err() {
                    continue;
                }

                if any {
                    matched.push(rel);
                }

                if let Some(want) = wanted_total {
                    if matched.len() >= want {
                        stop_all = true;
                    }
                }
                if stop_all {
                    break;
                }
            }

            let (shown, truncated) = apply_limit(matched, head_limit, offset);
            if shown.is_empty() {
                return Ok("No matches found".to_string());
            }

            let mut out = shown.join("\n");
            if truncated {
                out.push_str("\n(Results are truncated. Use offset/head_limit to paginate.)");
            }
            Ok(out)
        }
    }
}

fn strip_line_term(mut bytes: &[u8]) -> &[u8] {
    if bytes.ends_with(b"\n") {
        bytes = &bytes[..bytes.len().saturating_sub(1)];
    }
    if bytes.ends_with(b"\r") {
        bytes = &bytes[..bytes.len().saturating_sub(1)];
    }
    bytes
}

fn apply_limit(items: Vec<String>, head_limit: usize, offset: usize) -> (Vec<String>, bool) {
    let items: Vec<String> = if offset > 0 {
        items.into_iter().skip(offset).collect()
    } else {
        items
    };

    if head_limit == 0 {
        return (items, false);
    }

    let truncated = items.len() > head_limit;
    let shown = items.into_iter().take(head_limit).collect();
    (shown, truncated)
}
