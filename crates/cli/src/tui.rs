use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use claude_core::config::settings::EditorMode;
use claude_core::types::ids::SessionId;
use claude_core::types::message::{ContentBlock, Message, UserMessage};
use claude_core::types::permissions::PermissionMode;
use claude_services::auth::AuthMode;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, terminal};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Terminal;
use similar::TextDiff;
use tokio::sync::{mpsc, oneshot};

use crate::args::{Args, InputFormat, OutputFormat};

mod input;
mod keymap;
mod markdown;
mod slash;
mod vim;

use input::{InputBuffer, PromptHistory, ReverseHistorySearch};
use keymap::{KeyAction, KeyContext, KeySequence, KeybindingResolver, ResolveOutcome};
use markdown::{MarkdownRenderer, StreamingMarkdown};
use slash::{match_commands, parse_slash_command, SlashCommandDef, SLASH_COMMANDS};
use vim::{VimHandleResult, VimKey, VimMachine, VimMode};

const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
const MAX_INPUT_VISIBLE_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone)]
struct ChatEntry {
    role: Role,
    text: String,
}

struct RenderedEntry {
    header: Line<'static>,
    body: RenderedBody,
    dirty: bool,
}

enum RenderedBody {
    Static(Vec<Line<'static>>),
    Streaming(StreamingMarkdown),
}

struct App {
    input: InputBuffer,
    prompt_history: PromptHistory,
    reverse_history_search: Option<ReverseHistorySearch>,
    vim: Option<VimMachine>,
    keymap: KeybindingResolver,
    typeahead_query: Option<String>,
    typeahead_selected: usize,
    typeahead_suppressed_text: Option<String>,
    status: String,
    spinner_idx: usize,
    in_flight: bool,
    active_assistant_idx: Option<usize>,

    transcript: Vec<ChatEntry>,
    rendered: Vec<RenderedEntry>,
    render_width: usize,
    md: MarkdownRenderer,
    history: Vec<Message>,

    // Week 4: tools + permissions
    tool_entry_for_id: HashMap<String, usize>,
    permission_prompt: Option<PermissionPrompt>,
    /// Lowercased tool names that are auto-approved in Default permission mode.
    always_allow_tools: Arc<Mutex<HashSet<String>>>,

    // Week 3: scrollback + virtualization
    /// Start line (0-based) of the transcript viewport.
    scroll_top: usize,
    /// When true, keep the viewport pinned to the bottom as new content arrives.
    scroll_follow: bool,
    /// Last rendered height of the messages viewport (used for page scrolling).
    last_msg_view_height: usize,
    /// Prefix-sum of entry start lines. `line_offsets[i]` is the first line of entry `i`,
    /// and the final element is the total line count.
    line_offsets: Vec<usize>,

    session_id: SessionId,
    session_path: PathBuf,
    model: String,
    cwd: PathBuf,
    user_settings_path: PathBuf,

    // Week 8: slash commands need access to the engine so commands can change model or compact.
    client: claude_services::api::AnthropicClient,
    auth: AuthMode,
    engine_inputs: EngineInputs,
    engine: std::sync::Arc<claude_query::QueryEngine>,

    // Week 8: running totals for `/cost`.
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cost_usd: Option<f64>,
    last_turn_input_tokens: u64,
    last_turn_output_tokens: u64,
    last_turn_cost_usd: Option<f64>,
}

#[derive(Clone)]
struct EngineInputs {
    max_tokens: u32,
    cfg: claude_query::QueryEngineConfig,
}

#[derive(Debug)]
enum QueryEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolUseResult {
        id: String,
        name: String,
        input: serde_json::Value,
        result: serde_json::Value,
        is_error: bool,
    },
    PermissionRequest {
        id: String,
        name: String,
        input: serde_json::Value,
        reply_tx: oneshot::Sender<claude_query::PermissionDecision>,
    },
    Finished(claude_query::RunResult),
    CompactionFinished {
        session_id: SessionId,
        session_path: PathBuf,
        history: Vec<Message>,
    },
    CompactionError(String),
    Error(String),
}

struct PermissionPrompt {
    id: String,
    tool_name: String,
    details: String,
    reply_tx: oneshot::Sender<claude_query::PermissionDecision>,
}

#[derive(Clone)]
struct TuiObserver {
    tx: mpsc::UnboundedSender<QueryEvent>,
    always_allow_tools: Arc<Mutex<HashSet<String>>>,
}

#[async_trait]
impl claude_query::QueryObserver for TuiObserver {
    async fn on_tool_use_start(&self, id: &str, name: &str, input: &serde_json::Value) {
        let _ = self.tx.send(QueryEvent::ToolUseStart {
            id: id.to_string(),
            name: name.to_string(),
            input: input.clone(),
        });
    }

    async fn on_tool_use_result(
        &self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        result: &serde_json::Value,
        is_error: bool,
    ) {
        let _ = self.tx.send(QueryEvent::ToolUseResult {
            id: id.to_string(),
            name: name.to_string(),
            input: input.clone(),
            result: result.clone(),
            is_error,
        });
    }

