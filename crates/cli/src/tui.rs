use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
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
use tokio::sync::mpsc;

use crate::args::{Args, InputFormat, OutputFormat};

mod markdown;

use markdown::{MarkdownRenderer, StreamingMarkdown};

const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
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
}

#[derive(Debug)]
enum QueryEvent {
    TextDelta(String),
    Finished(claude_query::RunResult),
    Error(String),
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
    let (session_id, session_path, history) = crate::resolve_session(args, &cwd)?;

    let model = crate::resolve_model(args.model.clone(), settings.model.clone());

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
        scroll_top: 0,
        scroll_follow: true,
        last_msg_view_height: 0,
        line_offsets: Vec::new(),
        session_id,
        session_path,
        model: model.clone(),
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

    app.transcript.push(ChatEntry {
        role: Role::User,
        text: prompt.clone(),
    });
    app.rendered.push(RenderedEntry::new(Role::User));

    app.transcript.push(ChatEntry {
        role: Role::Assistant,
        text: String::new(),
    });
    app.rendered.push(RenderedEntry::new_streaming(Role::Assistant));
    app.active_assistant_idx = Some(app.transcript.len().saturating_sub(1));

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

    tokio::spawn(async move {
        let res = engine
            .run_with_history(history_for_engine, |event| {
                if let Some(text) = crate::extract_text_delta(event) {
                    let _ = query_tx.send(QueryEvent::TextDelta(text.to_string()));
                }
                Ok(())
            })
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
        QueryEvent::Finished(result) => {
            let finished_idx = app.active_assistant_idx;
            app.in_flight = false;
            app.active_assistant_idx = None;
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
    f.set_cursor(cursor_x, cursor_y);

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
}

fn transcript_from_history(history: &[Message]) -> Vec<ChatEntry> {
    let mut out = Vec::new();
    for msg in history {
        match msg {
            Message::User(UserMessage { content }) => {
                let text = extract_text_blocks(content);
                if !text.trim().is_empty() {
                    out.push(ChatEntry {
                        role: Role::User,
                        text,
                    });
                }
            }
            Message::Assistant(claude_core::types::message::AssistantMessage { content, .. }) => {
                let text = extract_text_blocks(content);
                if !text.trim().is_empty() {
                    out.push(ChatEntry {
                        role: Role::Assistant,
                        text,
                    });
                }
            }
        }
    }
    out
}

fn extract_text_blocks(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            _ => {}
        }
    }
    out
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
