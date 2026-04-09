mod args;
mod tui;

use anyhow::Context as _;
use clap::Parser;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

use crate::args::{
    Args, AuthCommand, Command, McpCommand, McpTransport, OutputFormat, SettingsScope,
};
use claude_core::types::permissions::PermissionMode;
use claude_core::types::{
    ids::SessionId,
    message::{ContentBlock, Message, UserMessage},
};
use std::collections::HashMap;
use std::io::IsTerminal as _;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

// Week 8: prefer a single-threaded runtime for lower startup overhead in a CLI.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();

    let res = run(&args).await;
    if let Err(err) = res {
        render_error(&args, &err);
        std::process::exit(exit_code_for(&err));
    }
}

async fn run(args: &Args) -> anyhow::Result<()> {
    if let Some(cwd) = &args.cwd {
        std::env::set_current_dir(cwd)
            .with_context(|| format!("setting --cwd to {}", cwd.display()))?;
    }

    // Week 1: config plumbing exists and is exercised on startup.
    let global_path = claude_core::config::global::default_global_config_path()?;
    let mut global_cfg = claude_core::config::global::load_global_config(&global_path)
        .with_context(|| format!("loading global config at {global_path:?}"))?;

    if let Some(cmd) = args.command.as_ref() {
        match cmd {
            Command::Auth { command } => match command {
                AuthCommand::Login => {
                    run_oauth_login(&global_path, &mut global_cfg).await?;
                    return Ok(());
                }
                AuthCommand::Logout => {
                    run_auth_logout(&global_path, &mut global_cfg)?;
                    return Ok(());
                }
            },
            Command::Doctor => {
                run_doctor(args, &global_path, &mut global_cfg).await?;
                return Ok(());
            }
            Command::Mcp { command } => {
                run_mcp_command(args, command).await?;
                return Ok(());
            }
        }
    }

    let settings = load_effective_settings(args)?;

    // Apply env vars from settings early (Week 3+).
    if let Some(env) = &settings.env {
        for (k, v) in env {
            // Rust 2024: mutating the process environment is `unsafe` because other
            // threads (including those spawned by the async runtime) may observe it.
            // We only do this once at startup for compatibility with the TS CLI.
            unsafe {
                std::env::set_var(k, v);
            }
        }
    }

    let stdin_is_tty = std::io::stdin().is_terminal();
    let prompt = match args
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(p) => p.to_string(),
        None => {
            if stdin_is_tty && !args.print {
                let auth = claude_services::auth::resolve_auth(
                    &global_path,
                    &mut global_cfg,
                    &settings,
                    claude_services::auth::ResolveAuthOpts {
                        cli_api_key: args.api_key.as_deref(),
                        bare: args.bare,
                    },
                )
                .await?;

                tui::run_tui(args, &settings, auth).await?;
                return Ok(());
            }

            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            let buf = buf.trim().to_string();
            if buf.is_empty() {
                return Err(UsageError(
                    "no prompt provided (pass a positional prompt or pipe stdin)".to_string(),
                )
                .into());
            }
            buf
        }
    };

    let auth = claude_services::auth::resolve_auth(
        &global_path,
        &mut global_cfg,
        &settings,
        claude_services::auth::ResolveAuthOpts {
            cli_api_key: args.api_key.as_deref(),
            bare: args.bare,
        },
    )
    .await?;

    match args.output_format {
        OutputFormat::Text => {
            run_headless(&args, &settings, auth, &prompt, HeadlessOutput::Text).await?;
        }
        OutputFormat::StreamJson => {
            run_headless(&args, &settings, auth, &prompt, HeadlessOutput::StreamJson).await?;
        }
        OutputFormat::Json => {
            run_headless(&args, &settings, auth, &prompt, HeadlessOutput::Json).await?;
        }
    }
    Ok(())
}

fn exit_code_for(err: &anyhow::Error) -> i32 {
    if find_in_chain::<UsageError>(err).is_some() {
        return 2;
    }
    1
}

fn render_error(args: &Args, err: &anyhow::Error) {
    if let Some(usage) = find_in_chain::<UsageError>(err) {
        eprintln!("error: {}", usage.0);
        eprintln!("hint: run `claude-rs --help` for usage.");
        return;
    }

    if let Some(svc) = find_in_chain::<claude_services::ServicesError>(err) {
        render_services_error(args, svc, err);
        return;
    }

    if args.debug.is_some() {
        eprintln!("error: {err:?}");
    } else {
        eprintln!("error: {err}");
    }
}

