use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use crate::{CoreError, Result};

const PASTE_STORE_DIR: &str = "paste-cache";

pub fn hash_pasted_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(digest.len().saturating_mul(2));
    for b in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }

    hex.truncate(16);
    hex
}

pub fn paste_store_dir() -> Result<PathBuf> {
    Ok(crate::paths::claude_config_home_dir()?.join(PASTE_STORE_DIR))
}

pub fn store_pasted_text(hash: &str, content: &str) -> Result<PathBuf> {
    let dir = paste_store_dir()?;
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{hash}.txt"));
    std::fs::write(&path, content.as_bytes())?;
    Ok(path)
}

pub fn retrieve_pasted_text(hash: &str) -> Result<Option<String>> {
    let path = paste_store_dir()?.join(format!("{hash}.txt"));
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CoreError::Io { source }),
    };
    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
}
