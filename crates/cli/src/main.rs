mod args;

use anyhow::Context as _;
use clap::Parser;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

use crate::args::{Args, Command, OutputFormat};
use claude_core::types::permissions::PermissionMode;
use std::collections::HashMap;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

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
    let model = resolve_model(args.model.clone(), settings.model.clone());

    let max_tokens = args.max_tokens.unwrap_or(1024);
    let max_turns = args.max_turns.unwrap_or(8);

    let system_prompt = load_system_prompt_override(args)?;
    let append_system_prompt = load_append_system_prompt(args)?;

    let cwd = std::env::current_dir()?;

    let permission_mode = args
        .permission_mode
        .or(settings.permission_mode)
        .unwrap_or(PermissionMode::Default);

    let mut allowed_tools = settings.allowed_tools.clone().unwrap_or_default();
    allowed_tools.extend(args.allowed_tools.clone());

    let mut disallowed_tools = settings.disallowed_tools.clone().unwrap_or_default();
    disallowed_tools.extend(args.disallowed_tools.clone());

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
            mcp_servers,
            agent_depth: 0,
            max_agent_depth: 2,
        },
    )?;

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