fn render_services_error(args: &Args, svc: &claude_services::ServicesError, err: &anyhow::Error) {
    use claude_services::ServicesError;

    match svc {
        ServicesError::MissingAuth { .. } => {
            eprintln!("error: {svc}");
            eprintln!(
                "hint: set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN, or run `claude-rs auth login`."
            );
        }
        ServicesError::OAuthExpired => {
            eprintln!("error: {svc}");
            eprintln!(
                "hint: run `claude-rs auth login` to refresh OAuth, or set ANTHROPIC_API_KEY/ANTHROPIC_AUTH_TOKEN."
            );
        }
        ServicesError::ApiKeyHelper { .. } => {
            eprintln!("error: {svc}");
            eprintln!(
                "hint: check your settings `apiKeyHelper` or set ANTHROPIC_API_KEY/ANTHROPIC_AUTH_TOKEN."
            );
        }
        ServicesError::ApiStatus { status, body } => {
            let body = one_line_preview(body, 400);
            if *status == 0 {
                eprintln!("error: API request failed: {body}");
            } else {
                eprintln!("error: API request failed (HTTP {status})");
                if !body.is_empty() {
                    eprintln!("details: {body}");
                }
            }

            match *status {
                401 | 403 => {
                    eprintln!(
                        "hint: check ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN, or re-run `claude-rs auth login`."
                    );
                }
                404 | 405 => {
                    let base = std::env::var("ANTHROPIC_BASE_URL")
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
                    eprintln!(
                        "hint: check ANTHROPIC_BASE_URL (currently {base}); it must support POST /v1/messages."
                    );
                }
                429 | 529 => {
                    eprintln!(
                        "hint: you're being rate limited / overloaded; retry later (the CLI retries a few times automatically)."
                    );
                }
                _ => {}
            }
        }
        ServicesError::Http { .. } | ServicesError::EventStream { .. } => {
            eprintln!("error: {svc}");
            eprintln!("hint: check network connectivity, proxy settings, and ANTHROPIC_BASE_URL.");
        }
        _ => {
            if args.debug.is_some() {
                eprintln!("error: {err:?}");
            } else {
                eprintln!("error: {svc}");
            }
        }
    }

    if args.debug.is_some() {
        eprintln!("\n(debug) {err:?}");
    }
}

fn find_in_chain<T: std::error::Error + 'static>(err: &anyhow::Error) -> Option<&T> {
    err.chain().find_map(|e| e.downcast_ref::<T>())
}

fn one_line_preview(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let normalized = s
        .chars()
        .map(|ch| if ch.is_ascii_whitespace() { ' ' } else { ch })
        .collect::<String>();
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, max_chars)
}

#[derive(Debug, Clone, Copy)]
enum HeadlessOutput {
    Text,
    Json,
    StreamJson,
}