    async fn request_permission(
        &self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> claude_query::PermissionDecision {
        let key = name.trim().to_ascii_lowercase();
        if let Ok(set) = self.always_allow_tools.lock() {
            if set.contains(&key) {
                return claude_query::PermissionDecision::AlwaysAllowTool;
            }
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(QueryEvent::PermissionRequest {
            id: id.to_string(),
            name: name.to_string(),
            input: input.clone(),
            reply_tx,
        });

        match reply_rx.await {
            Ok(d) => d,
            Err(_) => claude_query::PermissionDecision::Deny,
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        terminal::enable_raw_mode().context("enable raw mode")?;
        execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
            .context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}

impl RenderedEntry {
    fn new(role: Role) -> Self {
        Self {
            header: role_header(role),
            body: RenderedBody::Static(Vec::new()),
            dirty: true,
        }
    }

    fn new_streaming(role: Role) -> Self {
        Self {
            header: role_header(role),
            body: RenderedBody::Streaming(StreamingMarkdown::new()),
            dirty: true,
        }
    }
}

impl RenderedBody {
    fn line_count(&self) -> usize {
        match self {
            Self::Static(lines) => lines.len(),
            Self::Streaming(stream) => stream.line_count(),
        }
    }
}

impl App {
    fn ensure_rendered(&mut self, width: usize) {
        let width = width.max(1);

        if self.render_width != width {
            self.render_width = width;
            for entry in &mut self.rendered {
                entry.dirty = true;
                if let RenderedBody::Streaming(stream) = &mut entry.body {
                    stream.reset();
                }
            }
        }

        // Defensive: keep caches aligned even if a future edit forgets to push/pop both.
        while self.rendered.len() < self.transcript.len() {
            let role = self.transcript[self.rendered.len()].role;
            self.rendered.push(RenderedEntry::new(role));
        }
        if self.rendered.len() > self.transcript.len() {
            self.rendered.truncate(self.transcript.len());
        }

        for (idx, cache) in self.rendered.iter_mut().enumerate() {
            if !cache.dirty {
                continue;
            }
            let Some(entry) = self.transcript.get(idx) else {
                continue;
            };
            match &mut cache.body {
                RenderedBody::Static(lines) => {
                    *lines = self.md.render(&entry.text, width);
                }
                RenderedBody::Streaming(stream) => {
                    stream.update(&entry.text, &self.md, width);
                }
            }
            cache.dirty = false;
        }
    }

    fn recompute_line_offsets(&mut self) {
        self.line_offsets.clear();
        self.line_offsets
            .reserve(self.rendered.len().saturating_add(1));

        let mut acc: usize = 0;
        for entry in &self.rendered {
            self.line_offsets.push(acc);
            // header + body + separator blank line
            acc = acc.saturating_add(1);
            acc = acc.saturating_add(entry.body.line_count());
            acc = acc.saturating_add(1);
        }
        self.line_offsets.push(acc);
    }

    fn total_rendered_lines(&self) -> usize {
        self.line_offsets.last().copied().unwrap_or(0)
    }

    fn finalize_streaming(&mut self, idx: usize) {
        let width = self.render_width.max(1);

        let text = match self.transcript.get(idx) {
            Some(e) => e.text.clone(),
            None => return,
        };

        let Some(cache) = self.rendered.get_mut(idx) else {
            return;
        };

        let body = std::mem::replace(&mut cache.body, RenderedBody::Static(Vec::new()));
        match body {
            RenderedBody::Streaming(stream) => {
                let lines = stream.into_static(&text, &self.md, width);
                cache.body = RenderedBody::Static(lines);
            }
            other => {
                cache.body = other;
            }
        }
        cache.dirty = false;
    }

    fn current_key_context(&self) -> KeyContext {
        if self.typeahead_active() {
            return KeyContext::CommandTypeahead;
        }
        if self.vim.as_ref().is_some_and(|vim| vim.is_normal()) {
            return KeyContext::VimNormal;
        }
        if self.vim.is_some() {
            return KeyContext::VimInsert;
        }
        KeyContext::Input
    }

    fn sync_typeahead(&mut self) {
        if self.typeahead_suppressed_text.as_deref() != Some(self.input.as_str()) {
            self.typeahead_suppressed_text = None;
        }
        if self.typeahead_suppressed_text.is_some() {
            self.typeahead_query = None;
            self.typeahead_selected = 0;
            return;
        }

        let Some(query) = slash_query(&self.input) else {
            self.typeahead_query = None;
            self.typeahead_selected = 0;
            return;
        };

        let suggestions = match_commands(&query);
        if suggestions.is_empty() {
            self.typeahead_query = None;
            self.typeahead_selected = 0;
            return;
        }

        let query_changed = self.typeahead_query.as_deref() != Some(query.as_str());
        if query_changed {
            self.typeahead_selected = 0;
        } else {
            self.typeahead_selected = self.typeahead_selected.min(suggestions.len().saturating_sub(1));
        }
        self.typeahead_query = Some(query);
    }

    fn typeahead_active(&self) -> bool {
        self.typeahead_query.is_some()
    }

    fn typeahead_items(&self) -> Vec<&'static SlashCommandDef> {
        match self.typeahead_query.as_deref() {
            Some(query) => match_commands(query),
            None => Vec::new(),
        }
    }

    fn move_typeahead(&mut self, delta: isize) {
        let suggestions = self.typeahead_items();
        if suggestions.is_empty() {
            self.typeahead_selected = 0;
            return;
        }
        let len = suggestions.len() as isize;
        let cur = self.typeahead_selected.min(suggestions.len().saturating_sub(1)) as isize;
        let next = (cur + delta).rem_euclid(len) as usize;
        self.typeahead_selected = next;
    }

    fn accept_typeahead(&mut self) -> bool {
        let suggestions = self.typeahead_items();
        let Some(selected) = suggestions.get(self.typeahead_selected.min(suggestions.len().saturating_sub(1))) else {
            return false;
        };

        let current = self.input.as_str().trim();
        let suffix = current
            .strip_prefix('/')
            .map(|rest| rest.split_once(char::is_whitespace).map(|(_, tail)| tail).unwrap_or(""))
            .unwrap_or("");

        let new_text = if suffix.trim().is_empty() {
            format!("/{} ", selected.name)
        } else {
            format!("/{} {}", selected.name, suffix.trim_start())
        };
        self.input.set_text(new_text);
        self.sync_typeahead();
        true
    }

    fn dismiss_typeahead(&mut self) {
        self.typeahead_suppressed_text = Some(self.input.as_str().to_string());
        self.typeahead_query = None;
        self.typeahead_selected = 0;
    }

    fn push_system_message(&mut self, text: impl Into<String>) {
        self.transcript.push(ChatEntry {
            role: Role::System,
            text: text.into(),
        });
        self.rendered.push(RenderedEntry::new(Role::System));
        scroll_to_bottom(self);
    }

    fn clear_chat(&mut self) -> anyhow::Result<()> {
        let new_id = SessionId::new();
        let new_path = claude_core::history::session_file_path(&self.cwd, new_id)?;
        self.session_id = new_id;
        self.session_path = new_path;
        self.history.clear();
        self.input.clear();
        self.transcript.clear();
        self.rendered.clear();
        self.tool_entry_for_id.clear();
        self.permission_prompt = None;
        self.active_assistant_idx = None;
        self.dismiss_typeahead();
        self.scroll_top = 0;
        self.scroll_follow = true;
        self.push_system_message("Started a new session. Previous transcript remains on disk.");
        self.status = "session cleared".to_string();
        Ok(())
    }

    fn rebuild_engine(&mut self) -> anyhow::Result<()> {
        self.engine = std::sync::Arc::new(claude_query::QueryEngine::new(
            self.client.clone(),
            self.auth.clone(),
            self.model.clone(),
            self.engine_inputs.max_tokens,
            self.engine_inputs.cfg.clone(),
        )?);
        Ok(())
    }

    fn record_run_cost(&mut self, result: &claude_query::RunResult) {
        self.last_turn_input_tokens = result.usage.input_tokens;
        self.last_turn_output_tokens = result.usage.output_tokens;
        self.last_turn_cost_usd = result.cost_usd;
        self.total_input_tokens = self
            .total_input_tokens
            .saturating_add(result.usage.input_tokens as u64);
        self.total_output_tokens = self
            .total_output_tokens
            .saturating_add(result.usage.output_tokens as u64);
        self.total_cost_usd = match (self.total_cost_usd, result.cost_usd) {
            (Some(acc), Some(cost)) => Some(acc + cost),
            (None, _) | (_, None) => None,
        };
    }
}

fn slash_query(input: &InputBuffer) -> Option<String> {
    let raw = input.as_str();
    if raw.contains('\n') {
        return None;
    }
    let trimmed = raw.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return None;
    };

    // Only show typeahead while editing the command name (before any whitespace).
    if rest.contains(char::is_whitespace) {
        return None;
    }

    Some(rest.to_ascii_lowercase())
}

fn slash_candidate_name(app: &App) -> Option<String> {
    let input = app.input.as_str().trim();
    let rest = input.strip_prefix('/')?;
    let typed = rest.split_whitespace().next().unwrap_or_default().trim().to_ascii_lowercase();
    if typed.is_empty() {
        return None;
    }
    if SLASH_COMMANDS.iter().any(|cmd| cmd.name == typed) {
        return Some(typed);
    }

    let suggestions = app.typeahead_items();
    suggestions
        .get(app.typeahead_selected.min(suggestions.len().saturating_sub(1)))
        .map(|cmd| cmd.name.to_string())
}

fn slash_command_help() -> String {
    let mut out = String::new();
    out.push_str("Slash commands\n\n");
    for cmd in SLASH_COMMANDS {
        out.push_str(&format!("- `{}`: {} ({})\n", cmd.name, cmd.description, cmd.usage));
    }
    out.push_str("\nUseful shortcuts\n\n");
    out.push_str("- `Ctrl+L`: start a new empty session\n");
    out.push_str("- `Ctrl+X Ctrl+K`: compact the current chat\n");
    out.push_str("- `Ctrl+X Ctrl+C`: exit\n");
    out.push_str("- `Tab`: accept the selected slash command\n");
    out.push_str("\nConfig\n\n");
    out.push_str("- Override shortcuts via `tuiKeybindings` in settings JSON (user/project/local).\n");
    out
}

fn role_header(role: Role) -> Line<'static> {
    let (label, style) = match role {
        Role::User => (
            "You",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Role::Assistant => (
            "Claude",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Role::Tool => (
            "Tool",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Role::System => (
            "System",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::DIM),
        ),
    };

    Line::from(Span::styled(label, style))
}

fn entry_index_for_line(line_offsets: &[usize], line: usize) -> usize {
    let entries = line_offsets.len().saturating_sub(1);
    if entries == 0 {
        return 0;
    }

    // `line_offsets` is a non-decreasing prefix sum. We want the last index `i`
    // where `line_offsets[i] <= line`.
    match line_offsets.binary_search(&line) {
        Ok(i) => i.min(entries.saturating_sub(1)),
        Err(i) => i.saturating_sub(1).min(entries.saturating_sub(1)),
    }
}

fn render_transcript(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    area: ratatui::layout::Rect,
    start_line: usize,
) {
    if area.height == 0 || app.rendered.is_empty() {
        return;
    }

    // Week 3: virtual scrolling. Use `line_offsets` to jump straight to the first
    // entry intersecting `start_line` and only iterate the visible tail.
    let entries = app.rendered.len();
    if app.line_offsets.len() != entries.saturating_add(1) {
        return;
    }
    let total_lines = app.total_rendered_lines();
    if total_lines == 0 {
        return;
    }
    if start_line >= total_lines {
        return;
    }

    let start_idx = entry_index_for_line(&app.line_offsets, start_line);
    let mut skip_in_entry = start_line.saturating_sub(app.line_offsets[start_idx]);

    let mut row: u16 = 0;
    for entry in app.rendered.iter().skip(start_idx) {
        if row >= area.height {
            break;
        }

        // Header (1 line)
        if skip_in_entry == 0 {
            let line_area = ratatui::layout::Rect {
                x: area.x,
                y: area.y.saturating_add(row),
                width: area.width,
                height: 1,
            };
            f.render_widget(&entry.header, line_area);
            row = row.saturating_add(1);
        } else {
            skip_in_entry = skip_in_entry.saturating_sub(1);
        }

        if row >= area.height {
            break;
        }

        // Body (N lines)
        let body_len = entry.body.line_count();
        let body_skip = skip_in_entry.min(body_len);
        match &entry.body {
            RenderedBody::Static(lines) => {
                for line in lines.iter().skip(body_skip) {
                    if row >= area.height {
                        break;
                    }
                    let line_area = ratatui::layout::Rect {
                        x: area.x,
                        y: area.y.saturating_add(row),
                        width: area.width,
                        height: 1,
                    };
                    f.render_widget(line, line_area);
                    row = row.saturating_add(1);
                }
            }
            RenderedBody::Streaming(stream) => {
                for line in stream.iter_lines().skip(body_skip) {
                    if row >= area.height {
                        break;
                    }
                    let line_area = ratatui::layout::Rect {
                        x: area.x,
                        y: area.y.saturating_add(row),
                        width: area.width,
                        height: 1,
                    };
                    f.render_widget(line, line_area);
                    row = row.saturating_add(1);
                }
            }
        }
        if row >= area.height {
            break;
        }

        // Separator blank line (1 line).
        row = row.saturating_add(1);

        // After the first entry, we should never need to skip again.
        skip_in_entry = 0;
    }
}

pub async fn run_tui(
    args: &Args,
    settings: &claude_core::config::settings::Settings,
    auth: AuthMode,
) -> anyhow::Result<()> {
    if !matches!(args.output_format, OutputFormat::Text) {
        return Err(crate::UsageError(
            "TUI mode requires --output-format text".to_string(),
        )
        .into());
    }
    if !matches!(args.input_format, InputFormat::Text) {
        return Err(crate::UsageError(
            "TUI mode requires --input-format text".to_string(),
        )
        .into());
    }
    if args.replay_user_messages {
        return Err(crate::UsageError(
            "TUI mode does not support --replay-user-messages".to_string(),
        )
        .into());
    }

    let _term = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).context("init terminal")?;
    terminal.clear().ok();

    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let (session_id, session_path, history) = crate::resolve_session(args, &cwd)?;

    let model = crate::resolve_model(args.model.clone(), settings.model.clone());

    let config_home = claude_core::paths::claude_config_home_dir()?;
    let user_settings_path = config_home.join("settings.json");
    let always_allow_tools = Arc::new(Mutex::new(
        settings
            .always_allow_tools
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<HashSet<_>>(),
    ));

    let md = MarkdownRenderer::new();

    let prompt_history = PromptHistory::new(prompt_history_from_messages(&history));
    let vim = matches!(settings.editor_mode, Some(EditorMode::Vim)).then(VimMachine::new);

    let mut keymap = KeybindingResolver::new(Duration::from_millis(900));
    // Global defaults. These can be overridden via settings `tuiKeybindings`.
    keymap.add_binding(
        &[KeyContext::Global],
        KeySequence::parse("ctrl+l")?,
        KeyAction::ClearChat,
    );
    // Week 8 deliverable: chord detection.
    keymap.add_binding(
        &[KeyContext::Global],
        KeySequence::parse("ctrl+x ctrl+k")?,
        KeyAction::CompactChat,
    );
    keymap.add_binding(
        &[KeyContext::Global],
        KeySequence::parse("ctrl+x ctrl+c")?,
        KeyAction::Quit,
    );
    // Slash command typeahead navigation.
    keymap.add_binding(
        &[KeyContext::CommandTypeahead],
        KeySequence::parse("up")?,
        KeyAction::TypeaheadPrev,
    );
    keymap.add_binding(
        &[KeyContext::CommandTypeahead],
        KeySequence::parse("down")?,
        KeyAction::TypeaheadNext,
    );
    keymap.add_binding(
        &[KeyContext::CommandTypeahead],
        KeySequence::parse("tab")?,
        KeyAction::TypeaheadAccept,
    );
    keymap.add_binding(
        &[KeyContext::CommandTypeahead],
        KeySequence::parse("enter")?,
        KeyAction::TypeaheadExecute,
    );
    keymap.add_binding(
        &[KeyContext::CommandTypeahead],
        KeySequence::parse("esc")?,
        KeyAction::TypeaheadDismiss,
    );

    let keymap_warnings = settings
        .tui_keybindings
        .as_ref()
        .map(|m| keymap.apply_user_overrides(m))
        .unwrap_or_default();

    let mut transcript = transcript_from_history(&history);
    if transcript.is_empty() {
        transcript.push(ChatEntry {
            role: Role::System,
            text: "Ctrl+C to exit. Type a prompt and press Enter.".to_string(),
        });
    }
    if !keymap_warnings.is_empty() {
        transcript.push(ChatEntry {
            role: Role::System,
            text: format!(
                "warn: some tuiKeybindings entries were ignored:\n- {}",
                keymap_warnings.join("\n- ")
            ),
        });
    }
    let rendered = transcript.iter().map(|e| RenderedEntry::new(e.role)).collect();

    let client = claude_services::api::AnthropicClient::new(None);
    let engine_inputs = compute_engine_inputs(args, settings)?;
    let engine = std::sync::Arc::new(claude_query::QueryEngine::new(
        client.clone(),
        auth.clone(),
        model.clone(),
        engine_inputs.max_tokens,
        engine_inputs.cfg.clone(),
    )?);

    let mut app = App {
        input: InputBuffer::new(),
        prompt_history,
        reverse_history_search: None,
        vim,
        keymap,
        typeahead_query: None,
        typeahead_selected: 0,
        typeahead_suppressed_text: None,
        status: "ready".to_string(),
        spinner_idx: 0,
        in_flight: false,
        active_assistant_idx: None,
        transcript,
        rendered,
        render_width: 0,
        md,
        history,
        tool_entry_for_id: HashMap::new(),
        permission_prompt: None,
        always_allow_tools,
        scroll_top: 0,
        scroll_follow: true,
        last_msg_view_height: 0,
        line_offsets: Vec::new(),
        session_id,
        session_path,
        model: model.clone(),
        cwd,
        user_settings_path,
        client,
        auth,
        engine_inputs,
        engine,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cost_usd: Some(0.0),
        last_turn_input_tokens: 0,
        last_turn_output_tokens: 0,
        last_turn_cost_usd: None,
    };
    let (query_tx, mut query_rx) = mpsc::unbounded_channel::<QueryEvent>();

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal
            .draw(|f| render(f, &mut app))
            .context("render")?;

        tokio::select! {
            maybe_ev = events.next() => {
                let Some(ev) = maybe_ev else { continue; };
                let ev = ev.context("read terminal event")?;
                if handle_term_event(&mut app, ev, query_tx.clone()).await? {
                    break;
                }
            }
            maybe_qev = query_rx.recv() => {
                let Some(qev) = maybe_qev else { continue; };
                handle_query_event(&mut app, qev);
            }
            _ = tick.tick() => {
                if app.in_flight {
                    app.spinner_idx = (app.spinner_idx + 1) % SPINNER_FRAMES.len();
                }
            }
        }
    }

    terminal.clear().ok();
    Ok(())
}

fn compute_engine_inputs(
    args: &Args,
    settings: &claude_core::config::settings::Settings,
 ) -> anyhow::Result<EngineInputs> {
    let max_tokens = args.max_tokens.unwrap_or(1024);
    let max_turns = args.max_turns.unwrap_or(8);

    let system_prompt = crate::load_system_prompt_override(args)?;
    let append_system_prompt = crate::load_append_system_prompt(args)?;

    let cwd = std::env::current_dir()?;

    let permission_mode = args
        .permission_mode
        .or(settings.permission_mode)
        .unwrap_or(PermissionMode::Default);

    let mut allowed_tools = settings.allowed_tools.clone().unwrap_or_default();
    allowed_tools.extend(args.allowed_tools.clone());

    let mut disallowed_tools = settings.disallowed_tools.clone().unwrap_or_default();
    disallowed_tools.extend(args.disallowed_tools.clone());

    let always_allow_tools = settings.always_allow_tools.clone().unwrap_or_default();

    // Week 1: AskUserQuestion reads stdin and will break raw-mode TUI. Later
    // weeks implement a proper in-TUI prompt for tool questions.
    if !disallowed_tools.iter().any(|t| t == "AskUserQuestion") {
        disallowed_tools.push("AskUserQuestion".to_string());
    }

    let mcp_servers = crate::resolve_mcp_servers(args, settings)?;

    Ok(EngineInputs {
        max_tokens,
        cfg: claude_query::QueryEngineConfig {
            cwd,
            bare: args.bare,
            add_dirs: args.add_dir.clone(),
            system_prompt: system_prompt.or_else(|| settings.custom_system_prompt.clone()),
            append_system_prompt,
            json_schema: args.json_schema.clone(),
            max_turns,
            max_budget_usd: args.max_budget_usd,
            permission_mode,
            base_tools: args.tools.clone(),
            allowed_tools,
            disallowed_tools,
            always_allow_tools,
            mcp_servers,
            agent_depth: 0,
            max_agent_depth: 2,
        },
    })
}

fn scroll_up(app: &mut App, amount: usize) {
    if amount == 0 {
        return;
    }
    app.scroll_top = app.scroll_top.saturating_sub(amount);
    app.scroll_follow = false;
}

fn scroll_down(app: &mut App, amount: usize) {
    if amount == 0 {
        return;
    }
    app.scroll_top = app.scroll_top.saturating_add(amount);
}

fn scroll_to_top(app: &mut App) {
    app.scroll_top = 0;
    app.scroll_follow = false;
}

fn scroll_to_bottom(app: &mut App) {
    app.scroll_follow = true;
}

fn vim_key_from_code(code: KeyCode) -> Option<VimKey> {
    match code {
        KeyCode::Char(ch) => Some(VimKey::Char(ch)),
        KeyCode::Left => Some(VimKey::Left),
        KeyCode::Right => Some(VimKey::Right),
        KeyCode::Up => Some(VimKey::Up),
        KeyCode::Down => Some(VimKey::Down),
        KeyCode::Backspace => Some(VimKey::Backspace),
        KeyCode::Delete => Some(VimKey::Delete),
        KeyCode::Enter => Some(VimKey::Enter),
        KeyCode::Esc => Some(VimKey::Esc),
        KeyCode::Home => Some(VimKey::Char('0')),
        KeyCode::End => Some(VimKey::Char('$')),
        _ => None,
    }
}

async fn handle_term_event(
    app: &mut App,
    ev: Event,
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<bool> {
    match ev {
        Event::Paste(text) => {
            if app.permission_prompt.is_some() {
                return Ok(false);
            }

            if let Some(search) = app.reverse_history_search.as_mut() {
                let mut q = text.replace('\n', " ");
                q = q.replace('\r', " ");
                search.push_str(q.trim_end(), app.prompt_history.entries(), &mut app.input);
            } else {
                if let Some(vim) = app.vim.as_mut() {
                    if vim.is_normal() {
                        // Safety: never interpret a paste as Normal-mode commands.
                        vim.cancel_pending();
                        let _ = vim.handle_normal_key(VimKey::Char('i'), &mut app.input);
                    }
                    if vim.is_insert() {
                        app.input.insert_str(&text);
                        vim.on_insert_text(&text);
                        return Ok(false);
                    }
                }

                app.input.insert_str(&text);
            }
        }
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }

            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
            let vim_normal = app.vim.as_ref().is_some_and(|v| v.is_normal());

            if ctrl {
                if let KeyCode::Char('c') = key.code {
                    return Ok(true);
                }
            }

            if app.permission_prompt.is_some() {
                let decision = match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        Some(claude_query::PermissionDecision::AllowOnce)
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        Some(claude_query::PermissionDecision::Deny)
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        Some(claude_query::PermissionDecision::AlwaysAllowTool)
                    }
                    _ => None,
                };

                if let Some(decision) = decision {
                    if let Some(prompt) = app.permission_prompt.take() {
                        if matches!(decision, claude_query::PermissionDecision::AlwaysAllowTool) {
                            // Update in-memory allowlist so future prompts in this TUI session can
                            // auto-approve without requiring a restart.
                            if let Ok(mut set) = app.always_allow_tools.lock() {
                                set.insert(prompt.tool_name.trim().to_ascii_lowercase());
                            }

                            let saved = persist_always_allow_tool(
                                &app.user_settings_path,
                                &prompt.tool_name,
                            );

                            match saved {
                                Ok(true) => {
                                    app.status = format!("always allow {} (saved)", prompt.tool_name)
                                }
                                Ok(false) => {
                                    app.status =
                                        format!("always allow {} (already saved)", prompt.tool_name)
                                }
                                Err(err) => {
                                    app.status = format!(
                                        "always allow {} (save failed: {})",
                                        prompt.tool_name,
                                        crate::one_line_preview(&err.to_string(), 120)
                                    )
                                }
                            }
                        }

                        let _ = prompt.reply_tx.send(decision);
                        if !matches!(decision, claude_query::PermissionDecision::AlwaysAllowTool) {
                            match decision {
                                claude_query::PermissionDecision::AllowOnce => {
                                    app.status = format!("allowed {}", prompt.tool_name)
                                }
                                claude_query::PermissionDecision::Deny => {
                                    app.status = format!("denied {}", prompt.tool_name)
                                }
                                claude_query::PermissionDecision::AlwaysAllowTool => {}
                            }
                        }
                    }
                }
                return Ok(false);
            }

