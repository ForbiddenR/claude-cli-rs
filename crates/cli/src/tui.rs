use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use claude_core::types::ids::SessionId;
use claude_core::types::message::{ContentBlock, Message, UserMessage};
use claude_core::types::permissions::PermissionMode;
use claude_services::auth::AuthMode;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, terminal};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use similar::TextDiff;
use tokio::sync::{mpsc, oneshot};

use crate::args::{Args, InputFormat, OutputFormat};

mod markdown;

use markdown::{MarkdownRenderer, StreamingMarkdown};

const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

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
    input: String,
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
        execute!(std::io::stdout(), EnterAlternateScreen).context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
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

    let mut transcript = transcript_from_history(&history);
    if transcript.is_empty() {
        transcript.push(ChatEntry {
            role: Role::System,
            text: "Ctrl+C to exit. Type a prompt and press Enter.".to_string(),
        });
    }
    let rendered = transcript.iter().map(|e| RenderedEntry::new(e.role)).collect();

    let mut app = App {
        input: String::new(),
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
    };

    let client = claude_services::api::AnthropicClient::new(None);
    let engine = build_engine(args, settings, client, auth, model).await?;

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
                if handle_term_event(&mut app, ev, &engine, query_tx.clone()).await? {
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

async fn build_engine(
    args: &Args,
    settings: &claude_core::config::settings::Settings,
    client: claude_services::api::AnthropicClient,
    auth: AuthMode,
    model: String,
) -> anyhow::Result<std::sync::Arc<claude_query::QueryEngine>> {
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

    let engine = claude_query::QueryEngine::new(
        client,
        auth,
        model,
        max_tokens,
        claude_query::QueryEngineConfig {
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
    )?;

    Ok(std::sync::Arc::new(engine))
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

async fn handle_term_event(
    app: &mut App,
    ev: Event,
    engine: &std::sync::Arc<claude_query::QueryEngine>,
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<bool> {
    match ev {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }

            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('c') => return Ok(true),
                    _ => {}
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

            match key.code {
                // Week 3: scrollback.
                KeyCode::Up => {
                    scroll_up(app, 1);
                }
                KeyCode::Down => {
                    scroll_down(app, 1);
                }
                KeyCode::PageUp => {
                    let amount = app.last_msg_view_height.saturating_sub(1).max(1);
                    scroll_up(app, amount);
                }
                KeyCode::PageDown => {
                    let amount = app.last_msg_view_height.saturating_sub(1).max(1);
                    scroll_down(app, amount);
                }
                KeyCode::Home => {
                    scroll_to_top(app);
                }
                KeyCode::End => {
                    scroll_to_bottom(app);
                }
                KeyCode::Esc => {
                    app.input.clear();
                }
                KeyCode::Backspace => {
                    app.input.pop();
                }
                KeyCode::Enter => {
                    submit_prompt(app, engine.clone(), query_tx)?;
                }
                KeyCode::Char(ch) => {
                    if !app.in_flight {
                        app.input.push(ch);
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

fn submit_prompt(
    app: &mut App,
    engine: std::sync::Arc<claude_query::QueryEngine>,
    query_tx: mpsc::UnboundedSender<QueryEvent>,
) -> anyhow::Result<()> {
    if app.in_flight {
        return Ok(());
    }

    let prompt = app.input.trim().to_string();
    if prompt.is_empty() {
        return Ok(());
    }
    app.input.clear();
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

fn render(f: &mut ratatui::Frame<'_>, app: &mut App) {
    let size = f.size();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
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

    // Input
    let input_block = Block::default().borders(Borders::ALL).title("Input");
    let input_inner_w = input_block.inner(chunks[2]).width.max(1) as usize;
    let visible = take_last_chars(&app.input, input_inner_w);
    let input = Paragraph::new(visible.clone()).block(input_block);
    f.render_widget(input, chunks[2]);

    let cursor_x = visible.chars().count().min(input_inner_w) as u16;
    let cursor_y = chunks[2].y + 1;
    let cursor_x = chunks[2].x + 1 + cursor_x;
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
    let scroll_hint = if app.scroll_follow {
        ""
    } else {
        " • scroll locked (End to follow)"
    };
    let status = Line::from(format!("{spin} {}{scroll_hint}", app.status))
        .style(Style::default().fg(Color::Gray).add_modifier(Modifier::DIM));
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

fn take_last_chars(s: &str, max: usize) -> String {
    let max = max.max(1);
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    s.chars().skip(count - max).collect()
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
