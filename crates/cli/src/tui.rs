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
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::args::{Args, InputFormat, OutputFormat};

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

#[derive(Debug)]
struct App {
    input: String,
    status: String,
    spinner_idx: usize,
    in_flight: bool,
    active_assistant_idx: Option<usize>,

    transcript: Vec<ChatEntry>,
    history: Vec<Message>,

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

    let mut transcript = transcript_from_history(&history);
    if transcript.is_empty() {
        transcript.push(ChatEntry {
            role: Role::System,
            text: "Ctrl+C to exit. Type a prompt and press Enter.".to_string(),
        });
    }

    let mut app = App {
        input: String::new(),
        status: "ready".to_string(),
        spinner_idx: 0,
        in_flight: false,
        active_assistant_idx: None,
        transcript,
        history,
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
            .draw(|f| render(f, &app))
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
    app.transcript.push(ChatEntry {
        role: Role::Assistant,
        text: String::new(),
    });
    app.active_assistant_idx = Some(app.transcript.len().saturating_sub(1));

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
            let idx = app.active_assistant_idx.unwrap_or_else(|| {
                app.transcript.push(ChatEntry {
                    role: Role::Assistant,
                    text: String::new(),
                });
                app.transcript.len().saturating_sub(1)
            });
            if let Some(entry) = app.transcript.get_mut(idx) {
                entry.text.push_str(&delta);
            }
        }
        QueryEvent::Finished(result) => {
            app.in_flight = false;
            app.active_assistant_idx = None;
            app.history = result.history;

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
                }
            }

            app.status = format!("error: {}", crate::one_line_preview(&err, 160));
            app.transcript.push(ChatEntry {
                role: Role::System,
                text: format!("error: {err}"),
            });
        }
    }
}

fn render(f: &mut ratatui::Frame<'_>, app: &App) {
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
    let inner_w = msg_block.inner(chunks[1]).width.max(1) as usize;
    let inner_h = msg_block.inner(chunks[1]).height.max(1) as usize;

    let lines = transcript_lines(&app.transcript, inner_w);
    let scroll_y = lines.len().saturating_sub(inner_h);
    let text = Text::from(lines.into_iter().map(Line::raw).collect::<Vec<_>>());

    let messages = Paragraph::new(text)
        .block(msg_block)
        .scroll((scroll_y.min(u16::MAX as usize) as u16, 0));
    f.render_widget(messages, chunks[1]);

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
    let status = Line::from(format!("{spin} {}", app.status))
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

fn transcript_lines(entries: &[ChatEntry], width: usize) -> Vec<String> {
    let width = width.max(1);

    let mut out: Vec<String> = Vec::new();
    for (idx, e) in entries.iter().enumerate() {
        if idx > 0 {
            out.push(String::new());
        }

        let prefix = match e.role {
            Role::User => "You: ",
            Role::Assistant => "Claude: ",
            Role::System => "",
        };

        let mut first = true;
        for raw_line in e.text.split('\n') {
            if first {
                let line = format!("{prefix}{raw_line}");
                out.extend(wrap_by_chars(&line, width));
                first = false;
            } else {
                out.extend(wrap_by_chars(raw_line, width));
            }
        }
        if e.text.is_empty() && !prefix.is_empty() {
            out.push(prefix.trim_end().to_string());
        }
    }
    out
}

fn wrap_by_chars(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut col: usize = 0;

    for ch in s.chars() {
        buf.push(ch);
        col += 1;
        if col >= width {
            out.push(std::mem::take(&mut buf));
            col = 0;
        }
    }

    if !buf.is_empty() || out.is_empty() {
        out.push(buf);
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
