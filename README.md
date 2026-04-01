# claude-cli-rs (Weeks 1–3)

This folder contains the Rust rewrite workspace for the **headless/CLI-only** mode.

## Quick Start

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"
cargo test
cargo run -p claude-cli -- --help
```

## Run A Prompt (API Key)

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"

export ANTHROPIC_API_KEY="..."
cargo run -p claude-cli -- -p "Hello"
```

## OAuth Login (Manual PKCE)

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"

cargo run -p claude-cli -- auth
# Open the printed URL, then paste the redirect URL when prompted.

cargo run -p claude-cli -- -p "Hello"
```

## Notes

- Global config path (current stub): `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`
- Week 2 implements basic **API key auth + OAuth token flow + streaming API client**.
- Week 3 adds a minimal **query engine** (system prompt + git/CLAUDE.md context injection, continuation on `max_tokens`, and cost tracking printed to stderr).