            if let Some(mut search) = app.reverse_history_search.take() {
                let history = app.prompt_history.entries();
                let keep_search = match key.code {
                    KeyCode::Char('r') | KeyCode::Char('R') if ctrl => {
                        search.next_match(history, &mut app.input);
                        true
                    }
                    KeyCode::Char('g') | KeyCode::Char('G') if ctrl => {
                        search.cancel(&mut app.input);
                        false
                    }
                    KeyCode::Esc => {
                        search.cancel(&mut app.input);
                        false
                    }
                    KeyCode::Enter => {
                        search.accept();
                        false
                    }
                    KeyCode::Backspace => {
                        search.backspace(history, &mut app.input);
                        true
                    }
                    KeyCode::Char(ch) if !ctrl && !alt => {
                        search.push_char(ch, history, &mut app.input);
                        true
                    }
                    _ => true,
                };

                if keep_search {
                    app.reverse_history_search = Some(search);
                } else if let Some(vim) = app.vim.as_mut() {
                    // Reverse-search sets the buffer programmatically; don't let dot-repeat track it.
                    vim.reset_insert_tracking();
                }
                return Ok(false);
            }

            app.sync_typeahead();
            let ctx = app.current_key_context();
            match app.keymap.resolve(ctx, key) {
                ResolveOutcome::Matched(action) => {
                    if handle_key_action(app, action, query_tx.clone()).await? {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                ResolveOutcome::PendingChord => {
                    // Keep the previous status (thinking, running tools, etc.) but add a hint.
                    if !app.status.contains("chord") {
                        app.status = format!("{} • chord…", app.status);
                    }
                    return Ok(false);
                }
                ResolveOutcome::NoMatch => {}
            }

            if ctrl {
                match key.code {
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        let mut search = ReverseHistorySearch::new(
                            &app.input,
                            app.prompt_history.entries().len(),
                        );
                        search.search_next(app.prompt_history.entries(), &mut app.input);
                        app.reverse_history_search = Some(search);
                        return Ok(false);
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        if !vim_normal {
                            app.input.move_to_start(selecting);
                        }
                        return Ok(false);
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        if !vim_normal {
                            app.input.move_to_end(selecting);
                        }
                        return Ok(false);
                    }
                    KeyCode::Left => {
                        if !vim_normal {
                            app.input.move_word_left(selecting);
                        }
                        return Ok(false);
                    }
                    KeyCode::Right => {
                        if !vim_normal {
                            app.input.move_word_right(selecting);
                        }
                        return Ok(false);
                    }
                    // Week 3: scrollback. Up/Down are history navigation, so use Ctrl+Up/Down.
                    KeyCode::Up => {
                        scroll_up(app, 1);
                        return Ok(false);
                    }
                    KeyCode::Down => {
                        scroll_down(app, 1);
                        return Ok(false);
                    }
                    KeyCode::Home => {
                        scroll_to_top(app);
                        return Ok(false);
                    }
                    KeyCode::End => {
                        scroll_to_bottom(app);
                        return Ok(false);
                    }
                    _ => {}
                }
            }

            match key.code {
                // Week 3: scrollback.
                KeyCode::PageUp => {
                    let amount = app.last_msg_view_height.saturating_sub(1).max(1);
                    scroll_up(app, amount);
                    return Ok(false);
                }
                KeyCode::PageDown => {
                    let amount = app.last_msg_view_height.saturating_sub(1).max(1);
                    scroll_down(app, amount);
                    return Ok(false);
                }
                _ => {}
            }

            if let Some(vim) = app.vim.as_mut() {
                if vim.is_normal() && !ctrl && !alt {
                    if let Some(vk) = vim_key_from_code(key.code) {
                        let res = vim.handle_normal_key(vk, &mut app.input);
                        if matches!(res, VimHandleResult::Submit) {
                            if handle_submit(app, query_tx.clone()).await? {
                                return Ok(true);
                            }
                        }
                    }
                    return Ok(false);
                }

                if vim.is_insert() && matches!(key.code, KeyCode::Esc) {
                    vim.enter_normal_mode(&mut app.input);
                    return Ok(false);
                }
            }

            // In Vim NORMAL mode, ignore non-vim modifiers instead of treating them as readline
            // editing/history navigation.
            if vim_normal {
                return Ok(false);
            }

            match key.code {
                // Week 6: command history.
                KeyCode::Up => {
                    if alt {
                        app.input.move_up_line(selecting);
                    } else {
                        app.prompt_history.prev(&mut app.input);
                        if let Some(vim) = app.vim.as_mut() {
                            vim.reset_insert_tracking();
                        }
                    }
                }
                KeyCode::Down => {
                    if alt {
                        app.input.move_down_line(selecting);
                    } else {
                        app.prompt_history.next(&mut app.input);
                        if let Some(vim) = app.vim.as_mut() {
                            vim.reset_insert_tracking();
                        }
                    }
                }
                KeyCode::Home => {
                    app.input.move_to_start(selecting);
                }
                KeyCode::End => {
                    app.input.move_to_end(selecting);
                }
                KeyCode::Esc => {
                    if app.input.has_selection() {
                        app.input.clear_selection();
                    } else if app.prompt_history.is_navigating() {
                        app.prompt_history.cancel_navigation(&mut app.input);
                    } else {
                        app.input.clear();
                    }
                }
                KeyCode::Backspace => {
                    app.input.backspace();
                    if let Some(vim) = app.vim.as_mut() {
                        vim.on_insert_backspace();
                    }
                }
                KeyCode::Delete => {
                    app.input.delete_forward();
                    if let Some(vim) = app.vim.as_mut() {
                        vim.on_insert_backspace();
                    }
                }
                KeyCode::Left => {
                    if alt {
                        app.input.move_word_left(selecting);
                    } else {
                        app.input.move_left(selecting);
                    }
                }
                KeyCode::Right => {
                    if alt {
                        app.input.move_word_right(selecting);
                    } else {
                        app.input.move_right(selecting);
                    }
                }
                // Week 6: multi-line input.
                KeyCode::Enter => {
                    if alt {
                        app.input.insert_char('\n');
                        if let Some(vim) = app.vim.as_mut() {
                            vim.on_insert_text("\n");
                        }
                    } else {
                        if handle_submit(app, query_tx.clone()).await? {
                            return Ok(true);
                        }
                    }
                }
                KeyCode::Tab => {
                    app.input.insert_char('\t');
                    if let Some(vim) = app.vim.as_mut() {
                        vim.on_insert_text("\t");
                    }
                }
                KeyCode::Char(ch) => {
                    if !ctrl && !alt {
                        app.input.insert_char(ch);
                        if let Some(vim) = app.vim.as_mut() {
                            let mut buf = [0u8; 4];
                            let s = ch.encode_utf8(&mut buf);
                            vim.on_insert_text(s);
                        }
                    }
                }
                _ => {}
            }
        }
        Event::Resize(_, _) => {}
        _ => {}
    }
    Ok(false)
}

