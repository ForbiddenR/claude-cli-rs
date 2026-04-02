use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;

use async_trait::async_trait;

use crate::util::{absolutize, expand_tilde, format_cat_n, is_path_allowed, normalize_path};
use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "Read";
const DEFAULT_LIMIT_LINES: usize = 2000;

#[derive(Debug, Default, Clone)]
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
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
              "description": "The absolute path to the file to read"
            },
            "offset": {
              "type": "integer",
              "minimum": 0,
              "description": "The line number to start reading from (1-based)"
            },
            "limit": {
              "type": "integer",
              "minimum": 1,
              "description": "The number of lines to read (default 2000)"
            },
            "pages": {
              "type": "string",
              "description": "Page range for PDF files (not implemented in Rust rewrite)",
              "nullable": true
            }
          },
          "required": ["file_path"]
        })
    }

    fn prompt(&self) -> String {
        "Read a file from the local filesystem. Results are returned in cat -n format (1-based line numbers). Use offset/limit for large files.".to_string()
    }

    async fn check_permissions(
        &self,
        input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        let Some(raw) = input.get("file_path").and_then(|v| v.as_str()) else {
            return PermissionResult::deny("missing required field: file_path");
        };
        let path = resolve_path(ctx, raw);
        if is_path_allowed(ctx, &path) {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(format!(
                "Read is not allowed for path outside the working directory. Path: {}",
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
        if file_path.trim().is_empty() {
            return Ok(ToolResult::err_text("missing required field: file_path"));
        }

        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;

        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT_LINES)
            .min(DEFAULT_LIMIT_LINES);

        let path = resolve_path(ctx, file_path);

        let out =
            tokio::task::spawn_blocking(move || read_file_range(path, offset, limit)).await??;
        Ok(ToolResult::ok_text(out))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // Read output can be large; still keep a hard cap.
        120_000
    }
}

fn resolve_path(ctx: &ToolUseContext, raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw.trim());
    let abs = absolutize(&ctx.cwd, &expanded);
    normalize_path(&abs)
}

fn read_file_range(path: PathBuf, offset: usize, limit: usize) -> anyhow::Result<String> {
    let mut f = File::open(&path)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
    let meta = f
        .metadata()
        .map_err(|e| anyhow::anyhow!("cannot stat {}: {e}", path.display()))?;
    if meta.is_dir() {
        anyhow::bail!("path is a directory: {}", path.display());
    }

    let encoding = detect_encoding(&mut f)?;

    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut truncated = false;

    match encoding {
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let bytes = fs::read(&path)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
            let decoded = decode_utf16(&bytes, encoding == TextEncoding::Utf16Le)?;

            // Note: String::lines treats \r\n as a single line ending.
            let mut line_no: usize = 0;
            let mut collected: usize = 0;
            for line in decoded.lines() {
                line_no += 1;
                if line_no < offset {
                    continue;
                }
                if collected >= limit {
                    truncated = true;
                    break;
                }
                lines.push((line_no, line.to_string()));
                collected += 1;
            }
        }
        TextEncoding::Utf8 { skip_bom } => {
            f.seek(SeekFrom::Start(0)).map_err(|e| {
                anyhow::anyhow!("cannot seek to start of {}: {e}", path.display())
            })?;
            let mut reader = BufReader::new(f);

            let mut line_no: usize = 0;
            let mut collected: usize = 0;
            let mut buf: Vec<u8> = Vec::new();

            loop {
                buf.clear();
                let n = reader
                    .read_until(b'\n', &mut buf)
                    .map_err(|e| anyhow::anyhow!("failed reading {}: {e}", path.display()))?;
                if n == 0 {
                    break;
                }

                line_no += 1;
                if line_no < offset {
                    continue;
                }
                if collected >= limit {
                    truncated = true;
                    break;
                }

                // Trim only trailing newline; keep other whitespace.
                if buf.ends_with(b"\n") {
                    buf.pop();
                    if buf.ends_with(b"\r") {
                        buf.pop();
                    }
                }

                if skip_bom && line_no == 1 && buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    buf.drain(..3);
                }

                let line = String::from_utf8_lossy(&buf).to_string();
                lines.push((line_no, line));
                collected += 1;
            }
        }
    }

    if lines.is_empty() {
        return Ok(format!("(no content) {}", path.display()));
    }

    let mut out = format_cat_n(&lines);
    if truncated {
        out.push_str("\n(Results are truncated. Use offset/limit to read more.)\n");
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEncoding {
    Utf8 { skip_bom: bool },
    Utf16Le,
    Utf16Be,
}

fn detect_encoding(f: &mut File) -> anyhow::Result<TextEncoding> {
    let mut head = [0u8; 4];
    let n = f
        .read(&mut head)
        .map_err(|e| anyhow::anyhow!("failed reading BOM bytes: {e}"))?;
    f.seek(SeekFrom::Start(0))
        .map_err(|e| anyhow::anyhow!("failed seeking to start: {e}"))?;

    let head = &head[..n];

    if head.starts_with(&[0xFF, 0xFE]) {
        return Ok(TextEncoding::Utf16Le);
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        return Ok(TextEncoding::Utf16Be);
    }
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(TextEncoding::Utf8 { skip_bom: true });
    }

    Ok(TextEncoding::Utf8 { skip_bom: false })
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> anyhow::Result<String> {
    // Skip UTF-16 BOM if present.
    let bytes = if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        &bytes[2..]
    } else {
        bytes
    };

    if bytes.len() < 2 {
        return Ok(String::new());
    }

    let mut code_units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let u = if little_endian {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        };
        code_units.push(u);
    }

    let s: String = std::char::decode_utf16(code_units.into_iter())
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect();
    Ok(s)
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
    fn read_utf16le_file_decodes_to_text() {
        let path = temp_path("utf16le");
        // UTF-16LE with BOM: "hi\nthere\n"
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
        for ch in "hi\nthere\n".encode_utf16() {
            bytes.extend_from_slice(&ch.to_le_bytes());
        }
        fs::write(&path, &bytes).expect("write utf16 file");

        let out = read_file_range(path.clone(), 1, 10).expect("read should succeed");
        assert!(out.contains("\thi"));
        assert!(out.contains("\tthere"));

        let _ = fs::remove_file(&path);
    }
}