async fn run_headless(
    args: &Args,
    settings: &claude_core::config::settings::Settings,
    auth: claude_services::auth::AuthMode,
    prompt: &str,
    output: HeadlessOutput,
) -> anyhow::Result<()> {
    let client = claude_services::api::AnthropicClient::new(None);
    let client_for_hooks = client.clone();
    let auth_for_hooks = auth.clone();
    let model = resolve_model(args.model.clone(), settings.model.clone());
    let mem_hook_enabled = !args.bare && is_env_truthy("CLAUDE_RS_EXTRACT_MEMORIES");

    let max_tokens = args.max_tokens.unwrap_or(1024);
    let max_turns = args.max_turns.unwrap_or(8);

    let system_prompt = load_system_prompt_override(args)?;
    let append_system_prompt = load_append_system_prompt(args)?;

    let cwd = std::env::current_dir()?;
    let cwd_for_hooks = cwd.clone();

    let (session_id, session_path, mut history) = resolve_session(args, &cwd)?;

    // Persist the user prompt before starting the request so an interrupted run
    // still leaves a resumable transcript.
    let user_msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
    });
    if let Err(err) =
        claude_core::history::append_session_messages(&session_path, &[user_msg.clone()])
    {
        log_warn_if_debug(args, format!("failed to persist session history: {err}"));
    }

    history.push(user_msg);

    let permission_mode = args
        .permission_mode
        .or(settings.permission_mode)
        .unwrap_or(PermissionMode::Default);

    let mut allowed_tools = settings.allowed_tools.clone().unwrap_or_default();
    allowed_tools.extend(args.allowed_tools.clone());

    let mut disallowed_tools = settings.disallowed_tools.clone().unwrap_or_default();
    disallowed_tools.extend(args.disallowed_tools.clone());

    let always_allow_tools = settings.always_allow_tools.clone().unwrap_or_default();

    let mcp_servers = resolve_mcp_servers(args, settings)?;

    let engine = claude_query::QueryEngine::new(
        client,
        auth,
        model.clone(),
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

    match output {
        HeadlessOutput::Text => {
            use std::io::Write as _;

            let result = engine
                .run_with_history(history, |event| {
                    if let Some(text) = extract_text_delta(event) {
                        print!("{text}");
                        std::io::stdout().flush().ok();
                    }
                    Ok(())
                })
                .await?;

            println!();
            persist_session_delta(args, session_id, &session_path, &result)?;
            print_cost_summary(&result);
            if let Err(err) = maybe_extract_memories_stop_hook(
                args,
                &client_for_hooks,
                &auth_for_hooks,
                &model,
                &cwd_for_hooks,
                prompt,
                &result,
            )
            .await
            {
                if mem_hook_enabled && args.debug.is_none() {
                    eprintln!("warn: memory extraction stop hook failed: {err}");
                }
                log_warn_if_debug(args, format!("memory stop hook failed: {err}"));
            }
            Ok(())
        }
        HeadlessOutput::StreamJson => {
            use std::io::Write as _;

            let result = engine
                .run_with_history(history, |event| {
                    let line = serde_json::to_string(event)?;
                    println!("{line}");
                    std::io::stdout().flush().ok();
                    Ok(())
                })
                .await?;

            persist_session_delta(args, session_id, &session_path, &result)?;
            print_cost_summary(&result);
            if let Err(err) = maybe_extract_memories_stop_hook(
                args,
                &client_for_hooks,
                &auth_for_hooks,
                &model,
                &cwd_for_hooks,
                prompt,
                &result,
            )
            .await
            {
                if mem_hook_enabled && args.debug.is_none() {
                    eprintln!("warn: memory extraction stop hook failed: {err}");
                }
                log_warn_if_debug(args, format!("memory stop hook failed: {err}"));
            }
            Ok(())
        }
        HeadlessOutput::Json => {
            let result = engine.run_with_history(history, |_event| Ok(())).await?;
            let out = serde_json::json!({ "text": result.text });
            println!("{}", serde_json::to_string_pretty(&out)?);
            persist_session_delta(args, session_id, &session_path, &result)?;
            print_cost_summary(&result);
            if let Err(err) = maybe_extract_memories_stop_hook(
                args,
                &client_for_hooks,
                &auth_for_hooks,
                &model,
                &cwd_for_hooks,
                prompt,
                &result,
            )
            .await
            {
                if mem_hook_enabled && args.debug.is_none() {
                    eprintln!("warn: memory extraction stop hook failed: {err}");
                }
                log_warn_if_debug(args, format!("memory stop hook failed: {err}"));
            }
            Ok(())
        }
    }
}

async fn run_oauth_login(
    global_path: &std::path::Path,
    global_cfg: &mut claude_core::config::global::GlobalConfig,
) -> anyhow::Result<()> {
    let flow = claude_services::auth::build_manual_oauth_authorize_url();

    println!("Open this URL in your browser:\n\n{}\n", flow.authorize_url);
    println!("After signing in, paste the full redirect URL here and press Enter:");

    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    stdin.read_line(&mut line).await?;
    let line = line.trim();
    if line.is_empty() {
        anyhow::bail!("no redirect URL provided");
    }

    let parsed = claude_services::auth::parse_oauth_redirect_url(line)?;
    if parsed.state != flow.state {
        anyhow::bail!("state mismatch; restart `claude-rs auth` and try again");
    }

    let token = claude_services::auth::exchange_code_for_tokens(
        &parsed.authorization_code,
        &parsed.state,
        &flow.code_verifier,
    )
    .await?;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    global_cfg.oauth_access_token = Some(token.access_token);
    global_cfg.oauth_refresh_token = token.refresh_token;
    global_cfg.oauth_expires_at =
        Some(now_ms.saturating_add(token.expires_in.saturating_mul(1000)));

    claude_core::config::global::save_global_config(global_path, global_cfg)?;

    println!("Saved OAuth credentials to {:?}", global_path);
    Ok(())
}

fn run_auth_logout(
    global_path: &std::path::Path,
    global_cfg: &mut claude_core::config::global::GlobalConfig,
) -> anyhow::Result<()> {
    claude_services::auth::clear_oauth_tokens(global_cfg);
    claude_services::auth::clear_api_key(global_cfg);
    claude_core::config::global::save_global_config(global_path, global_cfg)?;

    println!("Cleared stored credentials in {:?}", global_path);
    println!("Note: environment variables (e.g. ANTHROPIC_API_KEY) are not modified.");
    Ok(())
}

