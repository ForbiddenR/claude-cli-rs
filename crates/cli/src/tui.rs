use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use similar::TextDiff;
use tokio::sync::{mpsc, oneshot};

use crate::args::{Args, InputFormat, OutputFormat};

mod input;
mod keymap;
mod markdown;
mod slash;
mod theme;
mod vim;

use input::{InputBuffer, PromptHistory, ReverseHistorySearch};
use keymap::{KeyAction, KeyContext, KeySequence, KeybindingResolver, ResolveOutcome};
use markdown::{MarkdownRenderer, StreamingMarkdown};
use slash::{SLASH_COMMANDS, SlashCommandDef, match_commands, parse_slash_command};
use theme::{Theme, ThemeName};
use vim::{VimHandleResult, VimKey, VimMachine, VimMode};

const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
const MAX_INPUT_VISIBLE_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Tool,
    System,
    Thinking,
}

#[derive(Debug, Clone, Copy)]
struct DisplayPrefs {
    show_thinking: bool,
    condensed: bool,
}

#[derive(Debug, Clone)]
struct ChatEntry {
    role: Role,
    kind: ChatEntryKind,
    text: String,
}

#[derive(Debug, Clone)]
enum ChatEntryKind {
    Plain,
    Tool(ToolEntry),
    Thinking(ThinkingEntry),
}

#[derive(Debug, Clone)]
struct ToolEntry {
    name: String,
    input: serde_json::Value,
    state: ToolEntryState,
}

#[derive(Debug, Clone)]
enum ToolEntryState {
    Running,
    Result {
        result: serde_json::Value,
        is_error: bool,
    },
}

#[derive(Debug, Clone)]
struct ThinkingEntry {
    thinking: String,
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
    dialog: Option<DialogState>,
    typeahead_query: Option<String>,
    typeahead_selected: usize,
    typeahead_suppressed_text: Option<String>,
    status: String,
    theme: Theme,
    toasts: Vec<Toast>,
    spinner_idx: usize,
    in_flight: bool,
    active_assistant_idx: Option<usize>,
    active_thinking_idx: Option<usize>,
    run_started_at: Option<Instant>,
    run_stream_chars: usize,
    last_turn_tokens_per_sec: Option<f64>,

    // Week 11: transcript display toggles.
    show_thinking: bool,
    condensed: bool,

    /// Bumps whenever transcript entries are added/removed or their displayed text changes.
    transcript_rev: u64,

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

