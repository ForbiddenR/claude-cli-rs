use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::OpenOptions,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{Result, types::ids::SessionId, types::message::Message};

const PROJECTS_DIR: &str = "projects";
const MAX_SANITIZED_LENGTH: usize = 200;

const MAX_INLINE_PASTE_CHARS: usize = 1024;
const PASTE_PREFIX: &str = "[[PASTE:";
const PASTE_SUFFIX: &str = "]]";

pub fn project_root_for_cwd(cwd: &Path) -> PathBuf {
    git_toplevel(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

pub fn project_dir_for_cwd(cwd: &Path) -> Result<PathBuf> {
    let root = project_root_for_cwd(cwd);
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let root_s = root.to_string_lossy();

    let dir = crate::paths::claude_config_home_dir()?
        .join(PROJECTS_DIR)
        .join(sanitize_path_component(&root_s));

    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn session_file_path(cwd: &Path, session_id: SessionId) -> Result<PathBuf> {
    Ok(project_dir_for_cwd(cwd)?.join(format!("{session_id}.jsonl")))
}

pub fn find_latest_session(cwd: &Path) -> Result<Option<(SessionId, PathBuf)>> {
    let project_dir = project_dir_for_cwd(cwd)?;

    let mut best: Option<(SessionId, PathBuf, std::time::SystemTime)> = None;

    let entries = match std::fs::read_dir(&project_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    for ent in entries {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = ent.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let Ok(id) = stem.parse::<SessionId>() else {
            continue;
        };

        let modified = match ent.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        match &best {
            Some((_best_id, _best_path, best_time)) if *best_time >= modified => {}
            _ => best = Some((id, path, modified)),
        }
    }

    Ok(best.map(|(id, path, _)| (id, path)))
}

pub fn load_session_messages(path: &Path) -> Result<Vec<Message>> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    let mut out: Vec<Message> = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut msg: Message = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        expand_paste_refs_in_message(&mut msg, &mut cache);
        out.push(msg);
    }

    Ok(out)
}

pub fn append_session_messages(path: &Path, messages: &[Message]) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Ensure file exists before locking.
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    let lock_path = lock_path_for(path);
    let _lock = crate::lockfile::acquire_lock(&lock_path, Duration::from_secs(5))?;

    let mut f = OpenOptions::new().create(true).append(true).open(path)?;

    for msg in messages {
        let mut stored = msg.clone();
        externalize_large_text_blocks(&mut stored);
        let line = serde_json::to_string(&stored)?;
        writeln!(f, "{line}")?;
    }

    Ok(())
}

pub fn sanitize_path_component(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
        } else {
            sanitized.push('-');
        }
    }

    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }

    let hash = crate::paste_store::hash_pasted_text(name);
    sanitized.truncate(MAX_SANITIZED_LENGTH);
    format!("{sanitized}-{hash}")
}

fn lock_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("session.jsonl"))
        .to_string_lossy()
        .to_string();
    path.with_file_name(format!("{file_name}.lock"))
}

fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

fn externalize_large_text_blocks(msg: &mut Message) {
    // Avoid blocking failures on paste store writes; best-effort only.
    match msg {
        Message::User(m) => {
            for block in &mut m.content {
                if let crate::types::message::ContentBlock::Text { text } = block {
                    if text.chars().count() > MAX_INLINE_PASTE_CHARS {
                        let hash = crate::paste_store::hash_pasted_text(text);
                        let _ = crate::paste_store::store_pasted_text(&hash, text);
                        *text = format!("{PASTE_PREFIX}{hash}{PASTE_SUFFIX}");
                    }
                }
            }
        }
        Message::Assistant(m) => {
            for block in &mut m.content {
                if let crate::types::message::ContentBlock::Text { text } = block {
                    if text.chars().count() > MAX_INLINE_PASTE_CHARS {
                        let hash = crate::paste_store::hash_pasted_text(text);
                        let _ = crate::paste_store::store_pasted_text(&hash, text);
                        *text = format!("{PASTE_PREFIX}{hash}{PASTE_SUFFIX}");
                    }
                }
            }
        }
    }
}

fn expand_paste_refs_in_message(msg: &mut Message, cache: &mut HashMap<String, Option<String>>) {
    match msg {
        Message::User(m) => {
            for block in &mut m.content {
                if let crate::types::message::ContentBlock::Text { text } = block {
                    *text = expand_paste_refs_in_text(text, cache);
                }
            }
        }
        Message::Assistant(m) => {
            for block in &mut m.content {
                if let crate::types::message::ContentBlock::Text { text } = block {
                    *text = expand_paste_refs_in_text(text, cache);
                }
            }
        }
    }
}

fn expand_paste_refs_in_text(text: &str, cache: &mut HashMap<String, Option<String>>) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some(start) = rest.find(PASTE_PREFIX) {
        out.push_str(&rest[..start]);

        let after_prefix = &rest[start + PASTE_PREFIX.len()..];
        let Some(end) = after_prefix.find(PASTE_SUFFIX) else {
            // Malformed ref; keep as-is.
            out.push_str(&rest[start..]);
            return out;
        };

        let hash = &after_prefix[..end];
        let content = if let Some(v) = cache.get(hash) {
            v.clone()
        } else {
            let loaded = crate::paste_store::retrieve_pasted_text(hash)
                .ok()
                .flatten();
            cache.insert(hash.to_string(), loaded.clone());
            loaded
        };

        match content {
            Some(s) => out.push_str(&s),
            None => {
                out.push_str(PASTE_PREFIX);
                out.push_str(hash);
                out.push_str(PASTE_SUFFIX);
            }
        }

        rest = &after_prefix[end + PASTE_SUFFIX.len()..];
    }

    out.push_str(rest);
    out
}