async fn run_doctor(
    args: &Args,
    global_path: &std::path::Path,
    global_cfg: &mut claude_core::config::global::GlobalConfig,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let project_root = claude_core::history::project_root_for_cwd(&cwd);
    let config_home = claude_core::paths::claude_config_home_dir()?;

    println!("claude-rs doctor\n");
    println!("cwd: {}", cwd.display());
    println!("project_root: {}", project_root.display());
    println!("config_home: {}", config_home.display());
    println!("global_config: {}", global_path.display());

    let user_settings = config_home.join("settings.json");
    let project_settings = project_root.join(".claude").join("settings.json");
    let local_settings = project_root.join(".claude").join("settings.local.json");

    println!("\nsettings (user): {}", user_settings.display());
    println!("settings (project): {}", project_settings.display());
    println!("settings (local): {}", local_settings.display());

    let _git_ok = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "\ngit: {}",
        if _git_ok { "ok" } else { "missing/unavailable" }
    );

    println!(
        "ANTHROPIC_BASE_URL: {}",
        std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .unwrap_or_else(|| "(default)".to_string())
    );
    println!(
        "ANTHROPIC_MODEL: {}",
        std::env::var("ANTHROPIC_MODEL")
            .ok()
            .unwrap_or_else(|| "(unset)".to_string())
    );

    let settings = load_effective_settings(args)?;
    let mcp_count = settings.mcp_servers.as_ref().map(|m| m.len()).unwrap_or(0);
    println!("mcp_servers: {mcp_count}");

    let auth_res = claude_services::auth::resolve_auth(
        global_path,
        global_cfg,
        &settings,
        claude_services::auth::ResolveAuthOpts {
            cli_api_key: args.api_key.as_deref(),
            bare: args.bare,
        },
    )
    .await;

    match auth_res {
        Ok(mode) => {
            let kind = match mode {
                claude_services::auth::AuthMode::ApiKey(_) => "api_key",
                claude_services::auth::AuthMode::AuthToken(_) => "auth_token",
                claude_services::auth::AuthMode::OAuthToken(_) => "oauth_token",
            };
            println!("auth: ok ({kind})");
        }
        Err(err) => {
            println!("auth: error ({err})");
        }
    }

    Ok(())
}

