#[derive(Debug, Clone, Copy)]
pub struct SlashCommandDef {
    pub name: &'static str,
    pub description: &'static str,
    pub usage: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommandDef] = &[
    SlashCommandDef {
        name: "help",
        description: "Show help for TUI commands and keybindings",
        usage: "/help",
    },
    SlashCommandDef {
        name: "model",
        description: "Show or set the current model",
        usage: "/model [model-id]",
    },
    SlashCommandDef {
        name: "clear",
        description: "Start a new empty session",
        usage: "/clear",
    },
    SlashCommandDef {
        name: "compact",
        description: "Summarize and compact history into a new session",
        usage: "/compact",
    },
    SlashCommandDef {
        name: "cost",
        description: "Show token/cost totals for this TUI run",
        usage: "/cost",
    },
    SlashCommandDef {
        name: "exit",
        description: "Exit the TUI",
        usage: "/exit",
    },
];

pub fn match_commands(prefix: &str) -> Vec<&'static SlashCommandDef> {
    let p = prefix.trim().to_ascii_lowercase();
    let mut out: Vec<&'static SlashCommandDef> = SLASH_COMMANDS
        .iter()
        .filter(|c| p.is_empty() || c.name.starts_with(&p))
        .collect();
    out.sort_by(|a, b| a.name.cmp(b.name));
    out
}

pub struct ParsedSlashCommand {
    pub name: String,
    pub args: Vec<String>,
}

pub fn parse_slash_command(input: &str) -> Option<ParsedSlashCommand> {
    let s = input.trim();
    let rest = s.strip_prefix('/')?;

    // `/` by itself isn't a command.
    if rest.trim().is_empty() {
        return None;
    }

    // `/?` alias.
    if rest.trim() == "?" {
        return Some(ParsedSlashCommand {
            name: "help".to_string(),
            args: Vec::new(),
        });
    }

    let mut it = rest.split_whitespace();
    let name = it.next()?.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let args = it.map(|s| s.to_string()).collect::<Vec<_>>();
    Some(ParsedSlashCommand { name, args })
}
