use crate::context::ContextSnapshot;

pub fn default_system_prompt() -> String {
    // Keep this intentionally short; the Rust rewrite focuses on the headless pipeline.
    [
        "You are Claude Code, a headless coding assistant.",
        "Be concise and correct. If you need more information, ask a focused question.",
    ]
    .join("\n")
}

pub struct SystemPromptParts<'a> {
    pub base: Option<&'a str>,
    pub append: Option<&'a str>,
    pub json_schema: Option<&'a str>,
    pub include_context: bool,
}

pub fn build_system_prompt(ctx: &ContextSnapshot, parts: SystemPromptParts<'_>) -> String {
    let mut out = String::new();

    let base_owned = default_system_prompt();
    let base = parts.base.unwrap_or(base_owned.as_str());
    out.push_str(base.trim());
    out.push('\n');

    if let Some(append) = parts.append {
        let append = append.trim();
        if !append.is_empty() {
            out.push('\n');
            out.push_str(append);
            out.push('\n');
        }
    }

    if parts.include_context {
        out.push_str("\n# Context\n\n");
        out.push_str(&format!("Current time (UTC): {}\n", ctx.now.to_rfc3339()));
        out.push_str(&format!("Working directory: {}\n", ctx.cwd.display()));

        if let Some(git) = &ctx.git {
            out.push_str(&format!("\nGit root: {}\n", git.root.display()));
            if let Some(branch) = &git.branch {
                out.push_str(&format!("Git branch: {branch}\n"));
            }
            if let Some(head) = &git.head {
                let oid = &head.oid;
                let short = oid.get(..8).unwrap_or(oid.as_str());
                match &head.summary {
                    Some(summary) => out.push_str(&format!("Git HEAD: {short} {summary}\n")),
                    None => out.push_str(&format!("Git HEAD: {short}\n")),
                }
            }

            let s = &git.status;
            if !(s.staged.is_empty()
                && s.modified.is_empty()
                && s.untracked.is_empty()
                && s.conflicted.is_empty())
            {
                out.push_str("\nGit status summary (truncated):\n");
                if !s.staged.is_empty() {
                    out.push_str(&format!("staged: {}\n", s.staged.join(", ")));
                }
                if !s.modified.is_empty() {
                    out.push_str(&format!("modified: {}\n", s.modified.join(", ")));
                }
                if !s.untracked.is_empty() {
                    out.push_str(&format!("untracked: {}\n", s.untracked.join(", ")));
                }
                if !s.conflicted.is_empty() {
                    out.push_str(&format!("conflicted: {}\n", s.conflicted.join(", ")));
                }
            }

            if !git.recent_commits.is_empty() {
                out.push_str("\nRecent commits:\n");
                for c in &git.recent_commits {
                    let oid = &c.oid;
                    let short = oid.get(..8).unwrap_or(oid.as_str());
                    match &c.summary {
                        Some(summary) => out.push_str(&format!("- {short} {summary}\n")),
                        None => out.push_str(&format!("- {short}\n")),
                    }
                }
            }
        }

        if !ctx.claude_md.is_empty() {
            for file in &ctx.claude_md {
                out.push_str(&format!("\nCLAUDE.md ({}):\n\n", file.path.display()));
                out.push_str(file.content.trim());
                out.push('\n');
            }
        }
    }

    if let Some(schema) = parts.json_schema {
        let schema = schema.trim();
        if !schema.is_empty() {
            out.push_str("\n# Structured Output\n\n");
            out.push_str(
                "When responding, return JSON that matches this JSON Schema. Do not wrap it in Markdown.\n\n",
            );
            out.push_str(schema);
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}
