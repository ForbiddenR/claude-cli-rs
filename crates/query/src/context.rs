use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub now: DateTime<Utc>,
    pub cwd: PathBuf,
    pub git: Option<GitContext>,
    pub claude_md: Vec<ClaudeMdFile>,
}

#[derive(Debug, Clone)]
pub struct ContextOpts {
    pub bare: bool,
    pub add_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct GitContext {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub head: Option<GitCommitSummary>,
    pub recent_commits: Vec<GitCommitSummary>,
    pub status: GitStatusSummary,
}

#[derive(Debug, Clone)]
pub struct GitCommitSummary {
    pub oid: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GitStatusSummary {
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    pub conflicted: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ClaudeMdFile {
    pub path: PathBuf,
    pub content: String,
}

pub fn gather_context(cwd: PathBuf, opts: ContextOpts) -> anyhow::Result<ContextSnapshot> {
    let now = Utc::now();

    let git = if opts.bare {
        None
    } else {
        gather_git_context(&cwd)?
    };

    let claude_md = gather_claude_md(&cwd, &opts, git.as_ref())?;

    Ok(ContextSnapshot {
        now,
        cwd,
        git,
        claude_md,
    })
}

fn gather_git_context(cwd: &Path) -> anyhow::Result<Option<GitContext>> {
    let Some(root) = git_stdout(cwd, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };
    let root = PathBuf::from(root);

    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .and_then(|s| if s == "HEAD" { None } else { Some(s) });

    let head = git_stdout(cwd, &["log", "-n", "1", "--pretty=format:%H%x00%s"])?.and_then(|s| {
        let mut it = s.split('\0');
        let oid = it.next()?.to_string();
        let summary = it.next().map(|v| v.to_string()).filter(|v| !v.is_empty());
        Some(GitCommitSummary { oid, summary })
    });

    let recent_commits = gather_recent_commits(cwd, 5)?;
    let status = gather_git_status(cwd, 50)?;

    Ok(Some(GitContext {
        root,
        branch,
        head,
        recent_commits,
        status,
    }))
}

fn gather_recent_commits(cwd: &Path, limit: usize) -> anyhow::Result<Vec<GitCommitSummary>> {
    let limit_s = limit.to_string();
    let Some(out) = git_stdout(cwd, &["log", "-n", limit_s.as_str(), "--oneline"])? else {
        return Ok(Vec::new());
    };
    let mut commits = Vec::new();
    for line in out.lines() {
        let Some((oid, rest)) = line.split_once(' ') else {
            continue;
        };
        commits.push(GitCommitSummary {
            oid: oid.to_string(),
            summary: Some(rest.trim().to_string()).filter(|s| !s.is_empty()),
        });
    }
    Ok(commits)
}

fn gather_git_status(cwd: &Path, limit: usize) -> anyhow::Result<GitStatusSummary> {
    let mut out = GitStatusSummary::default();
    let Some(status) = git_stdout(cwd, &["status", "--porcelain=v1"])? else {
        return Ok(out);
    };

    for line in status.lines().take(limit) {
        if line.len() < 4 {
            continue;
        }
        let x = line.as_bytes()[0] as char;
        let y = line.as_bytes()[1] as char;
        let path = line[3..].trim().to_string();

        // Conflicts are encoded via specific XY pairs (e.g. UU, AA, DD).
        if matches!(
            (x, y),
            ('U', 'U')
                | ('A', 'A')
                | ('D', 'D')
                | ('U', 'A')
                | ('A', 'U')
                | ('U', 'D')
                | ('D', 'U')
        ) {
            out.conflicted.push(path.clone());
        }
        if x == '?' && y == '?' {
            out.untracked.push(path);
            continue;
        }
        if x != ' ' {
            out.staged.push(path.clone());
        }
        if y != ' ' {
            out.modified.push(path);
        }
    }

    Ok(out)
}

fn git_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let output = match Command::new("git").arg("-C").arg(cwd).args(args).output() {
        Ok(out) => out,
        Err(_err) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    ))
}

fn gather_claude_md(
    cwd: &Path,
    opts: &ContextOpts,
    git: Option<&GitContext>,
) -> anyhow::Result<Vec<ClaudeMdFile>> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // In bare mode, CLAUDE.md is never auto-discovered. The user must opt-in by
    // providing explicit directories via --add-dir.
    if !opts.bare {
        roots.push(cwd.to_path_buf());
    }

    roots.extend(opts.add_dirs.iter().cloned());

    let stop_dir = git.map(|g| g.root.as_path());
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::new();

    for root in roots {
        if let Some(path) = find_upwards(&root, "CLAUDE.md", stop_dir) {
            if seen.insert(path.clone()) {
                if let Ok(content) = read_text_with_limit(&path, 50_000) {
                    out.push(ClaudeMdFile { path, content });
                }
            }
        }
    }

    Ok(out)
}

fn find_upwards(start: &Path, filename: &str, stop_dir: Option<&Path>) -> Option<PathBuf> {
    let mut cur = start;
    loop {
        let candidate = cur.join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }

        if let Some(stop) = stop_dir {
            if cur == stop {
                return None;
            }
        }

        let Some(parent) = cur.parent() else {
            return None;
        };
        cur = parent;
    }
}

fn read_text_with_limit(path: &Path, max_bytes: usize) -> anyhow::Result<String> {
    let bytes = fs::read(path)?;
    let bytes = if bytes.len() > max_bytes {
        &bytes[..max_bytes]
    } else {
        &bytes
    };
    Ok(String::from_utf8_lossy(bytes).to_string())
}