async fn handle_submit(
    app: &mut App,
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<bool> {
    if app.in_flight {
        return Ok(false);
    }

    app.sync_typeahead();
    let raw = app.input.as_str().to_string();
    let trimmed = raw.trim();

    if trimmed.starts_with('/') {
        if let Some(parsed) = parse_slash_command(trimmed) {
            if SLASH_COMMANDS.iter().any(|c| c.name == parsed.name) {
                app.input.clear();
                app.dismiss_typeahead();
                return execute_slash_command(app, &parsed.name, &parsed.args, query_tx).await;
            }
        }

        // If the user is typing a partial slash command and typeahead is open,
        // Enter should execute the selected suggestion.
        if app.typeahead_active() {
            if let Some(name) = slash_candidate_name(app) {
                app.input.clear();
                app.dismiss_typeahead();
                return execute_slash_command(app, &name, &[], query_tx).await;
            }
        }
    }

    submit_prompt(app, query_tx)?;
    Ok(false)
}

async fn handle_key_action(
    app: &mut App,
    action: KeyAction,
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<bool> {
    match action {
        KeyAction::Quit => return Ok(true),
        KeyAction::ClearChat => {
            if app.in_flight {
                app.status = "busy; wait for the current run to finish".to_string();
            } else {
                app.clear_chat()?;
            }
        }
        KeyAction::CompactChat => {
            if app.in_flight {
                app.status = "busy; wait for the current run to finish".to_string();
            } else {
                return execute_slash_command(app, "compact", &[], query_tx).await;
            }
        }
        KeyAction::ShowHelp => {
            app.push_system_message(slash_command_help());
            app.status = "help".to_string();
        }
        KeyAction::ShowCost => {
            return execute_slash_command(app, "cost", &[], query_tx).await;
        }
        KeyAction::TypeaheadNext => {
            app.move_typeahead(1);
            app.status = "slash command".to_string();
        }
        KeyAction::TypeaheadPrev => {
            app.move_typeahead(-1);
            app.status = "slash command".to_string();
        }
        KeyAction::TypeaheadAccept => {
            if app.accept_typeahead() {
                app.status = "slash command".to_string();
            }
        }
        KeyAction::TypeaheadExecute => {
            return handle_submit(app, query_tx).await;
        }
        KeyAction::TypeaheadDismiss => {
            app.dismiss_typeahead();
            app.status = "ready".to_string();
        }
    }

    Ok(false)
}

async fn execute_slash_command(
    app: &mut App,
    name: &str,
    args: &[String],
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<bool> {
    let cmd = name.trim().to_ascii_lowercase();
    match cmd.as_str() {
        "help" => {
            app.push_system_message(slash_command_help());
            app.status = "help".to_string();
        }
        "model" => {
            if args.is_empty() {
                app.push_system_message(format!("Current model: `{}`", app.model));
                app.status = format!("model {}", app.model);
            } else {
                let next = args.join(" ").trim().to_string();
                if next.is_empty() {
                    app.push_system_message("usage: /model [model-id]");
                } else if app.in_flight {
                    app.status = "cannot change model while a run is active".to_string();
                } else {
                    app.model = next.clone();
                    app.rebuild_engine()?;
                    app.push_system_message(format!("Model updated to `{next}`"));
                    app.status = format!("model {}", app.model);
                }
            }
        }
        "clear" => {
            if app.in_flight {
                app.status = "cannot clear while a run is active".to_string();
            } else {
                app.clear_chat()?;
            }
        }
        "compact" => {
            if app.in_flight {
                app.status = "cannot compact while a run is active".to_string();
            } else if app.history.len() < 4 {
                app.push_system_message("Not enough history to compact yet.");
                app.status = "compact skipped".to_string();
            } else {
                app.in_flight = true;
                app.spinner_idx = 0;
                app.status = "compacting history...".to_string();

                let engine = app.engine.clone();
                let history = app.history.clone();
                let cwd = app.cwd.clone();
                tokio::spawn(async move {
                    match engine.compact_history_now(history).await {
                        Ok(compacted) => {
                            let session_id = SessionId::new();
                            let session_path = match claude_core::history::session_file_path(&cwd, session_id) {
                                Ok(path) => path,
                                Err(err) => {
                                    let _ = query_tx.send(QueryEvent::CompactionError(err.to_string()));
                                    return;
                                }
                            };

                            // Best-effort persistence so `--continue/--resume` sees the compacted context.
                            let _ = claude_core::history::append_session_messages(&session_path, &compacted);

                            let _ = query_tx.send(QueryEvent::CompactionFinished {
                                session_id,
                                session_path,
                                history: compacted,
                            });
                        }
                        Err(err) => {
                            let _ = query_tx.send(QueryEvent::CompactionError(err.to_string()));
                        }
                    }
                });
            }
        }
        "cost" => {
            let body = match app.total_cost_usd {
                Some(total) => format!(
                    "Session usage\n\n- total input tokens: {}\n- total output tokens: {}\n- total cost: ${:.4}\n- last turn input tokens: {}\n- last turn output tokens: {}\n{}",
                    app.total_input_tokens,
                    app.total_output_tokens,
                    total,
                    app.last_turn_input_tokens,
                    app.last_turn_output_tokens,
                    app.last_turn_cost_usd
                        .map(|v| format!("- last turn cost: ${v:.4}"))
                        .unwrap_or_else(|| "- last turn cost: unavailable".to_string()),
                ),
                None => format!(
                    "Session usage\n\n- total input tokens: {}\n- total output tokens: {}\n- total cost: unavailable\n- last turn input tokens: {}\n- last turn output tokens: {}\n- last turn cost: unavailable",
                    app.total_input_tokens,
                    app.total_output_tokens,
                    app.last_turn_input_tokens,
                    app.last_turn_output_tokens,
                ),
            };
            app.push_system_message(body);
            app.status = "cost".to_string();
        }
        "exit" => return Ok(true),
        other => {
            let known = SLASH_COMMANDS.iter().map(|c| c.name).collect::<Vec<_>>().join(", ");
            app.push_system_message(format!("Unknown slash command `{other}`. Available: {known}"));
            app.status = format!("unknown command `{other}`");
        }
    }

    Ok(false)
}

fn submit_prompt(
    app: &mut App,
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<()> {
    if app.in_flight {
        return Ok(());
    }

    let prompt = app.input.as_str().to_string();
    if prompt.trim().is_empty() {
        return Ok(());
    }

    app.prompt_history.push(prompt.clone());
    app.prompt_history.reset_navigation();
    app.reverse_history_search = None;
    app.input.clear();
    if let Some(vim) = app.vim.as_mut() {
        vim.reset_insert_tracking();
    }
    app.permission_prompt = None;
    app.tool_entry_for_id.clear();
    app.active_assistant_idx = None;

    app.transcript.push(ChatEntry {
        role: Role::User,
        text: prompt.clone(),
    });
    app.rendered.push(RenderedEntry::new(Role::User));

    // Week 3: make sure the new turn is visible even if the user had scrolled up.
    scroll_to_bottom(app);

    app.status = "thinking...".to_string();
    app.in_flight = true;
    app.spinner_idx = 0;

    let user_msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: prompt }],
    });

    // Persist the user message immediately so an interrupted run is resumable.
    let _ = claude_core::history::append_session_messages(&app.session_path, &[user_msg.clone()]);

    app.history.push(user_msg);

    let history_for_engine = app.history.clone();
    let session_path = app.session_path.clone();
    let session_id = app.session_id;
    let always_allow_tools = app.always_allow_tools.clone();
    let engine = app.engine.clone();

    tokio::spawn(async move {
        let tx_for_deltas = query_tx.clone();
        let observer: std::sync::Arc<dyn claude_query::QueryObserver> =
            std::sync::Arc::new(TuiObserver { tx: query_tx.clone(), always_allow_tools });

        let res = engine
            .run_with_history_observed(history_for_engine, |event| {
                if let Some(text) = crate::extract_text_delta(event) {
                    let _ = tx_for_deltas.send(QueryEvent::TextDelta(text.to_string()));
                }
                Ok(())
            }, observer)
            .await;

        match res {
            Ok(result) => {
                if !result.new_messages.is_empty() {
                    let _ = claude_core::history::append_session_messages(&session_path, &result.new_messages);
                }
                write_session_meta_silent(session_id, &session_path, &result);
                let _ = query_tx.send(QueryEvent::Finished(result));
            }
            Err(err) => {
                let _ = query_tx.send(QueryEvent::Error(err.to_string()));
            }
        }
    });

    Ok(())
}

