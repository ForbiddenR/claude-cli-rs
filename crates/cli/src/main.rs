mod args;

use anyhow::Context as _;
use clap::Parser;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

use crate::args::{Args, Command, OutputFormat};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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

    let prompt = match args.prompt {
        Some(p) => p,
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

    let client = claude_services::api::AnthropicClient::new(None);
    let model = args.model.as_deref().unwrap_or("claude-sonnet-4-6");
    let max_tokens = args.max_tokens.unwrap_or(1024);

    match args.output_format {
        OutputFormat::Text => {
            use std::io::Write as _;

            client
                .stream_prompt(&auth, model, max_tokens, &prompt, |event| {
                    if let Some(text) = extract_text_delta(&event) {
                        print!("{text}");
                        std::io::stdout().flush().ok();
                    }
                    Ok(())
                })
                .await?;

            println!();
        }
        OutputFormat::StreamJson => {
            use std::io::Write as _;

            client
                .stream_prompt(&auth, model, max_tokens, &prompt, |event| {
                    let line = serde_json::to_string(&event)?;
                    println!("{line}");
                    std::io::stdout().flush().ok();
                    Ok(())
                })
                .await?;
        }
        OutputFormat::Json => {
            let mut text = String::new();
            client
                .stream_prompt(&auth, model, max_tokens, &prompt, |event| {
                    if let Some(delta) = extract_text_delta(&event) {
                        text.push_str(delta);
                    }
                    Ok(())
                })
                .await?;

            let out = serde_json::json!({ "text": text });
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
    }
    Ok(())
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
    global_cfg.oauth_expires_at = Some(now_ms.saturating_add(token.expires_in.saturating_mul(1000)));

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
