# claude-cli-rs (Weeks 1–8)

This folder contains the Rust rewrite workspace for the **headless/CLI-only** mode.

For a TypeScript CLI -> Rust CLI mapping, see `MIGRATION.md`.

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
cargo run -p claude-cli -- "Hello"

# Or via stdin:
echo "Hello" | cargo run -p claude-cli --
```

## Run A Prompt (Auth Token)

Uses `Authorization: Bearer ...`.

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"

export ANTHROPIC_AUTH_TOKEN="..."
cargo run -p claude-cli -- "Hello"
```

## Tool Execution (Week 4)

By default, "dangerous" tools like `Bash`, `Write`, and `Edit` are disabled. Enable them with a permission mode:

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"

export ANTHROPIC_API_KEY="..."
cargo run -p claude-cli -- "Create hello.py and run it" --permission-mode acceptEdits
```

## Override API Base URL

```bash
export ANTHROPIC_BASE_URL="http://localhost:8080"
```

## Override Model

```bash
export ANTHROPIC_MODEL="claude-sonnet-4-6"
```

## OAuth Login (Manual PKCE)

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"

cargo run -p claude-cli -- auth login
# Open the printed URL, then paste the redirect URL when prompted.

cargo run -p claude-cli -- "Hello"
```

## OAuth Logout

```bash
cd claude-cli-rs
. "$HOME/.cargo/env"

cargo run -p claude-cli -- auth logout
```

## Session Resume

By default, each run writes a session transcript (JSONL). Continue the most recent
session for the current project:

```bash
cargo run -p claude-cli -- "first prompt" --continue
cargo run -p claude-cli -- "follow up" --continue
```

Or resume by explicit session ID:

```bash
cargo run -p claude-cli -- --resume <session-id> "continue"
```

## Auto Memory Extraction (Stop Hook)

Opt-in via an env var:

```bash
export CLAUDE_RS_EXTRACT_MEMORIES=1
```

On each successful headless run, the CLI makes a small follow-up API call to extract durable
memories and appends them to an auto-memory log file:

- `~/.claude/projects/<project>/memory/logs/YYYY/MM/YYYY-MM-DD.md` (UTC)

Disable by unsetting the env var or using `--bare`.

## MCP Config Helpers

These modify `settings.json` files (user/project/local) by editing `mcpServers`.

```bash
# Add a stdio MCP server (note the -- before the server command args):
cargo run -p claude-cli -- mcp add --scope local --transport stdio github npx -- -y @modelcontextprotocol/server-github

# List servers:
cargo run -p claude-cli -- mcp list --scope local

# Remove a server:
cargo run -p claude-cli -- mcp remove --scope local github
```

## Notes

- Global config path (current stub): `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`
- Week 2 implements basic **API key auth + OAuth token flow + streaming API client**.
- Week 3 adds a minimal **query engine** (system prompt + git/CLAUDE.md context injection, continuation on `max_tokens`, and cost tracking printed to stderr).
- Week 4 adds a minimal **tool framework** + built-in `Bash/Read/Write/Edit/Glob/Grep`.
- Week 6 adds **session persistence/resume** and **context compaction**.
- Week 7 adds **stdin piping**, friendlier **error hints**, and cross-platform CI + integration tests.
- Week 8 adds expanded **tool tests**, additional **integration tests** (tool-use + `--continue`), and a migration guide.
