mod args;

use anyhow::Context as _;
use clap::Parser;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

use crate::args::{Args, Command, OutputFormat};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(cwd) = &args.cwd {
        std::env::set_current_dir(cwd)
            .with_context(|| format!("setting --cwd to {}", cwd.display()))?;
    }

    // Week 1: config plumbing exists and is exercised on startup.
    let global_path = claude_core::config::global::default_global_config_path()?;
    let mut global_cfg = claude_core::config::global::load_global_config(&global_path)
        .with_context(|| format!("loading global config at {global_path:?}"))?;

    if let Some(cmd) = args.command {
        match cmd {
            Command::Auth => {
                run_oauth_login(&global_path, &mut global_cfg).await?;
                return Ok(());
            }
            Command::Doctor => {
                eprintln!("doctor: not implemented (Week 2+)");
                return Ok(());
            }
            Command::Mcp => {
                eprintln!("mcp: not implemented (Week 5+)");
                return Ok(());
            }
        }
    }

    if !args.print {
        eprintln!("interactive mode is not implemented in the Rust rewrite. Use -p/--print.");
        std::process::exit(2);
    }

    let settings = if let Some(raw) = &args.settings {
        claude_core::config::settings::load_settings_arg(raw)?
    } else {
        claude_core::config::settings::Settings::default()
    };

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

    let prompt = match args.prompt.as_deref() {
        Some(p) => p.to_string(),
        None => {
            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            let buf = buf.trim().to_string();
            if buf.is_empty() {
                anyhow::bail!("no prompt provided (pass a positional prompt or pipe stdin)");
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
    let model = args
        .model
        .clone()
        .or_else(|| settings.model.clone())
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    let max_tokens = args.max_tokens.unwrap_or(1024);
    let max_turns = args.max_turns.unwrap_or(8);

    let system_prompt = load_system_prompt_override(args)?;
    let append_system_prompt = load_append_system_prompt(args)?;

    let cwd = std::env::current_dir()?;

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
        },
    );

    match output {
        HeadlessOutput::Text => {
            use std::io::Write as _;

            let result = engine
                .run(prompt, |event| {
                    if let Some(text) = extract_text_delta(event) {
                        print!("{text}");
                        std::io::stdout().flush().ok();
                    }
                    Ok(())
                })
                .await?;

            println!();
            print_cost_summary(&result);
            Ok(())
        }
        HeadlessOutput::StreamJson => {
            use std::io::Write as _;

            let result = engine
                .run(prompt, |event| {
                    let line = serde_json::to_string(event)?;
                    println!("{line}");
                    std::io::stdout().flush().ok();
                    Ok(())
                })
                .await?;

            print_cost_summary(&result);
            Ok(())
        }
        HeadlessOutput::Json => {
            let result = engine.run(prompt, |_event| Ok(())).await?;
            let out = serde_json::json!({ "text": result.text });
            println!("{}", serde_json::to_string_pretty(&out)?);
            print_cost_summary(&result);
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
