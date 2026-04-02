use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ToolResultStore {
    base_dir: PathBuf,
}

impl ToolResultStore {
    pub fn new(base_dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn store_text(&self, tool_name: &str, text: &str) -> anyhow::Result<PathBuf> {
        let filename = format!("{}-{}.txt", sanitize(tool_name), Uuid::new_v4());
        let path = self.base_dir.join(filename);
        fs::write(&path, text.as_bytes())?;
        Ok(path)
    }

    pub fn store_json(
        &self,
        tool_name: &str,
        value: &serde_json::Value,
    ) -> anyhow::Result<PathBuf> {
        let filename = format!("{}-{}.json", sanitize(tool_name), Uuid::new_v4());
        let path = self.base_dir.join(filename);
        let bytes = serde_json::to_vec_pretty(value)?;
        fs::write(&path, bytes)?;
        Ok(path)
    }
}

fn sanitize(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_ascii_whitespace() {
            out.push('_');
        }
    }
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}