fn handle_query_event(app: &mut App, qev: QueryEvent) {
    match qev {
        QueryEvent::TextDelta(delta) => {
            let idx = match app.active_assistant_idx {
                Some(idx) => idx,
                None => {
                    app.transcript.push(ChatEntry {
                        role: Role::Assistant,
                        text: String::new(),
                    });
                    app.rendered.push(RenderedEntry::new_streaming(Role::Assistant));
                    let idx = app.transcript.len().saturating_sub(1);
                    app.active_assistant_idx = Some(idx);
                    idx
                }
            };
            if let Some(entry) = app.transcript.get_mut(idx) {
                entry.text.push_str(&delta);
            }
            if let Some(cache) = app.rendered.get_mut(idx) {
                cache.dirty = true;
            }
        }
        QueryEvent::PermissionRequest {
            id,
            name,
            input,
            reply_tx,
        } => {
            // The assistant turn that requested tool use has finished streaming at this point.
            if let Some(idx) = app.active_assistant_idx.take() {
                app.finalize_streaming(idx);
            }
            let activity = tool_activity_status(&name, &input);
            let details = permission_details(&app.cwd, &name, &input);
            app.permission_prompt = Some(PermissionPrompt {
                id,
                tool_name: name,
                details,
                reply_tx,
            });
            app.status = format!("permission required • {activity}");
        }
        QueryEvent::ToolUseStart { id, name, input } => {
            if let Some(idx) = app.active_assistant_idx.take() {
                app.finalize_streaming(idx);
            }

            let text = format_tool_running_markdown(&name, &input);
            app.transcript.push(ChatEntry {
                role: Role::Tool,
                text,
            });
            app.rendered.push(RenderedEntry::new(Role::Tool));
            let idx = app.transcript.len().saturating_sub(1);
            app.tool_entry_for_id.insert(id, idx);
            app.status = tool_activity_status(&name, &input);
        }
        QueryEvent::ToolUseResult {
            id,
            name,
            input,
            result,
            is_error,
        } => {
            let text = format_tool_result_markdown(&name, &input, &result, is_error);
            if let Some(idx) = app.tool_entry_for_id.remove(&id) {
                if let Some(entry) = app.transcript.get_mut(idx) {
                    entry.text = text;
                }
                if let Some(cache) = app.rendered.get_mut(idx) {
                    cache.dirty = true;
                }
            } else {
                app.transcript.push(ChatEntry {
                    role: Role::Tool,
                    text,
                });
                app.rendered.push(RenderedEntry::new(Role::Tool));
            }
            app.status = format!("{name} done");
        }
        QueryEvent::Finished(result) => {
            let finished_idx = app.active_assistant_idx;
            app.in_flight = false;
            app.active_assistant_idx = None;
            app.permission_prompt = None;
            app.record_run_cost(&result);
            app.history = result.history;
            if let Some(idx) = finished_idx {
                app.finalize_streaming(idx);
            }

            app.status = match result.cost_usd {
                Some(cost) => format!(
                    "done • in={} out={} • ${:.4}",
                    result.usage.input_tokens, result.usage.output_tokens, cost
                ),
                None => format!(
                    "done • in={} out={}",
                    result.usage.input_tokens, result.usage.output_tokens
                ),
            };
        }
        QueryEvent::CompactionFinished {
            session_id,
            session_path,
            history,
        } => {
            app.in_flight = false;
            app.active_assistant_idx = None;
            app.permission_prompt = None;
            app.tool_entry_for_id.clear();

            app.session_id = session_id;
            app.session_path = session_path;
            app.history = history;

            app.transcript = transcript_from_history(&app.history);
            if app.transcript.is_empty() {
                app.transcript.push(ChatEntry {
                    role: Role::System,
                    text: "Ctrl+C to exit. Type a prompt and press Enter.".to_string(),
                });
            }
            app.rendered = app.transcript.iter().map(|e| RenderedEntry::new(e.role)).collect();
            app.render_width = 0;
            app.scroll_top = 0;
            app.scroll_follow = true;

            app.push_system_message(format!("Compaction complete • new session {}", app.session_id));
            app.status = "compaction done".to_string();
        }
        QueryEvent::CompactionError(err) => {
            app.in_flight = false;
            app.status = format!("compact failed: {}", crate::one_line_preview(&err, 160));
            app.push_system_message(format!("error: compaction failed: {err}"));
        }
        QueryEvent::Error(err) => {
            app.in_flight = false;
            app.active_assistant_idx = None;
            app.permission_prompt = None;

            // If we created an empty assistant entry for streaming, remove it on error.
            if let Some(last) = app.transcript.last() {
                if last.role == Role::Assistant && last.text.is_empty() {
                    app.transcript.pop();
                    app.rendered.pop();
                }
            }

            app.status = format!("error: {}", crate::one_line_preview(&err, 160));
            app.transcript.push(ChatEntry {
                role: Role::System,
                text: format!("error: {err}"),
            });
            app.rendered.push(RenderedEntry::new(Role::System));
        }
    }
}