    // Week 11: agent progress display.
    agents: Vec<AgentUiEntry>,

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
    /// When set, line offsets must be recomputed starting at this entry index.
    line_offsets_dirty_from: Option<usize>,

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

#[derive(Debug, Clone)]
enum DialogState {
    Onboarding(OnboardingDialog),
    ModelPicker(ModelPickerDialog),
    SessionResume(SessionResumeDialog),
    TranscriptSearch(TranscriptSearchDialog),
    AgentProgress(AgentProgressDialog),
    AgentDetail(AgentDetailDialog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
struct Toast {
    level: ToastLevel,
    message: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct OnboardingDialog;

#[derive(Debug, Clone)]
struct ModelPickerDialog {
    filter: InputBuffer,
    selected: usize,
    models: Vec<String>,
}

#[derive(Debug, Clone)]
struct SessionInfo {
    id: SessionId,
    path: PathBuf,
    updated_at_ms: u64,
    model: Option<String>,
    cost_usd: Option<f64>,
    response_preview: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionResumeDialog {
    filter: InputBuffer,
    selected: usize,
    sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone)]
struct SearchHit {
    entry_idx: usize,
    role: Role,
    preview: String,
}

#[derive(Debug, Clone)]
struct TranscriptSearchDialog {
    query: InputBuffer,
    selected: usize,
    hits: Vec<SearchHit>,
    last_query: String,
    last_transcript_len: usize,
    last_transcript_rev: u64,
}

#[derive(Debug, Clone)]
struct AgentProgressDialog {
    selected: usize,
}

#[derive(Debug, Clone)]
struct AgentDetailDialog {
    agent_tool_use_id: String,
}

#[derive(Clone)]
struct EngineInputs {
    max_tokens: u32,
    cfg: claude_query::QueryEngineConfig,
}

#[derive(Debug)]
enum QueryEvent {
    TextDelta(String),
    ThinkingDelta(String),
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
    AgentProgress {
        agent_tool_use_id: String,
        update: claude_query::AgentProgressUpdate,
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

#[derive(Debug, Clone)]
struct AgentUiEntry {
    tool_use_id: String,
    description: String,
    prompt: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    is_error: Option<bool>,
    result_preview: Option<String>,
    progress: claude_query::AgentProgressUpdate,
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

    async fn on_agent_progress(
        &self,
        agent_tool_use_id: &str,
        update: &claude_query::AgentProgressUpdate,
    ) {
        let _ = self.tx.send(QueryEvent::AgentProgress {
            agent_tool_use_id: agent_tool_use_id.to_string(),
            update: update.clone(),
        });
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        terminal::enable_raw_mode().context("enable raw mode")?;
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste
        )
        .context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}

impl RenderedEntry {
    fn new(role: Role, theme: &Theme) -> Self {
        Self {
            header: role_header(theme, role),
            body: RenderedBody::Static(Vec::new()),
            dirty: true,
        }
    }

    fn new_streaming(role: Role, theme: &Theme) -> Self {
        Self {
            header: role_header(theme, role),
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

impl ChatEntry {
    fn plain(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            kind: ChatEntryKind::Plain,
            text: text.into(),
        }
    }

    fn tool(
        name: impl Into<String>,
        input: serde_json::Value,
        state: ToolEntryState,
        prefs: DisplayPrefs,
    ) -> Self {
        let tool = ToolEntry {
            name: name.into(),
            input,
            state,
        };
        let text = render_tool_entry(&tool, prefs.condensed);
        Self {
            role: Role::Tool,
            kind: ChatEntryKind::Tool(tool),
            text,
        }
    }

    fn thinking(thinking: impl Into<String>, prefs: DisplayPrefs) -> Self {
        let entry = ThinkingEntry {
            thinking: thinking.into(),
        };
        let text = render_thinking_entry(&entry, prefs.show_thinking);
        Self {
            role: Role::Thinking,
            kind: ChatEntryKind::Thinking(entry),
            text,
        }
    }

    fn refresh_display(&mut self, prefs: DisplayPrefs) -> bool {
        let next = match &self.kind {
            ChatEntryKind::Plain => return false,
            ChatEntryKind::Tool(tool) => render_tool_entry(tool, prefs.condensed),
            ChatEntryKind::Thinking(entry) => render_thinking_entry(entry, prefs.show_thinking),
        };

        if next != self.text {
            self.text = next;
            return true;
        }
        false
    }
}

impl App {
    fn display_prefs(&self) -> DisplayPrefs {
        DisplayPrefs {
            show_thinking: self.show_thinking,
            condensed: self.condensed,
        }
    }

    fn next_transcript_rev(&mut self) {
        self.transcript_rev = self.transcript_rev.saturating_add(1);
    }

    fn append_entry(&mut self, entry: ChatEntry) {
        let role = entry.role;
        self.transcript.push(entry);
        self.rendered.push(RenderedEntry::new(role, &self.theme));
        let idx = self.transcript.len().saturating_sub(1);
        self.mark_line_offsets_dirty_from(idx);
        self.next_transcript_rev();
    }

    fn refresh_entry(&mut self, idx: usize) {
        if idx >= self.transcript.len() {
            return;
        }
        let prefs = self.display_prefs();
        if let Some(entry) = self.transcript.get_mut(idx) {
            let changed = entry.refresh_display(prefs);
            if changed {
                if let Some(cache) = self.rendered.get_mut(idx) {
                    cache.dirty = true;
                    cache.header = role_header(&self.theme, entry.role);
                }
                self.next_transcript_rev();
            }
        }
    }

    fn refresh_display_prefs(&mut self) {
        let prefs = self.display_prefs();
        let mut any_changed = false;
        for idx in 0..self.transcript.len() {
            if self.transcript[idx].refresh_display(prefs) {
                any_changed = true;
                if let Some(cache) = self.rendered.get_mut(idx) {
                    cache.dirty = true;
                    cache.header = role_header(&self.theme, self.transcript[idx].role);
                }
            }
        }
        if any_changed {
            self.next_transcript_rev();
        }
    }

    fn mark_line_offsets_dirty_from(&mut self, idx: usize) {
        match self.line_offsets_dirty_from {
            Some(existing) => self.line_offsets_dirty_from = Some(existing.min(idx)),
            None => self.line_offsets_dirty_from = Some(idx),
        }
    }

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
            // Width changes can change line wrapping across the entire transcript.
            self.mark_line_offsets_dirty_from(0);
        }

        // Defensive: keep caches aligned even if a future edit forgets to push/pop both.
        let start_len = self.rendered.len();
        while self.rendered.len() < self.transcript.len() {
            let role = self.transcript[self.rendered.len()].role;
            self.rendered.push(RenderedEntry::new(role, &self.theme));
        }
        if self.rendered.len() > start_len {
            self.mark_line_offsets_dirty_from(start_len);
        }
        if self.rendered.len() > self.transcript.len() {
            self.rendered.truncate(self.transcript.len());
            self.mark_line_offsets_dirty_from(0);
        }

        let mut line_offsets_dirty_from = self.line_offsets_dirty_from;
        for (idx, cache) in self.rendered.iter_mut().enumerate() {
            if !cache.dirty {
                continue;
            }
            let Some(entry) = self.transcript.get(idx) else {
                continue;
            };
            let before = cache.body.line_count();
            match &mut cache.body {
                RenderedBody::Static(lines) => {
                    *lines = self.md.render(&entry.text, width);
                }
                RenderedBody::Streaming(stream) => {
                    stream.update(&entry.text, &self.md, width);
                }
            }
            cache.dirty = false;
            let after = cache.body.line_count();
            if before != after {
                line_offsets_dirty_from = Some(match line_offsets_dirty_from {
                    Some(existing) => existing.min(idx),
                    None => idx,
                });
            }
        }
        self.line_offsets_dirty_from = line_offsets_dirty_from;
    }

    fn ensure_line_offsets(&mut self) {
        let entries = self.rendered.len();
        if entries == 0 {
            self.line_offsets.clear();
            self.line_offsets_dirty_from = None;
            return;
        }

        let covered_entries = self.line_offsets.len().saturating_sub(1);
        if covered_entries != entries {
            // Grow/shrink while keeping any already-correct prefix sums.
            self.line_offsets.resize(entries.saturating_add(1), 0);

            // If we grew, we can recompute only from the first newly-added entry.
            // If we shrank, rebuilding from the top is simplest and rare.
            if covered_entries < entries {
                self.mark_line_offsets_dirty_from(covered_entries);
            } else {
                self.mark_line_offsets_dirty_from(0);
            }
        }

        let Some(from) = self.line_offsets_dirty_from.take() else {
            return;
        };
        let from = from.min(entries.saturating_sub(1));

        if from == 0 {
            self.line_offsets[0] = 0;
        }

        for i in from..entries {
            let start = self.line_offsets[i];
            let height = 1usize
                .saturating_add(self.rendered[i].body.line_count())
                .saturating_add(1);
            self.line_offsets[i.saturating_add(1)] = start.saturating_add(height);
        }
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

        let before = cache.body.line_count();
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
        let after = cache.body.line_count();
        if before != after {
            self.mark_line_offsets_dirty_from(idx);
        }
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
            self.typeahead_selected = self
                .typeahead_selected
                .min(suggestions.len().saturating_sub(1));
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
        let cur = self
            .typeahead_selected
            .min(suggestions.len().saturating_sub(1)) as isize;
        let next = (cur + delta).rem_euclid(len) as usize;
        self.typeahead_selected = next;
    }

    fn accept_typeahead(&mut self) -> bool {
        let suggestions = self.typeahead_items();
        let Some(selected) = suggestions.get(
            self.typeahead_selected
                .min(suggestions.len().saturating_sub(1)),
        ) else {
            return false;
        };

        let current = self.input.as_str().trim();
        let suffix = current
            .strip_prefix('/')
            .map(|rest| {
                rest.split_once(char::is_whitespace)
                    .map(|(_, tail)| tail)
                    .unwrap_or("")
            })
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
        self.append_entry(ChatEntry::plain(Role::System, text));
        scroll_to_bottom(self);
    }

    fn push_toast(&mut self, level: ToastLevel, message: impl Into<String>, ttl: Duration) {
        let msg = message.into();
        if msg.trim().is_empty() {
            return;
        }

        let expires_at = Instant::now() + ttl;
        self.toasts.push(Toast {
            level,
            message: msg,
            expires_at,
        });

        // Avoid unbounded growth if something spams notifications.
        if self.toasts.len() > 32 {
            let drain = self.toasts.len().saturating_sub(32);
            self.toasts.drain(0..drain);
        }
    }

    fn prune_toasts(&mut self) {
        let now = Instant::now();
        self.toasts.retain(|t| t.expires_at > now);
    }

    fn agent_by_id(&self, tool_use_id: &str) -> Option<&AgentUiEntry> {
        self.agents
            .iter()
            .find(|agent| agent.tool_use_id == tool_use_id)
    }

    fn ensure_agent(
        &mut self,
        tool_use_id: &str,
        description: Option<&str>,
        prompt: Option<&str>,
    ) -> &mut AgentUiEntry {
        if let Some(idx) = self
            .agents
            .iter()
            .position(|agent| agent.tool_use_id == tool_use_id)
        {
            let agent = &mut self.agents[idx];
            if let Some(desc) = description.map(str::trim).filter(|s| !s.is_empty()) {
                agent.description = desc.to_string();
            }
            if let Some(prompt) = prompt.map(str::trim).filter(|s| !s.is_empty()) {
                agent.prompt = prompt.to_string();
            }
            return agent;
        }

        let description = description
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("sub-agent")
            .to_string();
        let prompt = prompt.unwrap_or_default().to_string();

        self.agents.push(AgentUiEntry {
            tool_use_id: tool_use_id.to_string(),
            description,
            prompt,
            started_at: Instant::now(),
            finished_at: None,
            is_error: None,
            result_preview: None,
            progress: claude_query::AgentProgressUpdate::default(),
        });
        let idx = self.agents.len().saturating_sub(1);
        &mut self.agents[idx]
    }

    fn update_agent_progress(
        &mut self,
        tool_use_id: &str,
        update: claude_query::AgentProgressUpdate,
    ) {
        let agent = self.ensure_agent(tool_use_id, None, None);
        agent.progress = update;
    }

    fn finish_agent(
        &mut self,
        tool_use_id: &str,
        input: &serde_json::Value,
        result: &serde_json::Value,
        is_error: bool,
    ) {
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let agent = self.ensure_agent(tool_use_id, description, prompt);
        agent.finished_at = Some(Instant::now());
        agent.is_error = Some(is_error);
        agent.result_preview = match result {
            serde_json::Value::String(s) => Some(crate::one_line_preview(s, 120)),
            other => serde_json::to_string(other)
                .ok()
                .map(|s| crate::one_line_preview(&s, 120)),
        };
    }

    fn prune_agents(&mut self) {
        let now = Instant::now();
        // Keep running agents indefinitely. For completed agents, keep a bounded, recent set so
        // `/agents` remains useful without growing unbounded.
        self.agents.retain(|agent| {
            agent
                .finished_at
                .map(|finished| now.duration_since(finished) < Duration::from_secs(600))
                .unwrap_or(true)
        });

        let max_finished = 40usize;
        let mut finished: Vec<(String, Instant)> = self
            .agents
            .iter()
            .filter_map(|agent| agent.finished_at.map(|t| (agent.tool_use_id.clone(), t)))
            .collect();
        if finished.len() > max_finished {
            finished.sort_by(|a, b| a.1.cmp(&b.1)); // oldest first
            let drop_n = finished.len().saturating_sub(max_finished);
            let mut drop_ids: HashSet<String> = HashSet::new();
            for (id, _) in finished.into_iter().take(drop_n) {
                drop_ids.insert(id);
            }
            if !drop_ids.is_empty() {
                self.agents
                    .retain(|agent| !drop_ids.contains(&agent.tool_use_id));
            }
        }

        let Some(dialog) = self.dialog.take() else {
            return;
        };

        match dialog {
            DialogState::AgentDetail(detail) => {
                if self.agent_by_id(&detail.agent_tool_use_id).is_some() {
                    self.dialog = Some(DialogState::AgentDetail(detail));
                } else {
                    self.status = "ready".to_string();
                }
            }
            DialogState::AgentProgress(mut dialog) => {
                if self.agents.is_empty() {
                    self.status = "ready".to_string();
                    return;
                }
                dialog.selected = dialog.selected.min(self.agents.len().saturating_sub(1));
                self.dialog = Some(DialogState::AgentProgress(dialog));
            }
            other => {
                self.dialog = Some(other);
            }
        }
    }

    fn apply_theme(&mut self, theme: Theme) {
        self.theme = theme;

        // Update per-entry cached headers so role colors match the new theme.
        let n = self.rendered.len().min(self.transcript.len());
        for idx in 0..n {
            let role = self.transcript[idx].role;
            self.rendered[idx].header = role_header(&self.theme, role);
        }
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
        self.agents.clear();
        self.permission_prompt = None;
        self.active_assistant_idx = None;
        self.active_thinking_idx = None;
        self.in_flight = false;
        self.run_started_at = None;
        self.run_stream_chars = 0;
        self.dismiss_typeahead();
        self.scroll_top = 0;
        self.scroll_follow = true;
        self.line_offsets.clear();
        self.line_offsets_dirty_from = Some(0);
        self.next_transcript_rev();
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

    fn switch_to_session(
        &mut self,
        session_id: SessionId,
        session_path: PathBuf,
        history: Vec<Message>,
        status: impl Into<String>,
        note: impl Into<String>,
    ) {
        self.session_id = session_id;
        self.session_path = session_path;
        self.history = history;
        self.prompt_history = PromptHistory::new(prompt_history_from_messages(&self.history));
        self.prompt_history.reset_navigation();
        self.reverse_history_search = None;
        self.input.clear();
        if let Some(vim) = self.vim.as_mut() {
            vim.reset_insert_tracking();
        }
        self.permission_prompt = None;
        self.active_assistant_idx = None;
        self.active_thinking_idx = None;
        self.tool_entry_for_id.clear();
        self.agents.clear();
        self.dismiss_typeahead();
        self.dialog = None;
        self.in_flight = false;
        self.run_started_at = None;
        self.run_stream_chars = 0;
        self.transcript = transcript_from_history(&self.history, self.display_prefs());
        if self.transcript.is_empty() {
            self.transcript.push(ChatEntry::plain(
                Role::System,
                "Ctrl+C to exit. Type a prompt and press Enter.",
            ));
        }
        self.rendered = self
            .transcript
            .iter()
            .map(|entry| RenderedEntry::new(entry.role, &self.theme))
            .collect();
        self.render_width = 0;
        self.scroll_top = 0;
        self.scroll_follow = true;
        self.line_offsets.clear();
        self.line_offsets_dirty_from = Some(0);
        self.transcript_rev = self.transcript_rev.saturating_add(1);
        self.push_system_message(note);
        self.status = status.into();
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
    let typed = rest
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if typed.is_empty() {
        return None;
    }
    if SLASH_COMMANDS.iter().any(|cmd| cmd.name == typed) {
        return Some(typed);
    }

    let suggestions = app.typeahead_items();
    suggestions
        .get(
            app.typeahead_selected
                .min(suggestions.len().saturating_sub(1)),
        )
        .map(|cmd| cmd.name.to_string())
}

fn slash_command_help() -> String {
    let mut out = String::new();
    out.push_str("Slash commands\n\n");
    for cmd in SLASH_COMMANDS {
        out.push_str(&format!(
            "- `{}`: {} ({})\n",
            cmd.name, cmd.description, cmd.usage
        ));
    }
    out.push_str("\nUseful shortcuts\n\n");
    out.push_str("- `Ctrl+L`: start a new empty session\n");
    out.push_str("- `Ctrl+X Ctrl+K`: compact the current chat\n");
    out.push_str("- `Ctrl+F`: search transcript\n");
    out.push_str("- `Ctrl+X Ctrl+C`: exit\n");
    out.push_str("- `Tab`: accept the selected slash command\n");
    out.push_str("\nConfig\n\n");
    out.push_str(
        "- Override shortcuts via `tuiKeybindings` in settings JSON (user/project/local).\n",
    );
    out
}

fn role_header(theme: &Theme, role: Role) -> Line<'static> {
    let (label, style) = match role {
        Role::User => ("You", theme.role_user),
        Role::Assistant => ("Claude", theme.role_assistant),
        Role::Tool => ("Tool", theme.role_tool),
        Role::System => ("System", theme.role_system),
        Role::Thinking => ("Thinking", theme.role_thinking),
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
        return Err(crate::UsageError("TUI mode requires --output-format text".to_string()).into());
    }
    if !matches!(args.input_format, InputFormat::Text) {
        return Err(crate::UsageError("TUI mode requires --input-format text".to_string()).into());
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
    // Week 9: transcript search.
    keymap.add_binding(
        &[KeyContext::Global],
        KeySequence::parse("ctrl+f")?,
        KeyAction::SearchTranscript,
    );
    // Week 11: agent progress display.
    keymap.add_binding(
        &[KeyContext::Global],
        KeySequence::parse("ctrl+g")?,
        KeyAction::ShowAgents,
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

    let show_thinking = settings.tui_show_thinking.unwrap_or(false);
    let condensed = settings.tui_condensed.unwrap_or(false);
    let display_prefs = DisplayPrefs {
        show_thinking,
        condensed,
    };

    let mut transcript = transcript_from_history(&history, display_prefs);
    if transcript.is_empty() {
        transcript.push(ChatEntry::plain(
            Role::System,
            "Ctrl+C to exit. Type a prompt and press Enter.",
        ));
    }
    if !keymap_warnings.is_empty() {
        transcript.push(ChatEntry::plain(
            Role::System,
            format!(
                "warn: some tuiKeybindings entries were ignored:\n- {}",
                keymap_warnings.join("\n- ")
            ),
        ));
    }

    let theme_name = settings
        .tui_theme
        .as_deref()
        .and_then(ThemeName::parse)
        .unwrap_or(ThemeName::Dark);
    let theme = Theme::new(theme_name);

    let rendered = transcript
        .iter()
        .map(|e| RenderedEntry::new(e.role, &theme))
        .collect();

    let client = claude_services::api::AnthropicClient::new(None);
    let engine_inputs = compute_engine_inputs(args, settings)?;
    let engine = std::sync::Arc::new(claude_query::QueryEngine::new(
        client.clone(),
        auth.clone(),
        model.clone(),
        engine_inputs.max_tokens,
        engine_inputs.cfg.clone(),
    )?);
    let show_onboarding = !settings.tui_onboarding_seen.unwrap_or(false);

    let mut app = App {
        input: InputBuffer::new(),
        prompt_history,
        reverse_history_search: None,
        vim,
        keymap,
        dialog: show_onboarding.then_some(DialogState::Onboarding(OnboardingDialog)),
        typeahead_query: None,
        typeahead_selected: 0,
        typeahead_suppressed_text: None,
        status: "ready".to_string(),
        theme: theme.clone(),
        toasts: Vec::new(),
        spinner_idx: 0,
        in_flight: false,
        active_assistant_idx: None,
        active_thinking_idx: None,
        run_started_at: None,
        run_stream_chars: 0,
        last_turn_tokens_per_sec: None,
        show_thinking,
        condensed,
        transcript_rev: 1,
        transcript,
        rendered,
        render_width: 0,
        md,
        history,
        tool_entry_for_id: HashMap::new(),
        permission_prompt: None,
        always_allow_tools,
        agents: Vec::new(),
        scroll_top: 0,
        scroll_follow: true,
        last_msg_view_height: 0,
        line_offsets: Vec::new(),
        line_offsets_dirty_from: Some(0),
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
        terminal.draw(|f| render(f, &mut app)).context("render")?;

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
                app.prune_toasts();
                app.prune_agents();
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
            if let Some(dialog) = app.dialog.as_mut() {
                dialog_paste(dialog, &text);
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
                                    app.status =
                                        format!("always allow {} (saved)", prompt.tool_name);
                                    app.push_toast(
                                        ToastLevel::Info,
                                        format!("Always allow saved: {}", prompt.tool_name),
                                        Duration::from_secs(3),
                                    );
                                }
                                Ok(false) => {
                                    app.status = format!(
                                        "always allow {} (already saved)",
                                        prompt.tool_name
                                    );
                                    app.push_toast(
                                        ToastLevel::Info,
                                        format!("Always allow already set: {}", prompt.tool_name),
                                        Duration::from_secs(3),
                                    );
                                }
                                Err(err) => {
                                    app.status = format!(
                                        "always allow {} (save failed: {})",
                                        prompt.tool_name,
                                        crate::one_line_preview(&err.to_string(), 120)
                                    );
                                    app.push_toast(
                                        ToastLevel::Warn,
                                        "Failed to save always-allow",
                                        Duration::from_secs(4),
                                    );
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

            if app.dialog.is_some() {
                if handle_dialog_key(app, key, query_tx.clone()).await? {
                    return Ok(true);
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
        KeyAction::ShowModelPicker => {
            if app.in_flight {
                app.status = "busy; wait for the current run to finish".to_string();
            } else {
                open_model_picker(app)?;
            }
        }
        KeyAction::ResumeSession => {
            if app.in_flight {
                app.status = "busy; wait for the current run to finish".to_string();
            } else {
                open_session_resume(app)?;
            }
        }
        KeyAction::SearchTranscript => {
            open_transcript_search(app, None);
        }
        KeyAction::ShowAgents => {
            open_agent_progress_dialog(app);
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
                open_model_picker(app)?;
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
                    app.push_toast(
                        ToastLevel::Info,
                        format!("Model: {next}"),
                        Duration::from_secs(3),
                    );
                    app.status = format!("model {}", app.model);
                }
            }
        }
        "theme" => {
            if args.is_empty() {
                let names = Theme::available_names().join(", ");
                app.push_system_message(format!(
                    "Theme\n\n- current: `{}`\n- available: {names}\n\nUsage: /theme [dark|light]",
                    app.theme.name.as_str()
                ));
                app.status = format!("theme {}", app.theme.name.as_str());
            } else {
                let raw = args.join(" ");
                let raw = raw.trim();
                let Some(name) = ThemeName::parse(raw) else {
                    let names = Theme::available_names().join(", ");
                    app.push_system_message(format!(
                        "Unknown theme `{}`.\n\nAvailable: {names}\n\nUsage: /theme [dark|light]",
                        crate::one_line_preview(raw, 80)
                    ));
                    app.status = "theme usage".to_string();
                    return Ok(false);
                };

                if name == app.theme.name {
                    app.status = format!("theme {}", name.as_str());
                    return Ok(false);
                }

                app.apply_theme(Theme::new(name));
                if let Err(err) = persist_tui_theme(&app.user_settings_path, name.as_str()) {
                    app.push_system_message(format!("warn: failed to persist theme: {err}"));
                }
                app.push_toast(
                    ToastLevel::Info,
                    format!("Theme: {}", name.as_str()),
                    Duration::from_secs(3),
                );
                app.status = format!("theme {}", name.as_str());
            }
        }
        "status" => {
            let always_allow = match app.always_allow_tools.lock() {
                Ok(set) => {
                    let mut items = set.iter().cloned().collect::<Vec<_>>();
                    items.sort();
                    items
                }
                Err(_) => Vec::new(),
            };

            let last_tps = app
                .last_turn_tokens_per_sec
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "unavailable".to_string());

            let total_cost = app
                .total_cost_usd
                .map(|v| format!("${v:.4}"))
                .unwrap_or_else(|| "unavailable".to_string());

            let body = format!(
                "Status\n\n\
- session: `{}`\n\
- session file: `{}`\n\
- cwd: `{}`\n\
- model: `{}`\n\
- theme: `{}`\n\
- condensed: `{}`\n\
- thinking: `{}`\n\
- mode: `{}`\n\
- in flight: `{}`\n\
- scroll: `{}`\n\
- permissionMode: `{:?}`\n\
- alwaysAllowTools: {}{}\n\
- totals: in={} out={} cost={}\n\
- last turn: in={} out={} cost={} t/s={}\n",
                app.session_id,
                app.session_path.display(),
                app.cwd.display(),
                app.model,
                app.theme.name.as_str(),
                app.condensed,
                app.show_thinking,
                if app.vim.is_some() { "vim" } else { "normal" },
                app.in_flight,
                if app.scroll_follow {
                    "follow".to_string()
                } else {
                    format!("locked (top line {})", app.scroll_top)
                },
                app.engine_inputs.cfg.permission_mode,
                always_allow.len(),
                if always_allow.is_empty() {
                    "".to_string()
                } else {
                    format!(" ({})", always_allow.join(", "))
                },
                app.total_input_tokens,
                app.total_output_tokens,
                total_cost,
                app.last_turn_input_tokens,
                app.last_turn_output_tokens,
                app.last_turn_cost_usd
                    .map(|v| format!("${v:.4}"))
                    .unwrap_or_else(|| "unavailable".to_string()),
                last_tps,
            );

            app.push_system_message(body);
            app.status = "status".to_string();
        }
        "thinking" => {
            let mut desired: Option<bool> = None;
            if !args.is_empty() {
                let raw = args.join(" ").trim().to_ascii_lowercase();
                match raw.as_str() {
                    "on" | "show" | "true" | "1" => desired = Some(true),
                    "off" | "hide" | "false" | "0" => desired = Some(false),
                    _ => {
                        app.push_system_message("usage: /thinking [on|off]");
                        app.status = "thinking usage".to_string();
                        return Ok(false);
                    }
                }
            }

            if args.is_empty() {
                app.push_system_message(format!(
                    "Thinking\n\n- showThinking: `{}`\n\nUsage: /thinking [on|off]",
                    app.show_thinking
                ));
                app.status = format!("thinking {}", app.show_thinking);
                return Ok(false);
            }

            let next = desired.unwrap_or(!app.show_thinking);
            if next != app.show_thinking {
                app.show_thinking = next;
                app.refresh_display_prefs();
                if let Err(err) = persist_tui_show_thinking(&app.user_settings_path, next) {
                    app.push_system_message(format!("warn: failed to persist thinking: {err}"));
                }
                app.push_toast(
                    ToastLevel::Info,
                    format!("Thinking: {}", if next { "shown" } else { "hidden" }),
                    Duration::from_secs(3),
                );
            }
            app.status = format!("thinking {}", app.show_thinking);
        }
        "condensed" => {
            let mut desired: Option<bool> = None;
            if !args.is_empty() {
                let raw = args.join(" ").trim().to_ascii_lowercase();
                match raw.as_str() {
                    "on" | "true" | "1" => desired = Some(true),
                    "off" | "false" | "0" => desired = Some(false),
                    _ => {
                        app.push_system_message("usage: /condensed [on|off]");
                        app.status = "condensed usage".to_string();
                        return Ok(false);
                    }
                }
            }

            if args.is_empty() {
                app.push_system_message(format!(
                    "Condensed Mode\n\n- condensed: `{}`\n\nUsage: /condensed [on|off]",
                    app.condensed
                ));
                app.status = format!("condensed {}", app.condensed);
                return Ok(false);
            }

            let next = desired.unwrap_or(!app.condensed);
            if next != app.condensed {
                app.condensed = next;
                app.refresh_display_prefs();
                if let Err(err) = persist_tui_condensed(&app.user_settings_path, next) {
                    app.push_system_message(format!("warn: failed to persist condensed: {err}"));
                }
                app.push_toast(
                    ToastLevel::Info,
                    format!("Condensed: {}", if next { "on" } else { "off" }),
                    Duration::from_secs(3),
                );
            }
            app.status = format!("condensed {}", app.condensed);
        }
        "agents" => {
            open_agent_progress_dialog(app);
        }
        "voice" => {
            app.push_system_message(
                "Voice input is not implemented in the Rust TUI yet.\n\n\
Planned behavior (Week 11): hold Space to talk, then send the transcript.\n\
Current status: feature stub only.",
            );
            app.status = "voice".to_string();
        }
        "vim" => {
            let mut desired: Option<bool> = None;
            if !args.is_empty() {
                let raw = args.join(" ").trim().to_ascii_lowercase();
                match raw.as_str() {
                    "on" | "enable" | "enabled" => desired = Some(true),
                    "off" | "disable" | "disabled" => desired = Some(false),
                    _ => {
                        app.push_system_message("usage: /vim [on|off]");
                        app.status = "vim usage".to_string();
                        return Ok(false);
                    }
                }
            }

            let next_on = desired.unwrap_or(app.vim.is_none());
            if next_on {
                if app.vim.is_none() {
                    app.vim = Some(VimMachine::new());
                    app.push_toast(ToastLevel::Info, "Vim mode enabled", Duration::from_secs(3));
                }
                app.status = "vim on".to_string();
            } else {
                if app.vim.take().is_some() {
                    app.push_toast(
                        ToastLevel::Info,
                        "Vim mode disabled",
                        Duration::from_secs(3),
                    );
                }
                app.status = "vim off".to_string();
            }
        }
        "permissions" => {
            let always_allow = match app.always_allow_tools.lock() {
                Ok(set) => {
                    let mut items = set.iter().cloned().collect::<Vec<_>>();
                    items.sort();
                    items
                }
                Err(_) => Vec::new(),
            };

            let body = format!(
                "Permissions\n\n\
- permissionMode: `{:?}`\n\
- alwaysAllowTools: {}{}\n\n\
Notes\n\n\
- Press `a` in a permission prompt to persist an always-allow rule.\n\
- Or edit your user settings JSON and set `alwaysAllowTools`.\n",
                app.engine_inputs.cfg.permission_mode,
                always_allow.len(),
                if always_allow.is_empty() {
                    "".to_string()
                } else {
                    format!(" ({})", always_allow.join(", "))
                },
            );
            app.push_system_message(body);
            app.status = "permissions".to_string();
        }
        "resume" => {
            if app.in_flight {
                app.status = "cannot resume while a run is active".to_string();
            } else if args.is_empty() {
                if let Err(err) = open_session_resume(app) {
                    app.status = "resume failed".to_string();
                    app.push_system_message(format!("error: failed to open session resume: {err}"));
                }
            } else {
                let joined = args.join(" ");
                let raw = joined.trim();
                if raw.is_empty() {
                    if let Err(err) = open_session_resume(app) {
                        app.status = "resume failed".to_string();
                        app.push_system_message(format!(
                            "error: failed to open session resume: {err}"
                        ));
                    }
                } else {
                    match raw.parse::<SessionId>() {
                        Ok(id) => {
                            if let Err(err) = resume_session_by_id(app, id) {
                                app.status = "resume failed".to_string();
                                app.push_system_message(format!(
                                    "error: failed to resume session {id}: {err}"
                                ));
                            }
                        }
                        Err(_) => {
                            app.status = "resume usage".to_string();
                            app.push_system_message("usage: /resume [session-id]");
                        }
                    }
                }
            }
        }
        "search" => {
            let query = args.join(" ");
            open_transcript_search(app, (!query.trim().is_empty()).then_some(query));
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
                            let session_path =
                                match claude_core::history::session_file_path(&cwd, session_id) {
                                    Ok(path) => path,
                                    Err(err) => {
                                        let _ = query_tx
                                            .send(QueryEvent::CompactionError(err.to_string()));
                                        return;
                                    }
                                };

                            // Best-effort persistence so `--continue/--resume` sees the compacted context.
                            let _ = claude_core::history::append_session_messages(
                                &session_path,
                                &compacted,
                            );

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
            let known = SLASH_COMMANDS
                .iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(", ");
            app.push_system_message(format!(
                "Unknown slash command `{other}`. Available: {known}"
            ));
            app.status = format!("unknown command `{other}`");
        }
    }

    Ok(false)
}

fn submit_prompt(app: &mut App, query_tx: mpsc::UnboundedSender<QueryEvent>) -> anyhow::Result<()> {
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
    app.active_thinking_idx = None;

    app.append_entry(ChatEntry::plain(Role::User, prompt.clone()));

    // Week 3: make sure the new turn is visible even if the user had scrolled up.
    scroll_to_bottom(app);

    app.status = "thinking...".to_string();
    app.in_flight = true;
    app.spinner_idx = 0;
    app.run_started_at = Some(Instant::now());
    app.run_stream_chars = 0;

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
            std::sync::Arc::new(TuiObserver {
                tx: query_tx.clone(),
                always_allow_tools,
            });

        let res = engine
            .run_with_history_observed(
                history_for_engine,
                |event| {
                    if let Some(text) = crate::extract_text_delta(event) {
                        let _ = tx_for_deltas.send(QueryEvent::TextDelta(text.to_string()));
                    }
                    if let Some(thinking) = crate::extract_thinking_delta(event) {
                        let _ = tx_for_deltas.send(QueryEvent::ThinkingDelta(thinking.to_string()));
                    }
                    Ok(())
                },
                observer,
            )
            .await;

        match res {
            Ok(result) => {
                if !result.new_messages.is_empty() {
                    let _ = claude_core::history::append_session_messages(
                        &session_path,
                        &result.new_messages,
                    );
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
            // If the model streamed a thinking block first, it typically ends before text starts.
            if let Some(idx) = app.active_thinking_idx.take() {
                app.refresh_entry(idx);
            }

            let delta_chars = delta.chars().count();
            let idx = match app.active_assistant_idx {
                Some(idx) => idx,
                None => {
                    app.transcript
                        .push(ChatEntry::plain(Role::Assistant, String::new()));
                    app.rendered
                        .push(RenderedEntry::new_streaming(Role::Assistant, &app.theme));
                    let idx = app.transcript.len().saturating_sub(1);
                    app.active_assistant_idx = Some(idx);
                    app.next_transcript_rev();
                    idx
                }
            };
            if let Some(entry) = app.transcript.get_mut(idx) {
                entry.text.push_str(&delta);
            }
            if let Some(cache) = app.rendered.get_mut(idx) {
                cache.dirty = true;
            }
            app.run_stream_chars = app.run_stream_chars.saturating_add(delta_chars);
        }
        QueryEvent::ThinkingDelta(delta) => {
            let idx = match app.active_thinking_idx {
                Some(idx) => idx,
                None => {
                    let entry = ChatEntry::thinking(String::new(), app.display_prefs());
                    app.append_entry(entry);
                    let idx = app.transcript.len().saturating_sub(1);
                    app.active_thinking_idx = Some(idx);
                    idx
                }
            };

            let prefs = app.display_prefs();
            let show_thinking = app.show_thinking;

            if let Some(entry) = app.transcript.get_mut(idx) {
                match &mut entry.kind {
                    ChatEntryKind::Thinking(t) => t.thinking.push_str(&delta),
                    other => {
                        // Defensive: preserve previous displayed text, but ensure kind matches.
                        let t = ThinkingEntry { thinking: delta };
                        let text = render_thinking_entry(&t, app.show_thinking);
                        *other = ChatEntryKind::Thinking(t);
                        entry.role = Role::Thinking;
                        entry.text = text;
                    }
                }

                // Only refresh the rendered markdown when thinking is visible; otherwise keep the
                // placeholder stable and refresh at block boundaries / completion.
                if show_thinking {
                    entry.refresh_display(prefs);
                    if let Some(cache) = app.rendered.get_mut(idx) {
                        cache.dirty = true;
                    }
                    app.next_transcript_rev();
                }
            }
        }
        QueryEvent::PermissionRequest {
            id,
            name,
            input,
            reply_tx,
        } => {
            if let Some(idx) = app.active_thinking_idx.take() {
                app.refresh_entry(idx);
            }
            // The assistant turn that requested tool use has finished streaming at this point.
            if let Some(idx) = app.active_assistant_idx.take() {
                app.finalize_streaming(idx);
            }
            // Permission prompts take over input focus.
            app.dialog = None;
            app.keymap.clear_pending();
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
            if let Some(idx) = app.active_thinking_idx.take() {
                app.refresh_entry(idx);
            }
            if let Some(idx) = app.active_assistant_idx.take() {
                app.finalize_streaming(idx);
            }

            let entry = ChatEntry::tool(
                name.clone(),
                input.clone(),
                ToolEntryState::Running,
                app.display_prefs(),
            );
            app.append_entry(entry);
            let idx = app.transcript.len().saturating_sub(1);
            let tool_use_id = id.clone();
            app.tool_entry_for_id.insert(id, idx);
            app.status = tool_activity_status(&name, &input);

            if name == "Agent" {
                let desc = input
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let prompt = input
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                app.ensure_agent(&tool_use_id, desc, prompt);

                if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                    let preview = crate::one_line_preview(desc, 60);
                    app.push_toast(
                        ToastLevel::Info,
                        format!("Agent started: {preview}"),
                        Duration::from_secs(3),
                    );
                }
            }
        }
        QueryEvent::ToolUseResult {
            id,
            name,
            input,
            result,
            is_error,
        } => {
            let state = ToolEntryState::Result {
                result: result.clone(),
                is_error,
            };
            let prefs = app.display_prefs();

            if let Some(idx) = app.tool_entry_for_id.remove(&id) {
                if let Some(entry) = app.transcript.get_mut(idx) {
                    entry.role = Role::Tool;
                    match &mut entry.kind {
                        ChatEntryKind::Tool(tool) => {
                            tool.name = name.clone();
                            tool.input = input.clone();
                            tool.state = state.clone();
                            entry.refresh_display(prefs);
                        }
                        other => {
                            *other = ChatEntryKind::Tool(ToolEntry {
                                name: name.clone(),
                                input: input.clone(),
                                state,
                            });
                            entry.refresh_display(prefs);
                        }
                    }
                }
                if let Some(cache) = app.rendered.get_mut(idx) {
                    cache.dirty = true;
                }
                app.next_transcript_rev();
            } else {
                let entry =
                    ChatEntry::tool(name.clone(), input.clone(), state, app.display_prefs());
                app.append_entry(entry);
            }
            app.status = format!("{name} done");

            if name == "Agent" {
                app.finish_agent(&id, &input, &result, is_error);
                if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                    let preview = crate::one_line_preview(desc, 60);
                    app.push_toast(
                        ToastLevel::Info,
                        format!("Agent finished: {preview}"),
                        Duration::from_secs(3),
                    );
                }
            }
        }
        QueryEvent::AgentProgress {
            agent_tool_use_id,
            update,
        } => {
            app.update_agent_progress(&agent_tool_use_id, update);
        }
        QueryEvent::Finished(result) => {
            let finished_idx = app.active_assistant_idx;
            app.in_flight = false;
            app.active_assistant_idx = None;
            if let Some(idx) = app.active_thinking_idx.take() {
                app.refresh_entry(idx);
            }
            app.permission_prompt = None;
            let elapsed = app.run_started_at.take().map(|t| t.elapsed());
            app.last_turn_tokens_per_sec = elapsed.and_then(|d| {
                let secs = d.as_secs_f64();
                if secs <= 0.0 {
                    return None;
                }
                Some(result.usage.output_tokens as f64 / secs)
            });
            app.run_stream_chars = 0;
            app.record_run_cost(&result);
            app.history = result.history;
            if let Some(idx) = finished_idx {
                app.finalize_streaming(idx);
            }
            app.next_transcript_rev();

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
            app.switch_to_session(
                session_id,
                session_path,
                history,
                "compaction done",
                format!("Compaction complete • new session {}", session_id),
            );
            app.push_toast(
                ToastLevel::Info,
                "Compaction complete",
                Duration::from_secs(3),
            );
        }
        QueryEvent::CompactionError(err) => {
            app.in_flight = false;
            app.run_started_at = None;
            app.run_stream_chars = 0;
            app.status = format!("compact failed: {}", crate::one_line_preview(&err, 160));
            app.push_system_message(format!("error: compaction failed: {err}"));
            app.push_toast(
                ToastLevel::Error,
                "Compaction failed",
                Duration::from_secs(4),
            );
        }
        QueryEvent::Error(err) => {
            app.in_flight = false;
            app.run_started_at = None;
            app.run_stream_chars = 0;
            app.active_assistant_idx = None;
            app.active_thinking_idx = None;
            app.permission_prompt = None;

            // If we created an empty assistant entry for streaming, remove it on error.
            if let Some(last) = app.transcript.last() {
                if last.role == Role::Assistant && last.text.is_empty() {
                    app.transcript.pop();
                    app.rendered.pop();
                    app.next_transcript_rev();
                }
            }

            app.status = format!("error: {}", crate::one_line_preview(&err, 160));
            app.append_entry(ChatEntry::plain(Role::System, format!("error: {err}")));
            app.push_toast(ToastLevel::Error, "Error", Duration::from_secs(4));
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

fn prepare_input_render(
    input: &InputBuffer,
    width: usize,
    max_lines: usize,
    theme: &Theme,
) -> PreparedInput {
    let width = width.max(1);
    let selection = input.selection_range();
    let mut raw_lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut row: usize = 0;
    let mut col: usize = 0;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = 0;

    let plain = Style::default();
    let selected = theme.selection;

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
    let style = app.theme.status;
    let vim_label = app.vim.as_ref().map(|vim| match vim.mode() {
        VimMode::Insert => "-- INSERT --",
        VimMode::Normal => "-- NORMAL --",
    });

    if let Some(search) = &app.reverse_history_search {
        let query = search.query();
        let preview = crate::one_line_preview(app.input.as_str(), 80);
        let vim_prefix = vim_label
            .map(|mode| format!("{mode} • "))
            .unwrap_or_default();
        let body = format!(
            "{spin} {vim_prefix}reverse-i-search `{query}`: {preview} • Enter accept • Esc cancel • Ctrl+R older"
        );
        return Line::from(body).style(style);
    }

    if let Some(query) = app.typeahead_query.as_deref() {
        let vim_prefix = vim_label
            .map(|mode| format!("{mode} • "))
            .unwrap_or_default();
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
    let mode = if let Some(vim) = app.vim.as_ref() {
        match vim.mode() {
            VimMode::Insert => "INSERT",
            VimMode::Normal => "NORMAL",
        }
    } else {
        "EDIT"
    };

    let cost_total = match app.total_cost_usd {
        Some(v) => format!("${v:.4}"),
        None => "unavailable".to_string(),
    };

    let tokens_per_sec = if app.in_flight {
        match app.run_started_at {
            Some(start) => {
                let secs = start.elapsed().as_secs_f64();
                if secs <= 0.0 {
                    "t/s --".to_string()
                } else {
                    // We don't know token counts mid-stream; estimate ~4 chars/token.
                    let approx_tokens = (app.run_stream_chars as f64) / 4.0;
                    format!("~{:.1} t/s", approx_tokens / secs)
                }
            }
            None => "t/s --".to_string(),
        }
    } else {
        app.last_turn_tokens_per_sec
            .map(|v| format!("{v:.1} t/s"))
            .unwrap_or_else(|| "t/s --".to_string())
    };

    let metrics = format!(
        "{mode} • {} • in={} out={} • tot {cost_total} • {tokens_per_sec}",
        app.model, app.total_input_tokens, app.total_output_tokens,
    );

    let agent_hint = active_agent_hint(app)
        .map(|hint| format!(" • {hint}"))
        .unwrap_or_default();

    Line::from(format!(
        "{spin} {metrics} • {}{agent_hint}{scroll_hint}{input_hint}",
        app.status,
    ))
    .style(style)
}

fn active_agent_hint(app: &App) -> Option<String> {
    let mut labels = Vec::new();
    for entry in &app.transcript {
        let ChatEntryKind::Tool(tool) = &entry.kind else {
            continue;
        };
        if tool.name != "Agent" {
            continue;
        }
        if !matches!(tool.state, ToolEntryState::Running) {
            continue;
        }

        let label = tool
            .input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| crate::one_line_preview(s, 32))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sub-agent".to_string());
        labels.push(label);
    }

    if labels.is_empty() {
        return None;
    }

    if labels.len() == 1 {
        return Some(format!("agent {} (Ctrl+G)", labels[0]));
    }

    Some(format!(
        "agents {} (+{}) (Ctrl+G)",
        labels[0],
        labels.len() - 1
    ))
}

fn render(f: &mut ratatui::Frame<'_>, app: &mut App) {
    app.sync_typeahead();
    if let Some(DialogState::TranscriptSearch(dialog)) = app.dialog.as_mut() {
        let q = dialog.query.as_str().trim().to_ascii_lowercase();
        if q != dialog.last_query
            || dialog.last_transcript_len != app.transcript.len()
            || dialog.last_transcript_rev != app.transcript_rev
        {
            recompute_search_hits(dialog, &app.transcript, app.transcript_rev);
        }
    }
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
        &app.theme,
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
    .style(app.theme.header);
    f.render_widget(Paragraph::new(header), chunks[0]);

    // Messages
    let msg_block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border)
        .title("Messages");
    f.render_widget(&msg_block, chunks[1]);

    let inner = msg_block.inner(chunks[1]);
    f.render_widget(ratatui::widgets::Clear, inner);

    let inner_w = inner.width.max(1) as usize;
    let inner_h = inner.height.max(1) as usize;

    app.last_msg_view_height = inner_h;
    app.ensure_rendered(inner_w);
    app.ensure_line_offsets();
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
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border)
        .title(input_title);
    let input_inner = input_block.inner(chunks[2]);

    let typeahead_area_height = typeahead_height.min(input_inner.height.saturating_sub(1));
    let (input_area, typeahead_area) = if typeahead_area_height >= 2 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(typeahead_area_height),
            ])
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

    let PreparedInput {
        text: input_text,
        cursor_row,
        cursor_col,
        visible_line_count: _,
    } = prepare_input_render(
        &app.input,
        input_area.width.max(1) as usize,
        input_area.height.max(1) as usize,
        &app.theme,
    );
    f.render_widget(&input_block, chunks[2]);
    f.render_widget(Paragraph::new(input_text), input_area);

    if typeahead_area.height > 0 && !typeahead.is_empty() {
        f.render_widget(ratatui::widgets::Clear, typeahead_area);

        // A top border line to visually separate suggestions from input text.
        let list_block = Block::default()
            .borders(Borders::TOP)
            .border_style(app.theme.border)
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
                    item = item.style(app.theme.selection);
                }
                item
            })
            .collect();

        let list = List::new(items);
        f.render_widget(list, list_inner);
    }

    let cursor_x = input_area.x + cursor_col as u16;
    let cursor_y = input_area.y + cursor_row as u16;
    if app.permission_prompt.is_none() && app.dialog.is_none() {
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

    // Week 9: dialog overlays.
    if let Some(dialog) = &app.dialog {
        render_dialog_modal(f, app, dialog, size);
    }

    // Week 10: transient notification toasts.
    if app.permission_prompt.is_none() && app.dialog.is_none() {
        render_toasts(f, app, size);
    }
}
fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
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

fn format_agent_elapsed(started_at: Instant, finished_at: Option<Instant>) -> String {
    let elapsed = finished_at
        .unwrap_or_else(Instant::now)
        .duration_since(started_at);
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m{rem:02}s")
    }
}

fn agent_state_label(agent: &AgentUiEntry) -> &'static str {
    match agent.finished_at {
        Some(_) if agent.is_error == Some(true) => "failed",
        Some(_) => "done",
        None => "running",
    }
}

fn agent_activity_lines(agent: &AgentUiEntry) -> Vec<String> {
    agent
        .progress
        .recent_activities
        .iter()
        .rev()
        .take(5)
        .map(|activity| tool_activity_status(&activity.tool_name, &activity.input))
        .collect()
}

fn agent_summary_line(agent: &AgentUiEntry, width: usize) -> String {
    let state = agent_state_label(agent);
    let timing = format_agent_elapsed(agent.started_at, agent.finished_at);
    let preview = agent
        .progress
        .recent_activities
        .last()
        .map(|activity| tool_activity_status(&activity.tool_name, &activity.input))
        .or_else(|| agent.progress.output_preview.clone())
        .or_else(|| agent.result_preview.clone())
        .unwrap_or_else(|| "starting".to_string());
    crate::one_line_preview(
        &format!(
            "{} • {} • {} tool(s) • {} tokens • {} • {}",
            agent.description,
            state,
            agent.progress.tool_use_count,
            agent.progress.token_count,
            timing,
            preview
        ),
        width,
    )
}

fn render_agent_progress_dialog(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    dialog: &AgentProgressDialog,
    area: ratatui::layout::Rect,
) {
    let popup = centered_rect(82, 62, area);
    f.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Agent Progress");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(2)])
        .split(inner);

    let visible_h = rows[0].height as usize;
    let start = list_window_start(dialog.selected, app.agents.len(), visible_h);
    let items = app
        .agents
        .iter()
        .skip(start)
        .take(visible_h)
        .enumerate()
        .map(|(offset, agent)| {
            let idx = start.saturating_add(offset);
            let mut item = ListItem::new(Line::from(agent_summary_line(
                agent,
                rows[0].width.saturating_sub(2) as usize,
            )));
            if idx == dialog.selected {
                item = item.style(app.theme.selection);
            }
            item
        })
        .collect::<Vec<_>>();
    f.render_widget(List::new(items), rows[0]);
    f.render_widget(
        Paragraph::new("Up/Down select • Enter details • Esc close • Ctrl+G open"),
        rows[1],
    );
}

fn render_agent_detail_dialog(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    dialog: &AgentDetailDialog,
    area: ratatui::layout::Rect,
) {
    let popup = centered_rect(84, 68, area);
    f.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default().borders(Borders::ALL).title("Agent Detail");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let Some(agent) = app.agent_by_id(&dialog.agent_tool_use_id) else {
        f.render_widget(Paragraph::new("Agent no longer available."), inner);
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(inner);

    let header = format!(
        "Description: {}\nState: {} • {} tool(s) • {} tokens • {}\nTool use id: {}\nPrompt: {}",
        agent.description,
        agent_state_label(agent),
        agent.progress.tool_use_count,
        agent.progress.token_count,
        format_agent_elapsed(agent.started_at, agent.finished_at),
        agent.tool_use_id,
        crate::one_line_preview(&agent.prompt, rows[0].width.saturating_sub(8) as usize),
    );
    f.render_widget(Paragraph::new(header), rows[0]);

    let mut body = String::new();
    if let Some(preview) = agent.progress.output_preview.as_deref() {
        body.push_str("Recent output\n");
        body.push_str(preview);
        body.push_str("\n\n");
    }

    let activities = agent_activity_lines(agent);
    body.push_str("Recent activities\n");
    if activities.is_empty() {
        body.push_str("- none yet");
    } else {
        for activity in activities {
            body.push_str("- ");
            body.push_str(&activity);
            body.push('\n');
        }
    }

    if let Some(preview) = agent.result_preview.as_deref() {
        body.push_str("\nFinal result preview\n");
        body.push_str(preview);
    }

    f.render_widget(Paragraph::new(body), rows[1]);
    f.render_widget(Paragraph::new("Esc/Left back • Ctrl+G agent list"), rows[2]);
}

fn render_toasts(f: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    if area.height < 4 || area.width < 20 {
        return;
    }
    if app.toasts.is_empty() {
        return;
    }

    // Show up to 3 most-recent toasts. Each uses 3 rows (border + 1 line).
    let max_visible = ((area.height.saturating_sub(1)) / 3).max(1) as usize;
    let max_visible = max_visible.min(3);

    let toasts = app
        .toasts
        .iter()
        .rev()
        .take(max_visible)
        .collect::<Vec<_>>();

    // Render oldest-to-newest top-down.
    let toasts = toasts.into_iter().rev().collect::<Vec<_>>();

    let width = area.width.min(60).max(20);
    let x = area.x + area.width.saturating_sub(width);
    let mut y = area.y.saturating_add(1);

    for toast in toasts {
        if y.saturating_add(3) > area.y.saturating_add(area.height) {
            break;
        }

        let rect = ratatui::layout::Rect {
            x,
            y,
            width,
            height: 3,
        };
        y = y.saturating_add(3);

        let style = match toast.level {
            ToastLevel::Info => app.theme.toast_info,
            ToastLevel::Warn => app.theme.toast_warn,
            ToastLevel::Error => app.theme.toast_error,
        };

        // Note: the inner space is (width - 2). Keep it one-line to avoid
        // obscuring too much of the transcript.
        let msg = crate::one_line_preview(&toast.message, width.saturating_sub(2) as usize);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(app.theme.border)
            .style(style);

        f.render_widget(ratatui::widgets::Clear, rect);
        f.render_widget(Paragraph::new(msg).block(block).style(style), rect);
    }
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

fn list_window_start(selected: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let half = height / 2;
    selected
        .saturating_sub(half)
        .min(len.saturating_sub(height))
}

fn available_models(current: &str) -> Vec<String> {
    let mut models = vec![
        "claude-sonnet-4-6".to_string(),
        "claude-opus-4-6".to_string(),
        "claude-opus-4-1-20250805".to_string(),
        "claude-3-5-haiku-20241022".to_string(),
        "claude-haiku-4-5-20251001".to_string(),
    ];
    if !models.iter().any(|model| model == current) {
        models.insert(0, current.to_string());
    }
    models
}

fn open_model_picker(app: &mut App) -> anyhow::Result<()> {
    let mut models = available_models(&app.model);
    if let Ok(sessions) = list_recent_sessions(&app.cwd) {
        for s in sessions {
            if let Some(model) = s.model {
                if !models.iter().any(|m| m == &model) {
                    models.push(model);
                }
            }
        }
    }
    models.sort();
    models.dedup();

    let selected = models
        .iter()
        .position(|model| model == &app.model)
        .unwrap_or(0);

    app.keymap.clear_pending();
    app.dismiss_typeahead();
    app.reverse_history_search = None;
    app.dialog = Some(DialogState::ModelPicker(ModelPickerDialog {
        filter: InputBuffer::new(),
        selected,
        models,
    }));
    app.status = "model picker".to_string();
    Ok(())
}

fn filtered_models(dialog: &ModelPickerDialog) -> Vec<String> {
    let q = dialog.filter.as_str().trim().to_ascii_lowercase();
    let mut out: Vec<String> = dialog
        .models
        .iter()
        .filter(|model| q.is_empty() || model.to_ascii_lowercase().contains(&q))
        .cloned()
        .collect();
    if out.is_empty() && !q.is_empty() {
        out.push(dialog.filter.as_str().trim().to_string());
    }
    out
}

fn open_session_resume(app: &mut App) -> anyhow::Result<()> {
    let sessions = list_recent_sessions(&app.cwd)?;
    if sessions.is_empty() {
        app.push_system_message("No previous sessions found for this project yet.");
        app.status = "resume unavailable".to_string();
        return Ok(());
    }

    let selected = sessions
        .iter()
        .position(|session| session.id == app.session_id)
        .unwrap_or(0);
    app.keymap.clear_pending();
    app.dismiss_typeahead();
    app.reverse_history_search = None;
    app.dialog = Some(DialogState::SessionResume(SessionResumeDialog {
        filter: InputBuffer::new(),
        selected,
        sessions,
    }));
    app.status = "resume session".to_string();
    Ok(())
}

fn list_recent_sessions(cwd: &Path) -> anyhow::Result<Vec<SessionInfo>> {
    let dir = claude_core::history::project_dir_for_cwd(cwd)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut sessions = Vec::new();
    for ent in entries {
        let ent = match ent {
            Ok(ent) => ent,
            Err(_) => continue,
        };
        let path = ent.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(id) = stem.parse::<SessionId>() else {
            continue;
        };

        let mut updated_at_ms = ent
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|dur| dur.as_millis() as u64)
            .unwrap_or(0);
        let mut model = None;
        let mut cost_usd = None;
        let mut response_preview = None;

        let meta_path = path.with_extension("meta.json");
        if let Ok(bytes) = std::fs::read(&meta_path) {
            if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(v) = meta.get("updated_at_ms").and_then(|v| v.as_u64()) {
                    updated_at_ms = v;
                }
                model = meta
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                cost_usd = meta.get("cost_usd").and_then(|v| v.as_f64());
                response_preview = meta
                    .get("response_preview")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }

        sessions.push(SessionInfo {
            id,
            path,
            updated_at_ms,
            model,
            cost_usd,
            response_preview,
        });
    }

    sessions.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    });
    Ok(sessions)
}

fn filtered_sessions(dialog: &SessionResumeDialog) -> Vec<SessionInfo> {
    let q = dialog.filter.as_str().trim().to_ascii_lowercase();
    dialog
        .sessions
        .iter()
        .filter(|session| {
            q.is_empty()
                || session.id.to_string().contains(&q)
                || session
                    .model
                    .as_deref()
                    .is_some_and(|model| model.to_ascii_lowercase().contains(&q))
                || session
                    .response_preview
                    .as_deref()
                    .is_some_and(|preview| preview.to_ascii_lowercase().contains(&q))
        })
        .cloned()
        .collect()
}

fn resume_session_by_id(app: &mut App, session_id: SessionId) -> anyhow::Result<()> {
    let session_path = claude_core::history::session_file_path(&app.cwd, session_id)?;
    let history = claude_core::history::load_session_messages(&session_path)?;
    app.switch_to_session(
        session_id,
        session_path,
        history,
        format!("resumed {}", session_id),
        format!("Resumed session `{session_id}`."),
    );
    Ok(())
}

fn open_transcript_search(app: &mut App, initial_query: Option<String>) {
    app.keymap.clear_pending();
    app.dismiss_typeahead();
    app.reverse_history_search = None;

    let mut dialog = TranscriptSearchDialog {
        query: InputBuffer::new(),
        selected: 0,
        hits: Vec::new(),
        last_query: String::new(),
        last_transcript_len: 0,
        last_transcript_rev: 0,
    };
    if let Some(query) = initial_query {
        dialog.query.set_text(query);
    }
    recompute_search_hits(&mut dialog, &app.transcript, app.transcript_rev);
    app.dialog = Some(DialogState::TranscriptSearch(dialog));
    app.status = "search transcript".to_string();
}

fn open_agent_progress_dialog(app: &mut App) {
    app.keymap.clear_pending();
    app.dismiss_typeahead();
    app.reverse_history_search = None;
    app.prune_agents();

    if app.agents.is_empty() {
        app.push_system_message(
            "Agent Progress\n\nNo agents are currently running.\nStart a task that uses the `Agent` tool to see teammate progress here.",
        );
        app.status = "agents idle".to_string();
        return;
    }

    app.dialog = Some(DialogState::AgentProgress(AgentProgressDialog {
        selected: 0,
    }));
    app.status = "agent progress".to_string();
}

fn recompute_search_hits(
    dialog: &mut TranscriptSearchDialog,
    transcript: &[ChatEntry],
    transcript_rev: u64,
) {
    let query = dialog.query.as_str().trim().to_ascii_lowercase();
    dialog.last_query = query.clone();
    dialog.last_transcript_len = transcript.len();
    dialog.last_transcript_rev = transcript_rev;
    dialog.hits.clear();

    if query.is_empty() {
        dialog.selected = 0;
        return;
    }

    for (entry_idx, entry) in transcript.iter().enumerate() {
        let haystack = entry.text.to_ascii_lowercase();
        if !haystack.contains(&query) {
            continue;
        }

        let preview = search_preview(&entry.text, &query);
        dialog.hits.push(SearchHit {
            entry_idx,
            role: entry.role,
            preview,
        });
    }

    if dialog.hits.is_empty() {
        dialog.selected = 0;
    } else {
        dialog.selected = dialog.selected.min(dialog.hits.len().saturating_sub(1));
    }
}

fn search_preview(text: &str, query: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let lower = text.to_ascii_lowercase();
    let start = lower
        .find(query)
        .and_then(|idx| {
            let prefix = text[..idx].chars().count();
            prefix.checked_sub(30)
        })
        .unwrap_or(0);
    let snippet = text.chars().skip(start).take(120).collect::<String>();
    crate::one_line_preview(&snippet, 120)
}

fn jump_to_entry(app: &mut App, entry_idx: usize) {
    let line = app
        .line_offsets
        .get(entry_idx)
        .copied()
        .unwrap_or_else(|| entry_idx.saturating_mul(3));
    app.scroll_top = line;
    app.scroll_follow = false;
    app.status = format!("jumped to match {}", entry_idx.saturating_add(1));
}

fn dialog_paste(dialog: &mut DialogState, text: &str) {
    let text = text.replace('\n', " ").replace('\r', " ");
    match dialog {
        DialogState::Onboarding(_) => {}
        DialogState::ModelPicker(model) => {
            model.filter.insert_str(text.trim_end());
            let len = filtered_models(model).len();
            model.selected = model.selected.min(len.saturating_sub(1));
        }
        DialogState::SessionResume(resume) => {
            resume.filter.insert_str(text.trim_end());
            let len = filtered_sessions(resume).len();
            resume.selected = resume.selected.min(len.saturating_sub(1));
        }
        DialogState::TranscriptSearch(search) => {
            search.query.insert_str(text.trim_end());
        }
        DialogState::AgentProgress(_) => {}
        DialogState::AgentDetail(_) => {}
    }
}

async fn handle_dialog_key(
    app: &mut App,
    key: crossterm::event::KeyEvent,
    _query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let Some(mut state) = app.dialog.take() else {
        return Ok(false);
    };

    let mut keep_dialog = true;
    let mut should_quit = false;

    match &mut state {
        DialogState::Onboarding(_) => match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                persist_tui_onboarding_seen(&app.user_settings_path)?;
                app.status = "ready".to_string();
                keep_dialog = false;
            }
            KeyCode::Esc
            | KeyCode::Char('q')
            | KeyCode::Char('Q')
            | KeyCode::Char('n')
            | KeyCode::Char('N') => {
                should_quit = true;
                keep_dialog = false;
            }
            _ => {}
        },
        DialogState::ModelPicker(dialog) => {
            let items = filtered_models(dialog);
            let len = items.len();
            match key.code {
                KeyCode::Esc => {
                    app.status = "ready".to_string();
                    keep_dialog = false;
                }
                KeyCode::Up => {
                    if len > 0 {
                        dialog.selected = dialog.selected.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if len > 0 {
                        dialog.selected = (dialog.selected + 1).min(len.saturating_sub(1));
                    }
                }
                KeyCode::Backspace => {
                    dialog.filter.backspace();
                    let len = filtered_models(dialog).len();
                    dialog.selected = dialog.selected.min(len.saturating_sub(1));
                }
                KeyCode::Delete => {
                    dialog.filter.delete_forward();
                    let len = filtered_models(dialog).len();
                    dialog.selected = dialog.selected.min(len.saturating_sub(1));
                }
                KeyCode::Left => dialog.filter.move_left(false),
                KeyCode::Right => dialog.filter.move_right(false),
                KeyCode::Home => dialog.filter.move_to_start(false),
                KeyCode::End => dialog.filter.move_to_end(false),
                KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
                    dialog.filter.move_to_start(false)
                }
                KeyCode::Char('e') | KeyCode::Char('E') if ctrl => dialog.filter.move_to_end(false),
                KeyCode::Char(ch) if !ctrl && !alt => {
                    dialog.filter.insert_char(ch);
                    let len = filtered_models(dialog).len();
                    dialog.selected = dialog.selected.min(len.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let picked = filtered_models(dialog)
                        .get(dialog.selected)
                        .cloned()
                        .or_else(|| {
                            let raw = dialog.filter.as_str().trim();
                            (!raw.is_empty()).then(|| raw.to_string())
                        });
                    if let Some(next_model) = picked {
                        if app.in_flight {
                            app.status = "cannot change model while a run is active".to_string();
                        } else if next_model.trim().is_empty() {
                            // no-op
                        } else {
                            app.model = next_model.clone();
                            app.rebuild_engine()?;
                            app.status = format!("model {}", app.model);
                            app.push_system_message(format!("Model updated to `{next_model}`"));
                            keep_dialog = false;
                        }
                    }
                }
                _ => {}
            }
        }
        DialogState::SessionResume(dialog) => {
            let items = filtered_sessions(dialog);
            let len = items.len();
            match key.code {
                KeyCode::Esc => {
                    app.status = "ready".to_string();
                    keep_dialog = false;
                }
                KeyCode::Up => {
                    if len > 0 {
                        dialog.selected = dialog.selected.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if len > 0 {
                        dialog.selected = (dialog.selected + 1).min(len.saturating_sub(1));
                    }
                }
                KeyCode::Backspace => {
                    dialog.filter.backspace();
                    let len = filtered_sessions(dialog).len();
                    dialog.selected = dialog.selected.min(len.saturating_sub(1));
                }
                KeyCode::Delete => {
                    dialog.filter.delete_forward();
                    let len = filtered_sessions(dialog).len();
                    dialog.selected = dialog.selected.min(len.saturating_sub(1));
                }
                KeyCode::Left => dialog.filter.move_left(false),
                KeyCode::Right => dialog.filter.move_right(false),
                KeyCode::Home => dialog.filter.move_to_start(false),
                KeyCode::End => dialog.filter.move_to_end(false),
                KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
                    dialog.filter.move_to_start(false)
                }
                KeyCode::Char('e') | KeyCode::Char('E') if ctrl => dialog.filter.move_to_end(false),
                KeyCode::Char(ch) if !ctrl && !alt => {
                    dialog.filter.insert_char(ch);
                    let len = filtered_sessions(dialog).len();
                    dialog.selected = dialog.selected.min(len.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let id = filtered_sessions(dialog)
                        .get(dialog.selected)
                        .map(|session| session.id)
                        .or_else(|| dialog.filter.as_str().trim().parse::<SessionId>().ok());
                    if let Some(id) = id {
                        resume_session_by_id(app, id)?;
                        keep_dialog = false;
                    }
                }
                _ => {}
            }
        }
        DialogState::TranscriptSearch(dialog) => match key.code {
            KeyCode::Esc => {
                app.status = "ready".to_string();
                keep_dialog = false;
            }
            KeyCode::Up => {
                if !dialog.hits.is_empty() {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if !dialog.hits.is_empty() {
                    dialog.selected =
                        (dialog.selected + 1).min(dialog.hits.len().saturating_sub(1));
                }
            }
            KeyCode::Backspace => {
                dialog.query.backspace();
                recompute_search_hits(dialog, &app.transcript, app.transcript_rev);
            }
            KeyCode::Delete => {
                dialog.query.delete_forward();
                recompute_search_hits(dialog, &app.transcript, app.transcript_rev);
            }
            KeyCode::Left => dialog.query.move_left(false),
            KeyCode::Right => dialog.query.move_right(false),
            KeyCode::Home => dialog.query.move_to_start(false),
            KeyCode::End => dialog.query.move_to_end(false),
            KeyCode::Char('a') | KeyCode::Char('A') if ctrl => dialog.query.move_to_start(false),
            KeyCode::Char('e') | KeyCode::Char('E') if ctrl => dialog.query.move_to_end(false),
            KeyCode::Char(ch) if !ctrl && !alt => {
                dialog.query.insert_char(ch);
                recompute_search_hits(dialog, &app.transcript, app.transcript_rev);
            }
            KeyCode::Enter => {
                if let Some(hit) = dialog.hits.get(dialog.selected) {
                    let entry_idx = hit.entry_idx;
                    jump_to_entry(app, entry_idx);
                    keep_dialog = false;
                }
            }
            _ => {}
        },
        DialogState::AgentProgress(dialog) => match key.code {
            KeyCode::Esc => {
                app.status = "ready".to_string();
                keep_dialog = false;
            }
            KeyCode::Up => {
                if !app.agents.is_empty() {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if !app.agents.is_empty() {
                    dialog.selected = (dialog.selected + 1).min(app.agents.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if let Some(agent) = app.agents.get(dialog.selected) {
                    app.dialog = Some(DialogState::AgentDetail(AgentDetailDialog {
                        agent_tool_use_id: agent.tool_use_id.clone(),
                    }));
                    app.status = "agent detail".to_string();
                    keep_dialog = false;
                }
            }
            _ => {}
        },
        DialogState::AgentDetail(detail) => match key.code {
            KeyCode::Esc | KeyCode::Left => {
                let selected = app
                    .agents
                    .iter()
                    .position(|agent| agent.tool_use_id == detail.agent_tool_use_id)
                    .unwrap_or(0);
                app.dialog = Some(DialogState::AgentProgress(AgentProgressDialog { selected }));
                app.status = "agent progress".to_string();
                keep_dialog = false;
            }
            _ => {}
        },
    }

    if keep_dialog {
        app.dialog = Some(state);
    }

    Ok(should_quit)
}

fn persist_tui_onboarding_seen(settings_path: &Path) -> anyhow::Result<()> {
    let _lock = crate::lock_settings_path(settings_path)?;
    let mut root = crate::load_settings_json_object_or_empty(settings_path)?;
    let Some(obj) = root.as_object_mut() else {
        anyhow::bail!(
            "settings root must be a JSON object: {}",
            settings_path.display()
        );
    };
    obj.insert(
        "tuiOnboardingSeen".to_string(),
        serde_json::Value::Bool(true),
    );
    crate::save_settings_json(settings_path, &root)?;
    Ok(())
}

fn persist_tui_theme(settings_path: &Path, theme_name: &str) -> anyhow::Result<()> {
    let theme_name = theme_name.trim();
    if theme_name.is_empty() {
        anyhow::bail!("theme name is empty");
    }

    let _lock = crate::lock_settings_path(settings_path)?;
    let mut root = crate::load_settings_json_object_or_empty(settings_path)?;
    let Some(obj) = root.as_object_mut() else {
        anyhow::bail!(
            "settings root must be a JSON object: {}",
            settings_path.display()
        );
    };

    obj.insert(
        "tuiTheme".to_string(),
        serde_json::Value::String(theme_name.to_string()),
    );
    crate::save_settings_json(settings_path, &root)?;
    Ok(())
}

fn persist_tui_show_thinking(settings_path: &Path, show: bool) -> anyhow::Result<()> {
    let _lock = crate::lock_settings_path(settings_path)?;
    let mut root = crate::load_settings_json_object_or_empty(settings_path)?;
    let Some(obj) = root.as_object_mut() else {
        anyhow::bail!(
            "settings root must be a JSON object: {}",
            settings_path.display()
        );
    };

    obj.insert("tuiShowThinking".to_string(), serde_json::Value::Bool(show));
    crate::save_settings_json(settings_path, &root)?;
    Ok(())
}

fn persist_tui_condensed(settings_path: &Path, condensed: bool) -> anyhow::Result<()> {
    let _lock = crate::lock_settings_path(settings_path)?;
    let mut root = crate::load_settings_json_object_or_empty(settings_path)?;
    let Some(obj) = root.as_object_mut() else {
        anyhow::bail!(
            "settings root must be a JSON object: {}",
            settings_path.display()
        );
    };

    obj.insert(
        "tuiCondensed".to_string(),
        serde_json::Value::Bool(condensed),
    );
    crate::save_settings_json(settings_path, &root)?;
    Ok(())
}

fn render_dialog_modal(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    dialog: &DialogState,
    area: ratatui::layout::Rect,
) {
    match dialog {
        DialogState::Onboarding(_) => render_onboarding_dialog(f, app, area),
        DialogState::ModelPicker(dialog) => render_model_picker_dialog(f, app, dialog, area),
        DialogState::SessionResume(dialog) => render_session_resume_dialog(f, app, dialog, area),
        DialogState::TranscriptSearch(dialog) => {
            render_transcript_search_dialog(f, app, dialog, area)
        }
        DialogState::AgentProgress(dialog) => render_agent_progress_dialog(f, app, dialog, area),
        DialogState::AgentDetail(dialog) => render_agent_detail_dialog(f, app, dialog, area),
    }
}

fn render_onboarding_dialog(f: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let popup = centered_rect(72, 52, area);
    f.render_widget(ratatui::widgets::Clear, popup);

    let body = format!(
        "Welcome to the Claude Rust TUI.\n\n\
Project: {}\n\
Model: {}\n\n\
This terminal UI can stream responses, run tools, and resume prior sessions.\n\
Tool calls still follow your current permission mode: {:?}.\n\n\
Useful keys\n\
- Enter submits the prompt\n\
- Alt+Enter inserts a newline\n\
- Ctrl+F opens transcript search\n\
- /model opens the model picker\n\
- /resume opens recent sessions\n\n\
Press Enter to continue and save this onboarding step.\n\
Press Esc to quit.",
        app.cwd.display(),
        app.model,
        app.engine_inputs.cfg.permission_mode
    );

    let widget =
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title("Welcome"));
    f.render_widget(widget, popup);
}

fn render_model_picker_dialog(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    dialog: &ModelPickerDialog,
    area: ratatui::layout::Rect,
) {
    let popup = centered_rect(70, 60, area);
    f.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default().borders(Borders::ALL).title("Model Picker");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(inner);

    let filter = Paragraph::new(format!("Filter: {}", dialog.filter.as_str()));
    f.render_widget(filter, rows[0]);

    let items = filtered_models(dialog);
    let visible_h = rows[1].height as usize;
    let start = list_window_start(dialog.selected, items.len(), visible_h);
    let items = items
        .into_iter()
        .skip(start)
        .take(visible_h)
        .enumerate()
        .map(|(offset, model)| {
            let idx = start.saturating_add(offset);
            let mut spans = vec![Span::styled(
                model.clone(),
                Style::default().fg(if model == app.model {
                    Color::Green
                } else {
                    Color::White
                }),
            )];
            if let Some(costs) = claude_query::cost::model_costs(&model) {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!(
                        "(${:.2}/${:.2} per Mtok)",
                        costs.input_per_mtok, costs.output_per_mtok
                    ),
                    Style::default().fg(Color::Gray),
                ));
            }
            let mut item = ListItem::new(Line::from(spans));
            if idx == dialog.selected {
                item = item.style(app.theme.selection);
            }
            item
        })
        .collect::<Vec<_>>();

    f.render_widget(List::new(items), rows[1]);
    f.render_widget(
        Paragraph::new("Up/Down select • Enter apply • type to filter • Esc close"),
        rows[2],
    );
    let cursor_x = rows[0]
        .x
        .saturating_add("Filter: ".len() as u16)
        .saturating_add(dialog.filter.cursor() as u16);
    f.set_cursor(cursor_x, rows[0].y);
}

fn render_session_resume_dialog(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    dialog: &SessionResumeDialog,
    area: ratatui::layout::Rect,
) {
    let popup = centered_rect(80, 65, area);
    f.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Resume Session");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("Filter: {}", dialog.filter.as_str())),
        rows[0],
    );

    let sessions = filtered_sessions(dialog);
    let visible_h = rows[1].height as usize;
    let start = list_window_start(dialog.selected, sessions.len(), visible_h);
    let items = sessions
        .into_iter()
        .skip(start)
        .take(visible_h)
        .enumerate()
        .map(|(offset, session)| {
            let idx = start.saturating_add(offset);
            let mut line = format!(
                "{}  {}",
                format_updated_at_ms(session.updated_at_ms),
                session.id
            );
            if let Some(model) = &session.model {
                line.push_str(&format!("  {model}"));
            }
            if let Some(cost) = session.cost_usd {
                line.push_str(&format!("  ${cost:.4}"));
            }
            if session.id == app.session_id {
                line.push_str("  (current)");
            }
            let preview = session
                .response_preview
                .as_deref()
                .map(|s| crate::one_line_preview(s, 100))
                .unwrap_or_else(|| session.path.display().to_string());
            let mut item = ListItem::new(Line::from(format!("{line}  —  {preview}")));
            if idx == dialog.selected {
                item = item.style(app.theme.selection);
            }
            item
        })
        .collect::<Vec<_>>();
    f.render_widget(List::new(items), rows[1]);
    f.render_widget(
        Paragraph::new(
            "Up/Down select • Enter resume • type to filter by id/model/preview • Esc close",
        ),
        rows[2],
    );
    let cursor_x = rows[0]
        .x
        .saturating_add("Filter: ".len() as u16)
        .saturating_add(dialog.filter.cursor() as u16);
    f.set_cursor(cursor_x, rows[0].y);
}

fn render_transcript_search_dialog(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    dialog: &TranscriptSearchDialog,
    area: ratatui::layout::Rect,
) {
    let popup = centered_rect(78, 60, area);
    f.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Transcript Search");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("Query: {}", dialog.query.as_str())),
        rows[0],
    );

