# claude-cli-rs (Week 1 Scaffold)

This folder contains the Rust rewrite workspace for the **headless/CLI-only** mode.

## Quick Start

```bash
cd claude-cli-rs
cargo test
cargo run -p claude-cli -- --help
```

## Notes

- Global config path (current stub): `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`
- `claude-rs` currently implements **flag parsing + config/settings plumbing only** (Week 1).