struct PreparedInput {
    text: Text<'static>,
    cursor_row: usize,
    cursor_col: usize,
    visible_line_count: usize,
}

fn prompt_history_from_messages(history: &[Message]) -> Vec<String> {
    let mut out = Vec::new();

    for msg in history {
        let Message::User(UserMessage { content }) = msg else {
            continue;
        };

        let mut text_buf = String::new();
        for block in content {
            if let ContentBlock::Text { text } = block {
                if !text_buf.is_empty() {
                    text_buf.push('\n');
                }
                text_buf.push_str(text);
            }
        }

        if !text_buf.trim().is_empty() {
            out.push(text_buf);
        }
    }

    out
}

fn prepare_input_render(input: &InputBuffer, width: usize, max_lines: usize) -> PreparedInput {
    let width = width.max(1);
    let selection = input.selection_range();
    let mut raw_lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut row: usize = 0;
    let mut col: usize = 0;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = 0;

    let plain = Style::default();
    let selected = Style::default().fg(Color::Black).bg(Color::Blue);

    if input.cursor() == 0 {
        cursor_row = 0;
        cursor_col = 0;
    }

    for (byte_idx, ch) in input.as_str().char_indices() {
        if ch == '\n' {
            raw_lines.push(std::mem::take(&mut current));
            row = row.saturating_add(1);
            col = 0;

            let next_idx = byte_idx.saturating_add(ch.len_utf8());
            if input.cursor() == next_idx {
                cursor_row = row;
                cursor_col = col;
            }
            continue;
        }

        let style = if selection
            .map(|(start, end)| byte_idx >= start && byte_idx < end)
            .unwrap_or(false)
        {
            selected
        } else {
            plain
        };

        let display = if ch == '\t' {
            String::from("    ")
        } else {
            ch.to_string()
        };

        for disp in display.chars() {
            current.push(Span::styled(disp.to_string(), style));
            col = col.saturating_add(1);
            if col >= width {
                raw_lines.push(std::mem::take(&mut current));
                row = row.saturating_add(1);
                col = 0;
            }
        }

        let next_idx = byte_idx.saturating_add(ch.len_utf8());
        if input.cursor() == next_idx {
            cursor_row = row;
            cursor_col = col;
        }
    }

    raw_lines.push(std::mem::take(&mut current));
    if raw_lines.is_empty() {
        raw_lines.push(Vec::new());
    }

    let total_lines = raw_lines.len().max(1);
    let max_lines = max_lines.max(1);
    let visible_line_count = total_lines.clamp(1, max_lines);
    let scroll_top = cursor_row
        .saturating_add(1)
        .saturating_sub(visible_line_count);

    let lines = raw_lines
        .into_iter()
        .skip(scroll_top)
        .take(visible_line_count)
        .map(|spans| {
            if spans.is_empty() {
                Line::from("")
            } else {
                Line::from(spans)
            }
        })
        .collect::<Vec<_>>();

    PreparedInput {
        text: Text::from(lines),
        cursor_row: cursor_row.saturating_sub(scroll_top),
        cursor_col,
        visible_line_count,
    }
}

fn render_status_line(app: &App, spin: &str) -> Line<'static> {
    let style = Style::default().fg(Color::Gray).add_modifier(Modifier::DIM);
    let vim_label = app.vim.as_ref().map(|vim| match vim.mode() {
        VimMode::Insert => "-- INSERT --",
        VimMode::Normal => "-- NORMAL --",
    });

    if let Some(search) = &app.reverse_history_search {
        let query = search.query();
        let preview = crate::one_line_preview(app.input.as_str(), 80);
        let vim_prefix = vim_label.map(|mode| format!("{mode} • ")).unwrap_or_default();
        let body = format!(
            "{spin} {vim_prefix}reverse-i-search `{query}`: {preview} • Enter accept • Esc cancel • Ctrl+R older"
        );
        return Line::from(body).style(style);
    }

    if let Some(query) = app.typeahead_query.as_deref() {
        let vim_prefix = vim_label.map(|mode| format!("{mode} • ")).unwrap_or_default();
        let body = format!(
            "{spin} {vim_prefix}slash command `/{query}` • Up/Down select • Tab complete • Enter run • Esc clear"
        );
        return Line::from(body).style(style);
    }

    let scroll_hint = if app.scroll_follow {
        ""
    } else {
        " • scroll locked (Ctrl+End to follow)"
    };
    let input_hint = if app.in_flight {
        ""
    } else if app.vim.as_ref().is_some_and(|vim| vim.is_normal()) {
        " • Enter submit • i/a/I/A edit • o/O open line • . repeat"
    } else if app.vim.is_some() {
        " • Esc normal • Alt+Enter newline • Up/Down history • Ctrl+R search"
    } else {
        " • Alt+Enter newline • Up/Down history • Ctrl+R search"
    };
    let vim_prefix = vim_label.map(|mode| format!("{mode} • ")).unwrap_or_default();

    Line::from(format!("{spin} {vim_prefix}{}{scroll_hint}{input_hint}", app.status)).style(style)
}

