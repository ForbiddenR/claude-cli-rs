use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Serialize;

use crate::util::{absolutize, expand_tilde, is_path_allowed, normalize_path};
use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "NotebookEdit";

#[derive(Debug, Default, Clone)]
pub struct NotebookEditTool;

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "notebook_path": {
              "type": "string",
              "description": "Path to the .ipynb file to edit (absolute preferred; relative is resolved from cwd)"
            },
            "cell_id": {
              "type": "string",
              "description": "Cell ID to edit. For notebooks without per-cell IDs, use cell-N (e.g., cell-0). Required unless edit_mode=insert."
            },
            "new_source": { "type": "string", "description": "The new source for the cell" },
            "cell_type": {
              "type": "string",
              "enum": ["code", "markdown"],
              "description": "Cell type (required for insert; optional for replace)"
            },
            "edit_mode": {
              "type": "string",
              "enum": ["replace", "insert", "delete"],
              "description": "Edit mode (defaults to replace)"
            }
          },
          "required": ["notebook_path", "new_source"]
        })
    }

    fn prompt(&self) -> String {
        "Edit a Jupyter notebook (.ipynb) by replacing, inserting, or deleting a cell.".to_string()
    }

    async fn check_permissions(
        &self,
        input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        if !ctx.allows_dangerous_tools() {
            return PermissionResult::deny(
                "NotebookEdit is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            );
        }

        let Some(raw) = input.get("notebook_path").and_then(|v| v.as_str()) else {
            return PermissionResult::deny("missing required field: notebook_path");
        };
        let path = resolve_path(ctx, raw);
        if !path.extension().is_some_and(|e| e == "ipynb") {
            return PermissionResult::deny("file must have .ipynb extension");
        }
        if is_path_allowed(ctx, &path) {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(format!(
                "NotebookEdit is not allowed outside the working directory. Path: {}",
                path.display()
            ))
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let notebook_path = input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if notebook_path.is_empty() {
            return Ok(ToolResult::err_text(
                "missing required field: notebook_path",
            ));
        }

        let new_source = input
            .get("new_source")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let cell_id = input
            .get("cell_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let cell_type = input
            .get("cell_type")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let original_mode = input
            .get("edit_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("replace")
            .trim()
            .to_string();

        let path = resolve_path(ctx, &notebook_path);

        let res = tokio::task::spawn_blocking(move || {
            edit_notebook(
                path,
                new_source,
                cell_id,
                cell_type.as_deref(),
                &original_mode,
            )
        })
        .await??;

        Ok(res)
    }
}

fn resolve_path(ctx: &ToolUseContext, raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw.trim());
    let abs = absolutize(&ctx.cwd, &expanded);
    normalize_path(&abs)
}

fn parse_cell_index(cell_id: &str) -> Option<usize> {
    let rest = cell_id.strip_prefix("cell-")?;
    rest.parse::<usize>().ok()
}

fn cell_display_id(cell: &serde_json::Value, idx: usize) -> String {
    cell.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("cell-{idx}"))
}

fn edit_notebook(
    path: PathBuf,
    new_source: String,
    cell_id: Option<String>,
    mut cell_type: Option<&str>,
    original_mode: &str,
) -> anyhow::Result<ToolResult> {
    if path.extension().is_none_or(|e| e != "ipynb") {
        return Ok(ToolResult::err_text(
            "File must be a Jupyter notebook (.ipynb). For other files, use Edit/Write.",
        ));
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    let mut notebook: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("notebook is not valid JSON: {e}"))?;

    let language = notebook
        .get("metadata")
        .and_then(|m| m.get("language_info"))
        .and_then(|li| li.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("python")
        .to_string();

    let cells = notebook
        .get_mut("cells")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("notebook JSON missing top-level cells[]"))?;

    let mut cell_index: usize;
    if cell_id.is_none() {
        if original_mode != "insert" {
            return Ok(ToolResult::err_text(
                "cell_id must be specified when edit_mode is not insert",
            ));
        }
        cell_index = 0;
    } else {
        let id = cell_id.as_deref().unwrap();
        cell_index = cells
            .iter()
            .enumerate()
            .find_map(|(i, c)| {
                if c.get("id").and_then(|v| v.as_str()) == Some(id) || format!("cell-{i}") == id {
                    Some(i)
                } else {
                    None
                }
            })
            .or_else(|| parse_cell_index(id).filter(|&i| i < cells.len()))
            .ok_or_else(|| anyhow::anyhow!("cell with ID \"{id}\" not found in notebook"))?;

        if original_mode == "insert" {
            cell_index = cell_index.saturating_add(1);
        }
    }

    let mut edit_mode = original_mode.to_string();
    if edit_mode == "replace" && cell_index == cells.len() {
        // Allow replace-one-past-end as insert-at-end.
        edit_mode = "insert".to_string();
        if cell_type.is_none() {
            cell_type = Some("code");
        }
    }

    match edit_mode.as_str() {
        "delete" => {
            if cell_index >= cells.len() {
                return Ok(ToolResult::err_text("cell index out of bounds"));
            }
            let removed = cells.remove(cell_index);
            let id = cell_display_id(&removed, cell_index);
            write_notebook(&path, &notebook)?;
            return Ok(ToolResult::ok_text(format!("Deleted cell {id}")));
        }
        "insert" => {
            let Some(cell_type) = cell_type else {
                return Ok(ToolResult::err_text(
                    "cell_type is required when using edit_mode=insert",
                ));
            };

            let new_id = uuid::Uuid::new_v4().to_string();
            let new_cell = if cell_type == "markdown" {
                serde_json::json!({
                    "cell_type": "markdown",
                    "id": new_id,
                    "source": new_source,
                    "metadata": {}
                })
            } else {
                serde_json::json!({
                    "cell_type": "code",
                    "id": new_id,
                    "source": new_source,
                    "metadata": {},
                    "execution_count": serde_json::Value::Null,
                    "outputs": []
                })
            };

            let idx = cell_index.min(cells.len());
            cells.insert(idx, new_cell);
            write_notebook(&path, &notebook)?;
            return Ok(ToolResult::ok_text(format!("Inserted cell {new_id}")));
        }
        _ => {
            // replace
            if cell_index >= cells.len() {
                return Ok(ToolResult::err_text("cell index out of bounds"));
            }

            let target = &mut cells[cell_index];
            let existing_type = target
                .get("cell_type")
                .and_then(|v| v.as_str())
                .unwrap_or("code")
                .to_string();

            // Update source.
            if let Some(obj) = target.as_object_mut() {
                obj.insert("source".to_string(), serde_json::Value::String(new_source));

                if existing_type == "code" {
                    obj.insert("execution_count".to_string(), serde_json::Value::Null);
                    obj.insert("outputs".to_string(), serde_json::Value::Array(Vec::new()));
                }

                if let Some(ct) = cell_type {
                    obj.insert(
                        "cell_type".to_string(),
                        serde_json::Value::String(ct.to_string()),
                    );
                }
            } else {
                return Ok(ToolResult::err_text("cell is not a JSON object"));
            }

            let id = cell_display_id(target, cell_index);
            write_notebook(&path, &notebook)?;
            return Ok(ToolResult::ok_text(format!(
                "Updated cell {id} (language: {language})"
            )));
        }
    }
}

fn write_notebook(path: &PathBuf, notebook: &serde_json::Value) -> anyhow::Result<()> {
    // Match TS behavior: indent=1 for .ipynb.
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    notebook.serialize(&mut ser)?;
    buf.push(b'\n');
    fs::write(path, &buf)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;
    Ok(())
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

    fn ctx_for(cwd: PathBuf) -> ToolUseContext {
        let store_dir = cwd.join(".claude-tools-test-results");
        ToolUseContext {
            cwd: cwd.clone(),
            allowed_roots: vec![cwd],
            permission_mode: PermissionMode::AcceptEdits,
            session: Arc::new(crate::SessionState::default()),
            result_store: Arc::new(crate::ToolResultStore::new(store_dir).expect("store")),
            agent: None,
            agent_depth: 0,
            max_agent_depth: 2,
        }
    }

    #[tokio::test]
    async fn notebook_edit_replaces_cell_source() {
        let cwd = temp_dir("notebook-edit");
        let mut ctx = ctx_for(cwd.clone());
        let tool = NotebookEditTool::default();

        let path = cwd.join("test.ipynb");
        let nb = serde_json::json!({
            "cells": [
                {
                    "cell_type": "code",
                    "id": "abc",
                    "source": "print(1)",
                    "metadata": {},
                    "execution_count": serde_json::Value::Null,
                    "outputs": [],
                }
            ],
            "metadata": { "language_info": { "name": "python" } },
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        std::fs::write(&path, serde_json::to_string(&nb).unwrap()).expect("write notebook");

        let input = serde_json::json!({
            "notebook_path": path.to_string_lossy().to_string(),
            "cell_id": "abc",
            "new_source": "print(2)",
            "edit_mode": "replace",
        });

        assert!(tool.check_permissions(&input, &ctx).await.is_allowed());
        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(!res.is_error);

        let updated = std::fs::read_to_string(&path).expect("read notebook");
        assert!(updated.contains("print(2)"));
    }

    #[tokio::test]
    async fn notebook_edit_insert_requires_cell_type() {
        let cwd = temp_dir("notebook-insert");
        let mut ctx = ctx_for(cwd.clone());
        let tool = NotebookEditTool::default();

        let path = cwd.join("test.ipynb");
        let nb = serde_json::json!({
            "cells": [],
            "metadata": { "language_info": { "name": "python" } },
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        std::fs::write(&path, serde_json::to_string(&nb).unwrap()).expect("write notebook");

        let input = serde_json::json!({
            "notebook_path": path.to_string_lossy().to_string(),
            "new_source": "print(1)",
            "edit_mode": "insert",
        });

        let res = tool.call(input, &mut ctx).await.expect("call");
        assert!(res.is_error);
        let out = res.content.as_str().unwrap_or_default();
        assert!(out.contains("cell_type"));
    }
}
