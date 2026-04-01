use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use claude_core::types::permissions::PermissionMode;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum InputFormat {
    Text,
    StreamJson,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ThinkingMode {
    Enabled,
    Adaptive,
    Disabled,
}

#[derive(Debug, Parser)]
#[command(name = "claude-rs", about = "Claude Code (headless) rewritten in Rust")]
pub struct Args {
    /// Prompt to send. If omitted, stdin is read.
    pub prompt: Option<String>,

    /// Print response and exit (required; interactive mode is not implemented).
    #[arg(short = 'p', long = "print", default_value_t = false)]
    pub print: bool,

    /// Minimal mode: avoid interactive-only features.
    #[arg(long = "bare", default_value_t = false)]
    pub bare: bool,

    /// Output format (only works with --print).
    #[arg(long = "output-format", value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// JSON Schema for structured output (only works with --print).
    #[arg(long = "json-schema")]
    pub json_schema: Option<String>,

    /// Input format (only works with --print).
    #[arg(long = "input-format", value_enum, default_value_t = InputFormat::Text)]
    pub input_format: InputFormat,

    /// Re-emit user messages from stdin back on stdout (SDK mode).
    #[arg(long = "replay-user-messages", default_value_t = false)]
    pub replay_user_messages: bool,

    /// Include all hook lifecycle events in the output stream (stream-json).
    #[arg(long = "include-hook-events", default_value_t = false)]
    pub include_hook_events: bool,

    /// Include partial message chunks as they arrive (stream-json).
    #[arg(long = "include-partial-messages", default_value_t = false)]
    pub include_partial_messages: bool,

    /// Model override.
    #[arg(long = "model")]
    pub model: Option<String>,

    /// API key override (otherwise uses ANTHROPIC_API_KEY, settings api_key_helper, or global config).
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Permission mode override.
    #[arg(long = "permission-mode", value_enum)]
    pub permission_mode: Option<PermissionMode>,

    /// Comma/space-separated list of tool names to allow.
    #[arg(long = "allowed-tools", alias = "allowedTools")]
    pub allowed_tools: Vec<String>,

    /// Comma/space-separated list of tool names to deny.
    #[arg(long = "disallowed-tools", alias = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Specify which built-in tools are available.
    #[arg(long = "tools")]
    pub tools: Vec<String>,

    /// Load MCP servers from JSON files or inline JSON (repeatable).
    #[arg(long = "mcp-config")]
    pub mcp_config: Vec<String>,

    /// Only use MCP servers from --mcp-config.
    #[arg(long = "strict-mcp-config", default_value_t = false)]
    pub strict_mcp_config: bool,

    /// Path to a settings JSON file or an inline JSON string.
    #[arg(long = "settings")]
    pub settings: Option<String>,

    /// Override current working directory for the session.
    #[arg(long = "cwd")]
    pub cwd: Option<PathBuf>,

    /// Enable git worktree mode (stub; retained for flag parity).
    #[arg(long = "worktree", default_value_t = false)]
    pub worktree: bool,

    /// Additional directories to allow tool access to.
    #[arg(long = "add-dir")]
    pub add_dir: Vec<PathBuf>,

    /// Continue the most recent conversation in the current directory.
    #[arg(short = 'c', long = "continue", default_value_t = false)]
    pub continue_session: bool,

    /// Resume a conversation by session ID.
    #[arg(short = 'r', long = "resume")]
    pub resume: Option<String>,

    /// Maximum number of agentic turns in print mode.
    #[arg(long = "max-turns")]
    pub max_turns: Option<u32>,

    /// Maximum tokens to sample per API call in print mode.
    #[arg(long = "max-tokens")]
    pub max_tokens: Option<u32>,

    /// Maximum dollar amount to spend on API calls in print mode.
    #[arg(long = "max-budget-usd")]
    pub max_budget_usd: Option<f64>,

    /// Thinking mode (internal; included for flag parity).
    #[arg(long = "thinking", value_enum)]
    pub thinking: Option<ThinkingMode>,

    /// Maximum number of thinking tokens (deprecated).
    #[arg(long = "max-thinking-tokens")]
    pub max_thinking_tokens: Option<u32>,

    /// System prompt to use for the session.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Read system prompt from a file.
    #[arg(long = "system-prompt-file")]
    pub system_prompt_file: Option<PathBuf>,

    /// Append a system prompt to the default system prompt.
    #[arg(long = "append-system-prompt")]
    pub append_system_prompt: Option<String>,

    /// Read system prompt from a file and append to the default system prompt.
    #[arg(long = "append-system-prompt-file")]
    pub append_system_prompt_file: Option<PathBuf>,

    /// Enable debug logging (optional filter string).
    #[arg(short = 'd', long = "debug", num_args = 0..=1)]
    pub debug: Option<Option<String>>,

    /// Enable debug logging and write to stderr.
    #[arg(long = "debug-to-stderr", default_value_t = false)]
    pub debug_to_stderr: bool,

    /// Write debug logs to a specific file (implicitly enables debug mode).
    #[arg(long = "debug-file")]
    pub debug_file: Option<PathBuf>,

    /// Override verbose mode setting from config.
    #[arg(long = "verbose", default_value_t = false)]
    pub verbose: bool,

    /// Disable all slash commands (skills).
    #[arg(long = "disable-slash-commands", default_value_t = false)]
    pub disable_slash_commands: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// OAuth login (manual PKCE flow).
    Auth,
    /// Diagnostics (stub).
    Doctor,
    /// MCP config helpers (stub).
    Mcp,
}