fn render(f: &mut ratatui::Frame<'_>, app: &mut App) {
    app.sync_typeahead();
    let size = f.size();
    let max_input_lines = size.height.saturating_sub(5).max(1) as usize;
    let max_input_lines = max_input_lines.min(MAX_INPUT_VISIBLE_LINES);

    let mut typeahead = if app.typeahead_active() {
        app.typeahead_items()
    } else {
        Vec::new()
    };
    typeahead.truncate(6);
    // Inside the input box: a single separator line + N commands.
    let typeahead_height = if typeahead.is_empty() {
        0
    } else {
        typeahead.len() as u16 + 1
    };

    let estimated_input = prepare_input_render(
        &app.input,
        size.width.saturating_sub(2) as usize,
        max_input_lines,
    );
    let input_height = estimated_input.visible_line_count.max(1) as u16 + 2 + typeahead_height;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(size);

    // Header
    let header = Line::from(format!(
        "claude-rs • session {} • model {} • Ctrl+C to exit",
        app.session_id, app.model
    ))
    .style(Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(Paragraph::new(header), chunks[0]);

    // Messages
    let msg_block = Block::default().borders(Borders::ALL).title("Messages");
    f.render_widget(&msg_block, chunks[1]);

    let inner = msg_block.inner(chunks[1]);
    f.render_widget(ratatui::widgets::Clear, inner);

    let inner_w = inner.width.max(1) as usize;
    let inner_h = inner.height.max(1) as usize;

    app.last_msg_view_height = inner_h;
    app.ensure_rendered(inner_w);
    app.recompute_line_offsets();
    let total_lines = app.total_rendered_lines();
    let max_scroll = total_lines.saturating_sub(inner_h);
    if app.scroll_follow {
        app.scroll_top = max_scroll;
    } else {
        app.scroll_top = app.scroll_top.min(max_scroll);
        if app.scroll_top == max_scroll {
            // Re-enable follow when the user scrolls back to the bottom.
            app.scroll_follow = true;
        }
    }
    render_transcript(f, app, inner, app.scroll_top);

    // Input (+ Week 8: slash command typeahead inside the input box).
    let input_title = if app.in_flight {
        "Input • / for commands • Alt+Enter newline"
    } else {
        "Input • / for commands • Alt+Enter newline • Up/Down history"
    };
    let input_block = Block::default().borders(Borders::ALL).title(input_title);
    let input_inner = input_block.inner(chunks[2]);

    let typeahead_area_height = typeahead_height.min(input_inner.height.saturating_sub(1));
    let (input_area, typeahead_area) = if typeahead_area_height >= 2 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(typeahead_area_height)])
            .split(input_inner);
        (split[0], split[1])
    } else {
        (
            input_inner,
            ratatui::layout::Rect {
                x: input_inner.x,
                y: input_inner.y.saturating_add(input_inner.height),
                width: input_inner.width,
                height: 0,
            },
        )
    };

    let input_layout = prepare_input_render(
        &app.input,
        input_area.width.max(1) as usize,
        input_area.height.max(1) as usize,
    );
    f.render_widget(&input_block, chunks[2]);
    f.render_widget(Paragraph::new(input_layout.text.clone()), input_area);

    if typeahead_area.height > 0 && !typeahead.is_empty() {
        f.render_widget(ratatui::widgets::Clear, typeahead_area);

        // A top border line to visually separate suggestions from input text.
        let list_block = Block::default()
            .borders(Borders::TOP)
            .title("Commands");
        let list_inner = list_block.inner(typeahead_area);
        f.render_widget(&list_block, typeahead_area);

        let available = list_inner.height as usize;
        let visible = available.min(typeahead.len());

        let items: Vec<ListItem<'static>> = typeahead
            .iter()
            .take(visible)
            .enumerate()
            .map(|(idx, cmd)| {
                let mut item = ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("/{}", cmd.name),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(cmd.description, Style::default().fg(Color::Gray)),
                ]));
                if idx == app.typeahead_selected {
                    item = item.style(Style::default().fg(Color::Black).bg(Color::Blue));
                }
                item
            })
            .collect();

        let list = List::new(items);
        f.render_widget(list, list_inner);
    }

    let cursor_x = input_area.x + input_layout.cursor_col as u16;
    let cursor_y = input_area.y + input_layout.cursor_row as u16;
    if app.permission_prompt.is_none() {
        f.set_cursor(cursor_x, cursor_y);
    }

    // Status
    let spin = if app.in_flight {
        SPINNER_FRAMES
            .get(app.spinner_idx % SPINNER_FRAMES.len())
            .copied()
            .unwrap_or("*")
    } else {
        " "
    };
    let status = render_status_line(app, spin);
    f.render_widget(Paragraph::new(status), chunks[3]);

    // Week 4: permission modal overlay.
    if let Some(prompt) = &app.permission_prompt {
        render_permission_modal(f, prompt, size);
    }
}
fn centered_rect(percent_x: u16, percent_y: u16, r: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let percent_x = percent_x.min(100).max(1);
    let percent_y = percent_y.min(100).max(1);

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1]);

    horizontal[1]
}

fn render_permission_modal(
    f: &mut ratatui::Frame<'_>,
    prompt: &PermissionPrompt,
    area: ratatui::layout::Rect,
) {
    let popup = centered_rect(80, 45, area);
    f.render_widget(ratatui::widgets::Clear, popup);

    let title = format!("Permission required • {}", prompt.tool_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(Color::White).bg(Color::Black));

    let body = format_permission_prompt_text(prompt);
    let para = Paragraph::new(body)
        .block(block)
        .style(Style::default().fg(Color::White).bg(Color::Black));

    f.render_widget(para, popup);
}

fn format_permission_prompt_text(prompt: &PermissionPrompt) -> String {
    let mut out = String::new();
    out.push_str("Allow this tool call?\n\n");
    out.push_str(&prompt.details);
    out.push_str(&format!("\n\nTool call id: {}\n", prompt.id));
    out.push_str("\n[y] allow once  [n] deny  [a] always allow (saved)\n");
    out
}

fn transcript_from_history(history: &[Message]) -> Vec<ChatEntry> {
    let mut out: Vec<ChatEntry> = Vec::new();
    let mut tool_input_for_id: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    let mut tool_entry_for_id: HashMap<String, usize> = HashMap::new();

    for msg in history {
        match msg {
            Message::User(UserMessage { content }) => {
                let mut text_buf = String::new();

                for b in content {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text_buf.is_empty() {
                                text_buf.push('\n');
                            }
                            text_buf.push_str(text);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            if !text_buf.trim().is_empty() {
                                out.push(ChatEntry {
                                    role: Role::User,
                                    text: std::mem::take(&mut text_buf),
                                });
                            }

                            let (tool_name, tool_input) = tool_input_for_id
                                .get(tool_use_id)
                                .cloned()
                                .unwrap_or_else(|| {
                                    (
                                        format!("Tool({})", tool_use_id),
                                        serde_json::Value::Null,
                                    )
                                });

                            let md = format_tool_result_markdown(
                                &tool_name,
                                &tool_input,
                                content,
                                *is_error,
                            );

                            if let Some(idx) = tool_entry_for_id.get(tool_use_id).copied() {
                                if let Some(entry) = out.get_mut(idx) {
                                    entry.text = md;
                                }
                            } else {
                                out.push(ChatEntry {
                                    role: Role::Tool,
                                    text: md,
                                });
                            }
                        }
                        ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. } => {}
                    }
                }

                if !text_buf.trim().is_empty() {
                    out.push(ChatEntry {
                        role: Role::User,
                        text: text_buf,
                    });
                }
            }
            Message::Assistant(claude_core::types::message::AssistantMessage { content, .. }) => {
                let mut text_buf = String::new();

                for b in content {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text_buf.is_empty() {
                                text_buf.push('\n');
                            }
                            text_buf.push_str(text);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            if !text_buf.trim().is_empty() {
                                out.push(ChatEntry {
                                    role: Role::Assistant,
                                    text: std::mem::take(&mut text_buf),
                                });
                            }

                            tool_input_for_id
                                .insert(id.clone(), (name.clone(), input.clone()));

                            let md = format_tool_running_markdown(name, input);
                            out.push(ChatEntry {
                                role: Role::Tool,
                                text: md,
                            });
                            tool_entry_for_id.insert(id.clone(), out.len().saturating_sub(1));
                        }
                        ContentBlock::ToolResult { .. } | ContentBlock::Thinking { .. } => {}
                    }
                }

                if !text_buf.trim().is_empty() {
                    out.push(ChatEntry {
                        role: Role::Assistant,
                        text: text_buf,
                    });
                }
            }
        }
    }
    out
}

fn render_value_pretty(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn tool_activity_status(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "Bash" => {
            if let Some(desc) = input
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return crate::one_line_preview(desc, 120);
            }

            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if cmd.is_empty() {
                "Running Bash...".to_string()
            } else {
                format!("Running: {}", crate::one_line_preview(cmd, 120))
            }
        }
        "Read" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if path.is_empty() {
                "Reading file...".to_string()
            } else {
                format!("Reading {}", crate::one_line_preview(path, 120))
            }
        }
        "Write" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if path.is_empty() {
                "Writing file...".to_string()
            } else {
                format!("Writing {}", crate::one_line_preview(path, 120))
            }
        }
        "Edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if path.is_empty() {
                "Editing file...".to_string()
            } else {
                format!("Editing {}", crate::one_line_preview(path, 120))
            }
        }
        "Glob" => {
            let pat = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if pat.is_empty() {
                "Searching files...".to_string()
            } else {
                format!("Searching files: {}", crate::one_line_preview(pat, 120))
            }
        }
        "Grep" => {
            let pat = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if pat.is_empty() {
                "Searching...".to_string()
            } else {
                format!("Searching: {}", crate::one_line_preview(pat, 120))
            }
        }
        _ => format!("Running {tool_name}..."),
    }
}

fn permission_details(cwd: &Path, tool_name: &str, input: &serde_json::Value) -> String {
    let mut out = String::new();

    match tool_name {
        "Bash" => {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if !desc.is_empty() {
                out.push_str("Description:\n");
                out.push_str(desc);
                out.push_str("\n\n");
            }

            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("Command:\n");
            out.push_str(cmd);
        }
        "Edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("File:\n");
            out.push_str(path);

            out.push_str("\n\nDiff preview:\n");
            out.push_str(&edit_diff_preview(cwd, input));
        }
        "Write" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("File:\n");
            out.push_str(path);

            out.push_str("\n\nDiff preview:\n");
            out.push_str(&write_diff_preview(cwd, input));
        }
        _ => {
            out.push_str(&tool_input_summary_plain(tool_name, input));
        }
    }

    truncate_with_notice(&out, 18_000)
}

fn persist_always_allow_tool(settings_path: &Path, tool_name: &str) -> anyhow::Result<bool> {
    let tool_name = tool_name.trim();
    if tool_name.is_empty() {
        anyhow::bail!("tool name is empty");
    }

    let _lock = crate::lock_settings_path(settings_path)?;
    let mut root = crate::load_settings_json_object_or_empty(settings_path)?;
    let Some(obj) = root.as_object_mut() else {
        anyhow::bail!("settings root must be a JSON object: {}", settings_path.display());
    };

    let entry = obj
        .entry("alwaysAllowTools".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));

    let serde_json::Value::Array(arr) = entry else {
        anyhow::bail!(
            "\"alwaysAllowTools\" must be a JSON array in {}",
            settings_path.display()
        );
    };

    for v in arr.iter() {
        if v.as_str().is_none() {
            anyhow::bail!(
                "\"alwaysAllowTools\" must contain only strings in {}",
                settings_path.display()
            );
        }
    }

    if arr
        .iter()
        .filter_map(|v| v.as_str())
        .any(|s| s.eq_ignore_ascii_case(tool_name))
    {
        return Ok(false);
    }

    arr.push(serde_json::Value::String(tool_name.to_string()));
    crate::save_settings_json(settings_path, &root)?;
    Ok(true)
}