    let visible_h = rows[1].height as usize;
    let start = list_window_start(dialog.selected, dialog.hits.len(), visible_h);
    let items = dialog
        .hits
        .iter()
        .skip(start)
        .take(visible_h)
        .enumerate()
        .map(|(offset, hit)| {
            let idx = start.saturating_add(offset);
            let role = match hit.role {
                Role::User => "You",
                Role::Assistant => "Claude",
                Role::Tool => "Tool",
                Role::System => "System",
                Role::Thinking => "Thinking",
            };
            let mut item = ListItem::new(Line::from(format!(
                "{role} #{:03}  {}",
                hit.entry_idx.saturating_add(1),
                hit.preview
            )));
            if idx == dialog.selected {
                item = item.style(app.theme.selection);
            }
            item
        })
        .collect::<Vec<_>>();
    f.render_widget(List::new(items), rows[1]);

    let footer = if dialog.query.as_str().trim().is_empty() {
        "Type to search the current transcript • Esc close"
    } else if dialog.hits.is_empty() {
        "No matches • Enter does nothing • Esc close"
    } else {
        "Up/Down select • Enter jump • Esc close"
    };
    f.render_widget(Paragraph::new(footer), rows[2]);
    let cursor_x = rows[0]
        .x
        .saturating_add("Query: ".len() as u16)
        .saturating_add(dialog.query.cursor() as u16);
    f.set_cursor(cursor_x, rows[0].y);
}

fn format_updated_at_ms(updated_at_ms: u64) -> String {
    let secs = (updated_at_ms / 1000) as i64;
    let Some(ts) = DateTime::<Utc>::from_timestamp(secs, 0) else {
        return "unknown-time".to_string();
    };
    ts.format("%Y-%m-%d %H:%M").to_string()
}

fn render_thinking_entry(entry: &ThinkingEntry, show_thinking: bool) -> String {
    if show_thinking {
        let body = crate::truncate_chars(entry.thinking.trim(), 40_000);
        if body.is_empty() {
            "_Thinking..._".to_string()
        } else {
            format!("```text\n{body}\n```")
        }
    } else {
        let approx_tokens = (entry.thinking.chars().count() / 4).max(1);
        format!("_Thinking hidden._ Use `/thinking on` to expand it. (~{approx_tokens} tokens)")
    }
}

fn summarize_tool_result(tool_name: &str, result: &serde_json::Value, is_error: bool) -> String {
    let prefix = if is_error { "error" } else { "ok" };

    match result {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                format!("{prefix} • empty output")
            } else {
                let line_count = s.lines().count();
                let preview = crate::one_line_preview(s, 100);
                format!("{prefix} • {line_count} line(s) • {preview}")
            }
        }
        serde_json::Value::Array(arr) => {
            format!("{prefix} • {} item(s)", arr.len())
        }
        serde_json::Value::Object(map) => {
            if tool_name == "Agent" {
                if let Some(text) = result.as_str() {
                    let preview = crate::one_line_preview(text, 100);
                    return format!("{prefix} • report • {preview}");
                }
            }
            let keys = map.keys().take(4).cloned().collect::<Vec<_>>();
            if keys.is_empty() {
                format!("{prefix} • empty object")
            } else {
                format!("{prefix} • fields: {}", keys.join(", "))
            }
        }
        serde_json::Value::Null => format!("{prefix} • null"),
        serde_json::Value::Bool(v) => format!("{prefix} • {v}"),
        serde_json::Value::Number(v) => format!("{prefix} • {v}"),
    }
}

