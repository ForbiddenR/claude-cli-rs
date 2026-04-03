# Migration Guide (TypeScript CLI -> `claude-rs`)

`claude-cli-rs/` is a **headless/CLI-only** Rust rewrite. It intentionally does **not** implement the TUI/Ink experience.

## Install / Build

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"
cargo build --release -p claude-cli
```

The binary will be at `target/release/claude-rs`.

## Common Command Mappings

- One-shot prompt (TypeScript `claude -p "..."`):

```bash
claude-rs "Hello"
```

- Piped stdin:

```bash
echo "Hello" | claude-rs
```

- JSON output:

```bash
claude-rs "Hello" --output-format json
```

- Streaming output (NDJSON of raw SSE events):

```bash
claude-rs "Hello" --output-format stream-json
```

- Enable tool execution (dangerous tools are disabled by default):

```bash
claude-rs "Create hello.py and run it" --permission-mode acceptEdits
```

## Auth

`claude-rs` supports three auth sources (in priority order):

1. `ANTHROPIC_AUTH_TOKEN` (sent as `Authorization: Bearer ...`)
2. `CLAUDE_CODE_OAUTH_TOKEN` or stored OAuth tokens (`claude-rs auth login`)
3. `ANTHROPIC_API_KEY` (sent as `x-api-key: ...`)

OAuth is a manual PKCE flow:

```bash
claude-rs auth login
```

## API Base URL / Model Overrides

- Override API base URL:

```bash
export ANTHROPIC_BASE_URL="http://localhost:8080"
```

The server must support `POST /v1/messages`.

- Override model:

```bash
export ANTHROPIC_MODEL="claude-sonnet-4-6"
```

## Files Written By The CLI

The config home is:

- `$CLAUDE_CONFIG_DIR` if set
- otherwise `~/.claude`

The CLI may write:

- Global config (credentials): `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`
- Session history (JSONL): `$CLAUDE_CONFIG_DIR/projects/<project>/...`
- Tool result spill files: `<cwd>/.claude-rs/tool-results/` (fallbacks to temp dir)

## Known Differences / Gaps

- No interactive UI/TUI.
- `WebFetch` returns fetched content; it does not apply the `prompt` (best-effort compatibility).
- Background tasks are not implemented (Task tools are session bookkeeping only).

