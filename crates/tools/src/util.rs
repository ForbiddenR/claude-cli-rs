use std::path::{Component, Path, PathBuf};

use crate::ToolUseContext;

pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(input));
    }

    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }

    PathBuf::from(input)
}

pub fn absolutize(cwd: &Path, input: &Path) -> PathBuf {
    if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    }
}

/// Best-effort lexical normalization (no filesystem access).
///
/// This is not a full canonicalization, but it removes `.` and resolves `..`.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();

    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only pop if we have a normal component; keep root/prefix intact.
                let popped = out.pop();
                if !popped {
                    out.push(c);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }

    out
}

pub fn is_path_allowed(ctx: &ToolUseContext, target: &Path) -> bool {
    if ctx.is_bypass_permissions() {
        return true;
    }

    ctx.allowed_roots
        .iter()
        .any(|root| target.starts_with(root))
}

pub fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_string(), false);
    }

    let mut out = String::new();
    out.extend(s.chars().take(max_chars));
    (out, true)
}

pub fn format_cat_n(lines: &[(usize, String)]) -> String {
    let mut out = String::new();
    for (n, line) in lines {
        // `cat -n` uses a 6-width right-aligned number and a tab.
        out.push_str(&format!("{:>6}\t{}\n", n, line));
    }
    out
}