async fn run_mcp_command(args: &Args, command: &McpCommand) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;

    match command {
        McpCommand::List { scope } => {
            let path = settings_path_for_scope(&cwd, *scope)?;
            let root = load_settings_json_object_or_empty(&path)?;

            let servers = root
                .get("mcpServers")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            if servers.is_empty() {
                println!(
                    "(no MCP servers in {:?} settings at {})",
                    scope,
                    path.display()
                );
                return Ok(());
            }

            for (name, cfg) in servers {
                let ty = infer_mcp_transport(&cfg);
                println!("{name}\t{ty}");
            }
            Ok(())
        }
        McpCommand::Add {
            name,
            command_or_url,
            args: cmd_args,
            scope,
            transport,
            env,
            header,
        } => {
            let path = settings_path_for_scope(&cwd, *scope)?;
            let _lock = lock_settings_path(&path)?;
            let mut root = load_settings_json_object_or_empty(&path)?;

            let cfg = match transport {
                McpTransport::Stdio => {
                    let mut obj = serde_json::Map::new();
                    obj.insert(
                        "type".to_string(),
                        serde_json::Value::String("stdio".to_string()),
                    );
                    obj.insert(
                        "command".to_string(),
                        serde_json::Value::String(command_or_url.clone()),
                    );
                    if !cmd_args.is_empty() {
                        obj.insert(
                            "args".to_string(),
                            serde_json::Value::Array(
                                cmd_args
                                    .iter()
                                    .cloned()
                                    .map(serde_json::Value::String)
                                    .collect(),
                            ),
                        );
                    }
                    if !env.is_empty() {
                        let env_map = parse_env_kv(env)?;
                        obj.insert("env".to_string(), serde_json::to_value(env_map)?);
                    }
                    serde_json::Value::Object(obj)
                }
                McpTransport::Sse => {
                    if !cmd_args.is_empty() {
                        log_warn_if_debug(args, "mcp add: ignoring extra args for sse transport");
                    }
                    let mut obj = serde_json::Map::new();
                    obj.insert(
                        "type".to_string(),
                        serde_json::Value::String("sse".to_string()),
                    );
                    obj.insert(
                        "url".to_string(),
                        serde_json::Value::String(command_or_url.clone()),
                    );
                    if !header.is_empty() {
                        let headers = parse_headers(header)?;
                        obj.insert("headers".to_string(), serde_json::to_value(headers)?);
                    }
                    serde_json::Value::Object(obj)
                }
                McpTransport::Ws => {
                    if !cmd_args.is_empty() {
                        log_warn_if_debug(args, "mcp add: ignoring extra args for ws transport");
                    }
                    let mut obj = serde_json::Map::new();
                    obj.insert(
                        "type".to_string(),
                        serde_json::Value::String("ws".to_string()),
                    );
                    obj.insert(
                        "url".to_string(),
                        serde_json::Value::String(command_or_url.clone()),
                    );
                    if !header.is_empty() {
                        let headers = parse_headers(header)?;
                        obj.insert("headers".to_string(), serde_json::to_value(headers)?);
                    }
                    serde_json::Value::Object(obj)
                }
            };

            let Some(root_obj) = root.as_object_mut() else {
                anyhow::bail!("settings root must be a JSON object: {}", path.display());
            };

            let servers_val = root_obj
                .entry("mcpServers".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            let Some(servers_obj) = servers_val.as_object_mut() else {
                anyhow::bail!(
                    "settings mcpServers must be a JSON object: {}",
                    path.display()
                );
            };

            servers_obj.insert(name.clone(), cfg);

            save_settings_json(&path, &root)?;
            println!(
                "Saved MCP server {name} to {:?} settings at {}",
                scope,
                path.display()
            );
            Ok(())
        }
        McpCommand::Remove { name, scope } => {
            let path = settings_path_for_scope(&cwd, *scope)?;
            let _lock = lock_settings_path(&path)?;
            let mut root = load_settings_json_object_or_empty(&path)?;

            let Some(root_obj) = root.as_object_mut() else {
                anyhow::bail!("settings root must be a JSON object: {}", path.display());
            };

            let Some(servers_val) = root_obj.get_mut("mcpServers") else {
                println!("MCP server {name} was not present in {:?} settings.", scope);
                return Ok(());
            };
            let Some(servers_obj) = servers_val.as_object_mut() else {
                anyhow::bail!(
                    "settings mcpServers must be a JSON object: {}",
                    path.display()
                );
            };

            if servers_obj.remove(name.as_str()).is_none() {
                println!("MCP server {name} was not present in {:?} settings.", scope);
                return Ok(());
            }

            save_settings_json(&path, &root)?;
            println!(
                "Removed MCP server {name} from {:?} settings at {}",
                scope,
                path.display()
            );
            Ok(())
        }
    }
}

fn infer_mcp_transport(cfg: &serde_json::Value) -> &'static str {
    if cfg.get("command").is_some() {
        return "stdio";
    }

    if let Some(ty) = cfg.get("type").and_then(|v| v.as_str()) {
        return match ty {
            "stdio" => "stdio",
            "sse" => "sse",
            "ws" => "ws",
            _ => "unknown",
        };
    }

    if cfg.get("url").is_some() {
        return "sse/ws";
    }

    "unknown"
}

fn lock_settings_path(path: &std::path::Path) -> anyhow::Result<claude_core::lockfile::LockGuard> {
    use std::ffi::OsStr;
    use std::time::Duration;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("settings.json"))
        .to_string_lossy()
        .to_string();
    let lock_path = path.with_file_name(format!("{file_name}.lock"));

    Ok(claude_core::lockfile::acquire_lock(
        &lock_path,
        Duration::from_secs(5),
    )?)
}

fn load_settings_json_object_or_empty(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        Err(err) => return Err(err.into()),
    };

    if bytes.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }

    let root: serde_json::Value = serde_json::from_slice(&bytes)?;
    if root.as_object().is_none() {
        anyhow::bail!("settings root must be a JSON object: {}", path.display());
    }
    Ok(root)
}