fn truncate_with_notice(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = crate::truncate_chars(s, max_chars);
    out.push_str("\n\n(truncated)");
    out
}

fn resolve_tool_path(cwd: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw.is_empty() {
        return cwd.to_path_buf();
    }

    let expanded = expand_tilde(raw);
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    normalize_path(&abs)
}

fn expand_tilde(input: &str) -> PathBuf {
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

/// Best-effort lexical normalization (no filesystem access).
///
/// This is not a full canonicalization, but it removes `.` and resolves `..`.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
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

fn edit_diff_preview(cwd: &Path, input: &serde_json::Value) -> String {
    const MAX_FILE_BYTES: u64 = 512 * 1024;
    const MAX_NEW_CHARS: usize = 200_000;
    const MAX_DIFF_CHARS: usize = 14_000;

    let raw_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if raw_path.is_empty() {
        return "(preview unavailable) missing required field: file_path".to_string();
    }

    let old_string = input
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let new_string = input
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let replace_all = input
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if old_string == new_string {
        return "(preview) old_string and new_string are identical; no edit to apply".to_string();
    }

    if new_string.chars().count() > MAX_NEW_CHARS {
        return "(preview unavailable) new_string is too large".to_string();
    }

    let path = resolve_tool_path(cwd, raw_path);
    if !path.starts_with(cwd) {
        return format!(
            "(preview unavailable) path is outside the working directory: {}",
            path.display()
        );
    }

    if !path.exists() {
        if old_string.is_empty() {
            let header_a = format!("a/{}", path.display());
            let header_b = format!("b/{}", path.display());
            let diff = TextDiff::from_lines("", new_string)
                .unified_diff()
                .header(&header_a, &header_b)
                .to_string();
            return truncate_with_notice(diff.trim_end(), MAX_DIFF_CHARS);
        }
        return format!("(preview) file does not exist: {}", path.display());
    }

    if old_string.is_empty() {
        return "(preview) old_string must be non-empty when editing an existing file".to_string();
    }

    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(err) => return format!("(preview unavailable) cannot stat {}: {err}", path.display()),
    };
    if meta.is_dir() {
        return format!("(preview) path is a directory: {}", path.display());
    }
    if meta.len() > MAX_FILE_BYTES {
        return format!(
            "(preview unavailable) file is too large ({} bytes)",
            meta.len()
        );
    }

    let original = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            return format!(
                "(preview unavailable) failed to read {}: {err}",
                path.display()
            )
        }
    };

    let (updated, count) = if replace_all {
        let count = original.matches(old_string).count();
        (original.replace(old_string, new_string), count)
    } else {
        match original.find(old_string) {
            Some(idx) => {
                let mut s = String::with_capacity(
                    original
                        .len()
                        .saturating_sub(old_string.len())
                        .saturating_add(new_string.len()),
                );
                s.push_str(&original[..idx]);
                s.push_str(new_string);
                s.push_str(&original[idx + old_string.len()..]);
                (s, 1)
            }
            None => (original.clone(), 0),
        }
    };

    if count == 0 {
        return format!("(preview) old_string not found in {}", path.display());
    }

    let mode = if replace_all { "all" } else { "first" };
    let header_a = format!("a/{}", path.display());
    let header_b = format!("b/{}", path.display());
    let diff = TextDiff::from_lines(&original, &updated)
        .unified_diff()
        .header(&header_a, &header_b)
        .to_string();

    let mut out = format!("(preview) would replace {mode} occurrence(s): {count}\n\n");
    out.push_str(diff.trim_end());
    truncate_with_notice(&out, MAX_DIFF_CHARS)
}

fn write_diff_preview(cwd: &Path, input: &serde_json::Value) -> String {
    const MAX_FILE_BYTES: u64 = 512 * 1024;
    const MAX_NEW_CHARS: usize = 200_000;
    const MAX_DIFF_CHARS: usize = 14_000;

    let raw_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if raw_path.is_empty() {
        return "(preview unavailable) missing required field: file_path".to_string();
    }

    let content = input.get("content").and_then(|v| v.as_str()).unwrap_or_default();
    if content.chars().count() > MAX_NEW_CHARS {
        return "(preview unavailable) content is too large".to_string();
    }

    let path = resolve_tool_path(cwd, raw_path);
    if !path.starts_with(cwd) {
        return format!(
            "(preview unavailable) path is outside the working directory: {}",
            path.display()
        );
    }

    let existed = path.exists();
    let original = if existed {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(err) => {
                return format!("(preview unavailable) cannot stat {}: {err}", path.display())
            }
        };
        if meta.is_dir() {
            return format!("(preview) path is a directory: {}", path.display());
        }
        if meta.len() > MAX_FILE_BYTES {
            return format!(
                "(preview unavailable) file is too large ({} bytes)",
                meta.len()
            );
        }

        match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) => {
                return format!(
                    "(preview unavailable) failed to read {}: {err}",
                    path.display()
                )
            }
        }
    } else {
        String::new()
    };

    if original == content {
        return "(preview) no changes".to_string();
    }

    let header_a = format!("a/{}", path.display());
    let header_b = format!("b/{}", path.display());
    let diff = TextDiff::from_lines(&original, content)
        .unified_diff()
        .header(&header_a, &header_b)
        .to_string();

    let kind = if existed { "update" } else { "create" };
    let mut out = format!("(preview) would {kind} file\n\n");
    out.push_str(diff.trim_end());
    truncate_with_notice(&out, MAX_DIFF_CHARS)
}

fn tool_primary_summary(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    match tool_name {
        "Bash" => input.get("command").and_then(|v| v.as_str()).map(|s| s.trim().to_string()),
        "Read" | "Write" | "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
        "Glob" | "Grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
        "NotebookEdit" => input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
        "WebFetch" => input.get("url").and_then(|v| v.as_str()).map(|s| s.trim().to_string()),
        "WebSearch" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn format_tool_running_markdown(tool_name: &str, input: &serde_json::Value) -> String {
    let Some(primary) = tool_primary_summary(tool_name, input) else {
        let rendered = render_value_pretty(input);
        let rendered = crate::truncate_chars(&rendered, 1200);
        return format!("Running **{tool_name}**\n\n```json\n{rendered}\n```");
    };

    if primary.contains('\n') || primary.contains('`') {
        let rendered = crate::truncate_chars(&primary, 1200);
        return format!("Running **{tool_name}**\n\n```text\n{rendered}\n```");
    }

    let preview = crate::truncate_chars(primary.trim(), 200);
    format!("Running **{tool_name}**: `{preview}`")
}

fn format_tool_result_markdown(
    tool_name: &str,
    input: &serde_json::Value,
    result: &serde_json::Value,
    is_error: bool,
) -> String {
    let mut out = String::new();
    let status = if is_error { " (error)" } else { "" };

    if let Some(primary) = tool_primary_summary(tool_name, input) {
        if !primary.contains('\n') && !primary.contains('`') {
            let preview = crate::truncate_chars(primary.trim(), 200);
            out.push_str(&format!("**{tool_name}**{status}: `{preview}`\n\n"));
        } else {
            let preview = crate::truncate_chars(primary.trim(), 1200);
            out.push_str(&format!("**{tool_name}**{status}\n\n```text\n{preview}\n```\n\n"));
        }
    } else {
        let rendered = crate::truncate_chars(&render_value_pretty(input), 1200);
        out.push_str(&format!("**{tool_name}**{status}\n\n```json\n{rendered}\n```\n\n"));
    }

    let (lang, body) = match (tool_name, result) {
        ("Edit", serde_json::Value::String(s)) => ("diff", crate::truncate_chars(s, 50_000)),
        (_name, serde_json::Value::String(s)) => ("text", crate::truncate_chars(s, 50_000)),
        (_name, other) => ("json", crate::truncate_chars(&render_value_pretty(other), 50_000)),
    };

    out.push_str("Result:\n");
    out.push_str(&format!("```{lang}\n{body}\n```"));
    out
}

fn tool_input_summary_plain(tool_name: &str, input: &serde_json::Value) -> String {
    let mut out = String::new();

    match tool_name {
        "Bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("Command:\n");
            out.push_str(cmd);
        }
        "Write" | "Edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("File:\n");
            out.push_str(path);
        }
        "NotebookEdit" => {
            let path = input
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("Notebook:\n");
            out.push_str(path);
        }
        "WebFetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or_default().trim();
            out.push_str("URL:\n");
            out.push_str(url);
        }
        "WebSearch" => {
            let q = input.get("query").and_then(|v| v.as_str()).unwrap_or_default().trim();
            out.push_str("Query:\n");
            out.push_str(q);
        }
        _ => {
            out.push_str("Input:\n");
            out.push_str(&render_value_pretty(input));
        }
    }

    crate::truncate_chars(&out, 2200)
}

fn write_session_meta_silent(session_id: SessionId, session_path: &Path, result: &claude_query::RunResult) {
    let meta_path = session_path.with_extension("meta.json");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let preview = crate::truncate_chars(&result.text, 800);

    let meta = serde_json::json!({
      "session_id": session_id.to_string(),
      "transcript_path": session_path.display().to_string(),
      "updated_at_ms": now_ms,
      "model": result.model,
      "turns": result.turns,
      "stop_reason": result.stop_reason,
      "usage": {
        "input_tokens": result.usage.input_tokens,
        "output_tokens": result.usage.output_tokens,
        "cache_creation_input_tokens": result.usage.cache_creation_input_tokens,
        "cache_read_input_tokens": result.usage.cache_read_input_tokens,
      },
      "cost_usd": result.cost_usd,
      "response_preview": preview,
    });

    let Ok(bytes) = serde_json::to_vec_pretty(&meta) else {
        return;
    };
    let _ = std::fs::write(meta_path, bytes);
}