fn render_tool_entry(tool: &ToolEntry, condensed: bool) -> String {
    match &tool.state {
        ToolEntryState::Running => format_tool_running_markdown(&tool.name, &tool.input),
        ToolEntryState::Result { result, is_error } => {
            if condensed {
                let mut out = String::new();
                let status = if *is_error { " (error)" } else { "" };
                if let Some(primary) = tool_primary_summary(&tool.name, &tool.input) {
                    if !primary.contains('\n') && !primary.contains('`') {
                        let preview = crate::truncate_chars(primary.trim(), 200);
                        out.push_str(&format!("**{}**{status}: `{preview}`\n\n", tool.name));
                    } else {
                        out.push_str(&format!("**{}**{status}\n\n", tool.name));
                    }
                } else {
                    out.push_str(&format!("**{}**{status}\n\n", tool.name));
                }

                let summary = summarize_tool_result(&tool.name, result, *is_error);
                out.push_str(&format!(
                    "Result summary: {summary}\n\n_Condensed view._ Use `/condensed off` to show full output."
                ));
                out
            } else {
                format_tool_result_markdown(&tool.name, &tool.input, result, *is_error)
            }
        }
    }
}

fn transcript_from_history(history: &[Message], prefs: DisplayPrefs) -> Vec<ChatEntry> {
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
                                out.push(ChatEntry::plain(
                                    Role::User,
                                    std::mem::take(&mut text_buf),
                                ));
                            }

                            let (tool_name, tool_input) = tool_input_for_id
                                .get(tool_use_id)
                                .cloned()
                                .unwrap_or_else(|| {
                                    (format!("Tool({})", tool_use_id), serde_json::Value::Null)
                                });

                            let tool_entry = ChatEntry::tool(
                                tool_name.clone(),
                                tool_input.clone(),
                                ToolEntryState::Result {
                                    result: content.clone(),
                                    is_error: *is_error,
                                },
                                prefs,
                            );

                            if let Some(idx) = tool_entry_for_id.get(tool_use_id).copied() {
                                if let Some(entry) = out.get_mut(idx) {
                                    *entry = tool_entry;
                                }
                            } else {
                                out.push(tool_entry);
                            }
                        }
                        ContentBlock::Thinking { thinking } => {
                            if !text_buf.trim().is_empty() {
                                out.push(ChatEntry::plain(
                                    Role::User,
                                    std::mem::take(&mut text_buf),
                                ));
                            }
                            out.push(ChatEntry::thinking(thinking.clone(), prefs));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }

                if !text_buf.trim().is_empty() {
                    out.push(ChatEntry::plain(Role::User, text_buf));
                }
            }
            Message::Assistant(claude_core::types::message::AssistantMessage {
                content, ..
            }) => {
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
                                out.push(ChatEntry::plain(
                                    Role::Assistant,
                                    std::mem::take(&mut text_buf),
                                ));
                            }

                            tool_input_for_id.insert(id.clone(), (name.clone(), input.clone()));

                            out.push(ChatEntry::tool(
                                name.clone(),
                                input.clone(),
                                ToolEntryState::Running,
                                prefs,
                            ));
                            tool_entry_for_id.insert(id.clone(), out.len().saturating_sub(1));
                        }
                        ContentBlock::Thinking { thinking } => {
                            if !text_buf.trim().is_empty() {
                                out.push(ChatEntry::plain(
                                    Role::Assistant,
                                    std::mem::take(&mut text_buf),
                                ));
                            }
                            out.push(ChatEntry::thinking(thinking.clone(), prefs));
                        }
                        ContentBlock::ToolResult { .. } => {}
                    }
                }

                if !text_buf.trim().is_empty() {
                    out.push(ChatEntry::plain(Role::Assistant, text_buf));
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
        "Agent" => {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if desc.is_empty() {
                "Running agent...".to_string()
            } else {
                format!("Agent: {}", crate::one_line_preview(desc, 120))
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
        anyhow::bail!(
            "settings root must be a JSON object: {}",
            settings_path.display()
        );
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
        Err(err) => {
            return format!(
                "(preview unavailable) cannot stat {}: {err}",
                path.display()
            );
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

    let original = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            return format!(
                "(preview unavailable) failed to read {}: {err}",
                path.display()
            );
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

    let content = input
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
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
                return format!(
                    "(preview unavailable) cannot stat {}: {err}",
                    path.display()
                );
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
                );
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
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
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
        "WebFetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
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
            out.push_str(&format!(
                "**{tool_name}**{status}\n\n```text\n{preview}\n```\n\n"
            ));
        }
    } else {
        let rendered = crate::truncate_chars(&render_value_pretty(input), 1200);
        out.push_str(&format!(
            "**{tool_name}**{status}\n\n```json\n{rendered}\n```\n\n"
        ));
    }

    let (lang, body) = match (tool_name, result) {
        ("Edit", serde_json::Value::String(s)) => ("diff", crate::truncate_chars(s, 50_000)),
        (_name, serde_json::Value::String(s)) => ("text", crate::truncate_chars(s, 50_000)),
        (_name, other) => (
            "json",
            crate::truncate_chars(&render_value_pretty(other), 50_000),
        ),
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
            let url = input
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            out.push_str("URL:\n");
            out.push_str(url);
        }
        "WebSearch" => {
            let q = input
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
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

fn write_session_meta_silent(
    session_id: SessionId,
    session_path: &Path,
    result: &claude_query::RunResult,
) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::types::message::AssistantMessage;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;
    use std::time::Duration as StdDuration;

    #[test]
    fn thinking_entry_can_toggle_visibility() {
        let mut entry = ChatEntry::thinking(
            "reasoning about the next step",
            DisplayPrefs {
                show_thinking: false,
                condensed: false,
            },
        );
        assert!(entry.text.contains("Thinking hidden"));

        let changed = entry.refresh_display(DisplayPrefs {
            show_thinking: true,
            condensed: false,
        });
        assert!(changed);
        assert!(entry.text.contains("reasoning about the next step"));
        assert!(!entry.text.contains("Thinking hidden"));
    }

    #[test]
    fn condensed_tool_entry_hides_full_payload() {
        let entry = ChatEntry::tool(
            "Bash",
            serde_json::json!({ "command": "printf 'hello\\nworld'" }),
            ToolEntryState::Result {
                result: serde_json::Value::String("hello\nworld\n".to_string()),
                is_error: false,
            },
            DisplayPrefs {
                show_thinking: false,
                condensed: true,
            },
        );

        assert!(entry.text.contains("Result summary:"));
        assert!(entry.text.contains("Condensed view"));
        assert!(!entry.text.contains("```text"));
    }

    #[test]
    fn transcript_from_history_renders_thinking_and_tool_result_entries() {
        let history = vec![
            Message::Assistant(AssistantMessage {
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "hidden chain of thought".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tool-1".to_string(),
                        name: "Agent".to_string(),
                        input: serde_json::json!({
                            "description": "research issue",
                            "prompt": "Investigate",
                        }),
                    },
                ],
                model: None,
                stop_reason: None,
                usage: None,
            }),
            Message::User(UserMessage {
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    content: serde_json::Value::String("done".to_string()),
                    is_error: false,
                }],
            }),
        ];

        let transcript = transcript_from_history(
            &history,
            DisplayPrefs {
                show_thinking: false,
                condensed: true,
            },
        );

        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].role, Role::Thinking);
        assert!(transcript[0].text.contains("Thinking hidden"));
        assert_eq!(transcript[1].role, Role::Tool);
        assert!(transcript[1].text.contains("Result summary:"));
        assert!(transcript[1].text.contains("research issue"));
    }

    fn buffer_to_trimmed_string(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf.get(x, y).symbol());
            }
            out.push_str(line.trim_end_matches(' '));
            out.push('\n');
        }
        out
    }

    fn render_app_snapshot(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(f, app)).expect("draw");
        buffer_to_trimmed_string(terminal.backend().buffer())
    }

    fn build_test_app(cwd: &Path) -> App {
        let session_id: SessionId = "00000000-0000-0000-0000-000000000000"
            .parse()
            .expect("session id");
        let session_path = cwd.join("session.jsonl");
        let model = "claude-sonnet-4-6".to_string();

        let theme = Theme::new(ThemeName::Dark);
        let md = MarkdownRenderer::new();
        let history: Vec<Message> = Vec::new();

        let show_thinking = false;
        let condensed = false;
        let prefs = DisplayPrefs {
            show_thinking,
            condensed,
        };

        let mut transcript = transcript_from_history(&history, prefs);
        if transcript.is_empty() {
            transcript.push(ChatEntry::plain(
                Role::System,
                "Ctrl+C to exit. Type a prompt and press Enter.",
            ));
        }
        let rendered = transcript
            .iter()
            .map(|e| RenderedEntry::new(e.role, &theme))
            .collect::<Vec<_>>();

        let client = claude_services::api::AnthropicClient::new(Some("http://127.0.0.1".into()));
        let auth = AuthMode::ApiKey("test-key".to_string());

        let engine_inputs = EngineInputs {
            max_tokens: 64,
            cfg: claude_query::QueryEngineConfig {
                cwd: cwd.to_path_buf(),
                bare: true,
                add_dirs: Vec::new(),
                system_prompt: None,
                append_system_prompt: None,
                json_schema: None,
                max_turns: 2,
                max_budget_usd: None,
                permission_mode: PermissionMode::Default,
                base_tools: Vec::new(),
                allowed_tools: Vec::new(),
                disallowed_tools: vec!["AskUserQuestion".to_string()],
                always_allow_tools: Vec::new(),
                mcp_servers: HashMap::new(),
                agent_depth: 0,
                max_agent_depth: 2,
            },
        };

        let engine = std::sync::Arc::new(
            claude_query::QueryEngine::new(
                client.clone(),
                auth.clone(),
                model.clone(),
                engine_inputs.max_tokens,
                engine_inputs.cfg.clone(),
            )
            .expect("engine"),
        );

        App {
            input: InputBuffer::new(),
            prompt_history: PromptHistory::new(Vec::new()),
            reverse_history_search: None,
            vim: None,
            keymap: KeybindingResolver::new(Duration::from_millis(900)),
            dialog: None,
            typeahead_query: None,
            typeahead_selected: 0,
            typeahead_suppressed_text: None,
            status: "ready".to_string(),
            theme: theme.clone(),
            toasts: Vec::new(),
            spinner_idx: 0,
            in_flight: false,
            active_assistant_idx: None,
            active_thinking_idx: None,
            run_started_at: None,
            run_stream_chars: 0,
            last_turn_tokens_per_sec: None,
            show_thinking,
            condensed,
            transcript_rev: 1,
            transcript,
            rendered,
            render_width: 0,
            md,
            history,
            tool_entry_for_id: HashMap::new(),
            permission_prompt: None,
            always_allow_tools: Arc::new(Mutex::new(HashSet::new())),
            agents: Vec::new(),
            scroll_top: 0,
            scroll_follow: true,
            last_msg_view_height: 0,
            line_offsets: Vec::new(),
            line_offsets_dirty_from: Some(0),
            session_id,
            session_path,
            model,
            cwd: cwd.to_path_buf(),
            user_settings_path: cwd.join("settings.json"),
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
        }
    }

    #[test]
    fn snapshot_renders_basic_shell() {
        let work = tempfile::tempdir().expect("temp work dir");
        let mut app = build_test_app(work.path());
        let snap = render_app_snapshot(&mut app, 64, 18);
        let expected = r#"claude-rs • session 00000000-0000-0000-0000-000000000000 • model
┌Messages──────────────────────────────────────────────────────┐
│System                                                        │
│Ctrl+C to exit. Type a prompt and press Enter.                │
│                                                              │
│                                                              │
│                                                              │
│                                                              │
│                                                              │
│                                                              │
│                                                              │
│                                                              │
│                                                              │
└──────────────────────────────────────────────────────────────┘
┌Input • / for commands • Alt+Enter newline • Up/Down history──┐
│                                                              │
└──────────────────────────────────────────────────────────────┘
  EDIT • claude-sonnet-4-6 • in=0 out=0 • tot $0.0000 • t/s -- •
"#;
        assert_eq!(snap, expected);
    }

    struct ExpectedRequest {
        must_contain: Option<String>,
        sse_body: String,
    }

    fn spawn_mock_sse_server_sequence(
        responses: Vec<ExpectedRequest>,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("server addr");

        let handle = thread::spawn(move || {
            for (idx, exp) in responses.into_iter().enumerate() {
                let (mut stream, _peer) = listener.accept().expect("accept");
                let _ = stream.set_read_timeout(Some(StdDuration::from_secs(10)));
                let _ = stream.set_write_timeout(Some(StdDuration::from_secs(10)));

                // Read until the end of headers.
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut tmp).expect("read request");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > 2_000_000 {
                        break;
                    }
                }

                let header_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .unwrap_or(buf.len());

                let header_str = String::from_utf8_lossy(&buf[..header_end]);
                let mut lines = header_str.split("\r\n");
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();

                let mut content_length: usize = 0;
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse::<usize>().unwrap_or(0);
                    }
                }

                // Read request body so we can assert on it.
                let mut body: Vec<u8> = Vec::new();
                let already_body = buf.len().saturating_sub(header_end);
                if already_body > 0 {
                    body.extend_from_slice(&buf[header_end..]);
                }
                let mut remaining = content_length.saturating_sub(already_body);
                while remaining > 0 {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    let take = remaining.min(n);
                    body.extend_from_slice(&tmp[..take]);
                    remaining = remaining.saturating_sub(take);
                }

                if let Some(needle) = exp.must_contain.as_deref() {
                    let body_str = String::from_utf8_lossy(&body);
                    assert!(
                        body_str.contains(needle),
                        "request {idx} body did not contain {needle}\nbody={body_str}"
                    );
                }

                let (status_line, body) = if method == "POST" && path == "/v1/messages" {
                    ("HTTP/1.1 200 OK\r\n", exp.sse_body)
                } else {
                    ("HTTP/1.1 404 Not Found\r\n", "not found".to_string())
                };

                let resp = format!(
                    "{status_line}Content-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.as_bytes().len()
                );
                stream.write_all(resp.as_bytes()).expect("write response");
                stream.flush().ok();
            }
        });

        (addr, handle)
    }

    fn sse_events(events: Vec<serde_json::Value>) -> String {
        let mut body = String::new();
        for ev in events {
            body.push_str("data: ");
            body.push_str(&ev.to_string());
            body.push('\n');
            body.push('\n');
        }
        body
    }

    fn mock_sse_ok_text(text: &str) -> String {
        let events = vec![
            serde_json::json!({
              "type": "message_start",
              "message": { "model": "claude-sonnet-4-6", "usage": { "input_tokens": 1, "output_tokens": 0 } }
            }),
            serde_json::json!({
              "type": "content_block_start",
              "index": 0,
              "content_block": { "type": "text", "text": "" }
            }),
            serde_json::json!({
              "type": "content_block_delta",
              "index": 0,
              "delta": { "type": "text_delta", "text": text }
            }),
            serde_json::json!({
              "type": "message_delta",
              "delta": { "stop_reason": "end_turn" },
              "usage": { "input_tokens": 1, "output_tokens": 1 }
            }),
            serde_json::json!({ "type": "message_stop" }),
        ];
        sse_events(events)
    }

    fn mock_sse_tool_use_write(file_path: &str, content: &str) -> String {
        let events = vec![
            serde_json::json!({
              "type": "message_start",
              "message": { "model": "claude-sonnet-4-6", "usage": { "input_tokens": 1, "output_tokens": 0 } }
            }),
            serde_json::json!({
              "type": "content_block_start",
              "index": 0,
              "content_block": {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "Write",
                "input": {
                  "file_path": file_path,
                  "content": content
                }
              }
            }),
            serde_json::json!({
              "type": "message_delta",
              "delta": { "stop_reason": "tool_use" },
              "usage": { "input_tokens": 1, "output_tokens": 1 }
            }),
            serde_json::json!({ "type": "message_stop" }),
        ];
        sse_events(events)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn integration_full_tui_flow_with_permission_and_tool() {
        let work = tempfile::tempdir().expect("temp work dir");
        let file_path = work.path().join("hello.txt");

        let sse1 = mock_sse_tool_use_write(&file_path.to_string_lossy(), "hi from tool");
        let sse2 = mock_sse_ok_text("All done");

        let (addr, handle) = spawn_mock_sse_server_sequence(vec![
            ExpectedRequest {
                must_contain: None,
                sse_body: sse1,
            },
            ExpectedRequest {
                must_contain: Some("\"tool_result\"".to_string()),
                sse_body: sse2,
            },
        ]);
        let base_url = format!("http://{addr}");

        // Build an app that targets the mock server.
        let mut app = build_test_app(work.path());
        app.client = claude_services::api::AnthropicClient::new(Some(base_url));
        app.engine = std::sync::Arc::new(
            claude_query::QueryEngine::new(
                app.client.clone(),
                app.auth.clone(),
                app.model.clone(),
                app.engine_inputs.max_tokens,
                app.engine_inputs.cfg.clone(),
            )
            .expect("engine"),
        );

        app.input.insert_str("Write a file");
        let (tx, mut rx) = mpsc::unbounded_channel::<QueryEvent>();
        submit_prompt(&mut app, tx.clone()).expect("submit");

        // Drive the event loop until completion, auto-allowing any permission prompt.
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("timeout")
                .expect("event");
            let done = matches!(ev, QueryEvent::Finished(_) | QueryEvent::Error(_));
            handle_query_event(&mut app, ev);

            if app.permission_prompt.is_some() {
                let key =
                    crossterm::event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty());
                handle_term_event(&mut app, Event::Key(key), tx.clone())
                    .await
                    .expect("allow");
            }

            if done {
                break;
            }
        }

        let written = std::fs::read_to_string(&file_path).expect("read written file");
        assert_eq!(written, "hi from tool");
        assert!(app.transcript.iter().any(|e| e.text.contains("All done")));
        assert!(app.transcript.iter().any(|e| e.text.contains("Write")));

        handle.join().expect("server thread");
    }
}