fn save_settings_json(path: &std::path::Path, root: &serde_json::Value) -> anyhow::Result<()> {
    if root.as_object().is_none() {
        anyhow::bail!("settings root must be a JSON object: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut bytes = serde_json::to_vec_pretty(root)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}

fn settings_path_for_scope(
    cwd: &std::path::Path,
    scope: SettingsScope,
) -> anyhow::Result<std::path::PathBuf> {
    let project_root = claude_core::history::project_root_for_cwd(cwd);
    let config_home = claude_core::paths::claude_config_home_dir()?;

    Ok(match scope {
        SettingsScope::User => config_home.join("settings.json"),
        SettingsScope::Project => project_root.join(".claude").join("settings.json"),
        SettingsScope::Local => project_root.join(".claude").join("settings.local.json"),
    })
}

fn parse_env_kv(pairs: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for raw in pairs {
        let Some((k, v)) = raw.split_once('=') else {
            anyhow::bail!("invalid --env entry (expected KEY=value): {raw}");
        };
        let k = k.trim();
        if k.is_empty() {
            anyhow::bail!("invalid --env entry (empty key): {raw}");
        }
        out.insert(k.to_string(), v.trim().to_string());
    }
    Ok(out)
}

fn parse_headers(pairs: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for raw in pairs {
        let Some((k, v)) = raw.split_once(':') else {
            anyhow::bail!("invalid --header entry (expected \"Key: Value\"): {raw}");
        };
        let k = k.trim();
        if k.is_empty() {
            anyhow::bail!("invalid --header entry (empty key): {raw}");
        }
        out.insert(k.to_string(), v.trim().to_string());
    }
    Ok(out)
}

fn resolve_session(
    args: &Args,
    cwd: &std::path::Path,
) -> anyhow::Result<(SessionId, std::path::PathBuf, Vec<Message>)> {
    if args.continue_session && args.resume.is_some() {
        anyhow::bail!("--continue and --resume cannot be used together");
    }

    if let Some(raw) = &args.resume {
        let id: SessionId = raw
            .trim()
            .parse()
            .with_context(|| "parsing --resume session id")?;
        let path = claude_core::history::session_file_path(cwd, id)?;
        let history = claude_core::history::load_session_messages(&path)?;
        return Ok((id, path, history));
    }

    if args.continue_session {
        if let Some((id, path)) = claude_core::history::find_latest_session(cwd)? {
            let history = claude_core::history::load_session_messages(&path)?;
            return Ok((id, path, history));
        }
    }

    let id = SessionId::new();
    let path = claude_core::history::session_file_path(cwd, id)?;
    Ok((id, path, Vec::new()))
}

fn persist_session_delta(
    args: &Args,
    session_id: SessionId,
    session_path: &std::path::Path,
    result: &claude_query::RunResult,
) -> anyhow::Result<()> {
    if result.new_messages.is_empty() {
        // Still write meta so resume UX has something to show.
        write_session_meta(args, session_id, session_path, result);
        completion_check(result);
        return Ok(());
    }

    if let Err(err) =
        claude_core::history::append_session_messages(session_path, &result.new_messages)
    {
        log_warn_if_debug(
            args,
            format!("failed to append session {session_id} messages: {err}"),
        );
    }

    write_session_meta(args, session_id, session_path, result);
    completion_check(result);
    Ok(())
}

fn completion_check(result: &claude_query::RunResult) {
    match result.stop_reason {
        Some(claude_core::types::message::StopReason::MaxTokens) => {
            eprintln!(
                "warn: response may be incomplete (stop_reason=max_tokens). Consider increasing --max-tokens or --max-turns."
            );
        }
        Some(claude_core::types::message::StopReason::ToolUse) => {
            eprintln!("warn: stopped while tools were requested (stop_reason=tool_use).");
        }
        _ => {}
    }
}

fn write_session_meta(
    args: &Args,
    session_id: SessionId,
    session_path: &std::path::Path,
    result: &claude_query::RunResult,
) {
    let meta_path = session_path.with_extension("meta.json");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let preview = truncate_chars(&result.text, 800);

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

    let bytes = match serde_json::to_vec_pretty(&meta) {
        Ok(b) => b,
        Err(err) => {
            log_warn_if_debug(args, format!("failed to serialize session meta: {err}"));
            return;
        }
    };

    if let Err(err) = std::fs::write(&meta_path, bytes) {
        log_warn_if_debug(
            args,
            format!(
                "failed to write session meta {}: {err}",
                meta_path.display()
            ),
        );
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

fn log_warn_if_debug(args: &Args, msg: impl AsRef<str>) {
    if args.debug.is_some() {
        eprintln!("warn: {}", msg.as_ref());
    }
}

fn load_effective_settings(args: &Args) -> anyhow::Result<claude_core::config::settings::Settings> {
    let cwd = std::env::current_dir()?;
    let project_root = claude_core::history::project_root_for_cwd(&cwd);
    let config_home = claude_core::paths::claude_config_home_dir()?;

    let user_settings = config_home.join("settings.json");
    let project_settings = project_root.join(".claude").join("settings.json");
    let local_settings = project_root.join(".claude").join("settings.local.json");

    let mut layers: Vec<claude_core::config::settings::Settings> = Vec::new();
    layers.push(claude_core::config::settings::load_settings_file(
        &user_settings,
    )?);
    layers.push(claude_core::config::settings::load_settings_file(
        &project_settings,
    )?);
    layers.push(claude_core::config::settings::load_settings_file(
        &local_settings,
    )?);

    if let Some(raw) = &args.settings {
        layers.push(claude_core::config::settings::load_settings_arg(raw)?);
    }

    Ok(claude_core::config::settings::Settings::merge(&layers))
}

fn extract_text_delta(event: &serde_json::Value) -> Option<&str> {
    let ty = event.get("type")?.as_str()?;
    if ty != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    let delta_ty = delta.get("type")?.as_str()?;
    if delta_ty != "text_delta" {
        return None;
    }
    delta.get("text")?.as_str()
}

fn extract_thinking_delta(event: &serde_json::Value) -> Option<&str> {
    let ty = event.get("type")?.as_str()?;
    if ty != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    let delta_ty = delta.get("type")?.as_str()?;
    if delta_ty != "thinking_delta" {
        return None;
    }
    delta.get("thinking")?.as_str()
}

fn load_system_prompt_override(args: &Args) -> anyhow::Result<Option<String>> {
    if args.system_prompt.is_some() {
        return Ok(args.system_prompt.clone());
    }
    if let Some(path) = &args.system_prompt_file {
        return Ok(Some(std::fs::read_to_string(path).with_context(|| {
            format!("reading --system-prompt-file {}", path.display())
        })?));
    }
    Ok(None)
}

fn load_append_system_prompt(args: &Args) -> anyhow::Result<Option<String>> {
    let mut out = args.append_system_prompt.clone();
    if let Some(path) = &args.append_system_prompt_file {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading --append-system-prompt-file {}", path.display()))?;
        match &mut out {
            Some(existing) => {
                existing.push('\n');
                existing.push_str(&content);
            }
            None => out = Some(content),
        }
    }
    Ok(out)
}

fn print_cost_summary(result: &claude_query::RunResult) {
    // Always write to stderr so stdout is clean for scripting.
    if let Some(cost) = result.cost_usd {
        eprintln!(
            "usage: in={} out={} cost=${:.4} model={} turns={}",
            result.usage.input_tokens, result.usage.output_tokens, cost, result.model, result.turns
        );
    } else {
        eprintln!(
            "usage: in={} out={} model={} turns={}",
            result.usage.input_tokens, result.usage.output_tokens, result.model, result.turns
        );
    }
}

async fn maybe_extract_memories_stop_hook(
    args: &Args,
    client: &claude_services::api::AnthropicClient,
    auth: &claude_services::auth::AuthMode,
    model: &str,
    cwd: &std::path::Path,
    user_prompt: &str,
    result: &claude_query::RunResult,
) -> anyhow::Result<()> {
    if args.bare {
        return Ok(());
    }

    if !is_env_truthy("CLAUDE_RS_EXTRACT_MEMORIES") {
        return Ok(());
    }

    let assistant_text = result.text.trim();
    if assistant_text.is_empty() {
        return Ok(());
    }

    let extracted = extract_memories_text(client, auth, model, user_prompt, assistant_text).await?;
    let lines = normalize_memory_lines(&extracted);
    if lines.is_empty() {
        return Ok(());
    }

    let path = append_memories_to_daily_log(cwd, &lines)?;
    log_warn_if_debug(
        args,
        format!(
            "saved {} memory entr{} to {}",
            lines.len(),
            if lines.len() == 1 { "y" } else { "ies" },
            path.display()
        ),
    );
    Ok(())
}

async fn extract_memories_text(
    client: &claude_services::api::AnthropicClient,
    auth: &claude_services::auth::AuthMode,
    model: &str,
    user_prompt: &str,
    assistant_text: &str,
) -> anyhow::Result<String> {
    use claude_services::api::MessagesRequest;

    let mut turn = String::new();
    turn.push_str("[user]\n");
    turn.push_str(user_prompt.trim());
    turn.push_str("\n\n[assistant]\n");
    turn.push_str(assistant_text.trim());

    let mut prompt = String::new();
    prompt.push_str("Extract durable memories to save for future coding sessions.\n");
    prompt.push_str("- Return 0-5 concise bullet points.\n");
    prompt.push_str("- Only include stable user preferences, constraints, and project facts that are not obvious from the repo.\n");
    prompt.push_str("- Do NOT include secrets (API keys, tokens, passwords). If any secrets appear, return an empty response.\n");
    prompt.push_str("- Return only the bullet points. No preamble.\n\n");
    prompt.push_str("# Turn\n\n");
    prompt.push_str(&turn);

    let req = MessagesRequest {
        model: model.to_string(),
        max_tokens: 512,
        system: Some("You are a memory extraction agent. Follow instructions exactly.".to_string()),
        tools: None,
        messages: vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: prompt }],
        })],
        stream: true,
    };

    let mut parser = claude_query::stream_parser::StreamParser::default();
    client
        .stream_messages(auth, &req, &mut |raw| {
            parser
                .process_event(&raw)
                .map_err(|e| claude_services::ServicesError::Callback {
                    detail: e.to_string(),
                })?;
            Ok(())
        })
        .await?;

    let parsed = parser.finish();
    Ok(parsed.text.trim().to_string())
}

fn normalize_memory_lines(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut s = line.trim();
        if s.is_empty() {
            continue;
        }

        if let Some(rest) = s.strip_prefix('-') {
            s = rest.trim();
        } else if let Some(rest) = s.strip_prefix('*') {
            s = rest.trim();
        }

        if s.is_empty() {
            continue;
        }

        let lower = s.to_ascii_lowercase();
        if lower == "none" || lower == "no memories" {
            continue;
        }

        out.push(s.to_string());
        if out.len() >= 8 {
            break;
        }
    }
    out
}

fn append_memories_to_daily_log(
    cwd: &std::path::Path,
    memories: &[String],
) -> anyhow::Result<std::path::PathBuf> {
    use chrono::{Datelike as _, Timelike as _, Utc};
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::time::Duration;

    let now = Utc::now();
    let project_dir = claude_core::history::project_dir_for_cwd(cwd)?;
    let mem_dir = project_dir.join("memory");

    let y = format!("{:04}", now.year());
    let m = format!("{:02}", now.month());
    let d = format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day());

    let log_path = mem_dir
        .join("logs")
        .join(&y)
        .join(&m)
        .join(format!("{d}.md"));

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = log_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("memories.md"))
        .to_string_lossy()
        .to_string();
    let lock_path = log_path.with_file_name(format!("{file_name}.lock"));
    let _lock = claude_core::lockfile::acquire_lock(&lock_path, Duration::from_secs(5))?;

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let ts = format!("{:02}:{:02}Z", now.hour(), now.minute());
    for mem in memories {
        writeln!(f, "- {ts} {mem}")?;
    }

    Ok(log_path)
}

fn is_env_truthy(key: &str) -> bool {
    let Ok(v) = std::env::var(key) else {
        return false;
    };
    let v = v.trim().to_ascii_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

fn resolve_model(cli_model: Option<String>, settings_model: Option<String>) -> String {
    sanitize_opt(cli_model)
        .or_else(anthropic_model_from_env)
        .or_else(|| sanitize_opt(settings_model))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

fn anthropic_model_from_env() -> Option<String> {
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn sanitize_opt(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn resolve_mcp_servers(
    args: &Args,
    settings: &claude_core::config::settings::Settings,
) -> anyhow::Result<HashMap<String, claude_core::config::mcp::McpServerConfig>> {
    let mut out: HashMap<String, claude_core::config::mcp::McpServerConfig> = HashMap::new();

    if !args.strict_mcp_config {
        if let Some(servers) = &settings.mcp_servers {
            out.extend(servers.clone());
        }
    }

    for raw in &args.mcp_config {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let cfg = load_mcp_config_arg(raw)?;
        out.extend(cfg.mcp_servers);
    }

    Ok(out)
}

fn load_mcp_config_arg(raw: &str) -> anyhow::Result<claude_core::config::mcp::McpJsonConfig> {
    let as_path = std::path::PathBuf::from(raw);
    if as_path.exists() {
        let bytes = std::fs::read(&as_path)
            .with_context(|| format!("reading MCP config {}", as_path.display()))?;
        return Ok(serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing MCP config {}", as_path.display()))?);
    }

    Ok(serde_json::from_str(raw).with_context(|| "parsing MCP config JSON")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn default_model_is_used_when_env_and_settings_empty() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("ANTHROPIC_MODEL");
        }
        assert_eq!(resolve_model(None, None), DEFAULT_MODEL);
    }

    #[test]
    fn env_model_is_used_when_provided() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_MODEL", " claude-3-5-haiku-20241022 ");
        }
        assert_eq!(
            resolve_model(None, Some("claude-opus-4-6".to_string())),
            "claude-3-5-haiku-20241022"
        );
        unsafe {
            std::env::remove_var("ANTHROPIC_MODEL");
        }
    }

    #[test]
    fn cli_model_overrides_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_MODEL", "claude-3-5-haiku-20241022");
        }
        assert_eq!(
            resolve_model(Some("claude-opus-4-6".to_string()), None),
            "claude-opus-4-6"
        );
        unsafe {
            std::env::remove_var("ANTHROPIC_MODEL");
        }
    }
}
