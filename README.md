# agent-history
Project-wide full-text search TUI for Codex / Claude conversation logs.

- Codex: `~/.codex/sessions/**.jsonl` and `~/.codex/archived_sessions/**.jsonl`
- Claude: `~/.claude/projects/**.jsonl`
- Auto-discovery: project-local `**/.codex/{sessions,archived_sessions}` and `**/.codex/history.jsonl` under `$HOME` (skips `.git`, `node_modules`, etc.)

Japanese README: `README.ja.md`

## Install
```bash
cargo install --path .
```

## Requirements
- Rust toolchain (to build/install from source)
- Optional: `codex` CLI (for `Enter` resume on OpenAI/Codex sessions)
- Optional: `claude` CLI (for `Enter` resume on Claude sessions)
- Optional: `$PAGER` (defaults to `less -R`) for `Ctrl+o`

No external fuzzy finder (e.g. `fzf`) is required.

## Usage
```bash
agent-history
```

You can start typing the query immediately; indexing runs in the background and shows progress.

For development:
```bash
cargo run --release
```

## List Format
Each result row is:

`timestamp(last activity)  dir  C|O  first message (1st line)`

- `C`: Claude
- `O`: OpenAI/Codex

Notes:
- Codex logs may start with `AGENTS.md` / `<environment_context>` metadata; those are excluded from the “first message”.
- Sessions that contain only such metadata are not shown.
- `subagents/` logs are excluded by default because they are noisy and can be huge.

## Options
```bash
# Add extra search roots (repeatable)
agent-history --root /path/to/dir

# Disable default roots and search only explicit roots
agent-history --no-default-roots --root /path/to/dir

# Also include ~/.codex/history.jsonl (simple text format)
agent-history --history

# Start with a query
agent-history --query "DMARC"

# Space-separated tokens are AND
agent-history --query "cloud run"

# Smartcase: tokens with ASCII uppercase are case-sensitive
agent-history --query "GitHub Actions"

# JSON output (non-interactive, for scripting / AI agent use)
agent-history --json --max-results 10
agent-history --query "webpack" --json
```

## Headless / JSON Mode

`--json` skips the TUI and prints a JSON array to stdout. This is useful for
scripting, piping into `jq`, or letting AI coding agents search conversation
history programmatically.

```bash
agent-history --query "deploy" --json --max-results 5
```

Each entry in the array contains:

| Field | Description |
|-------|-------------|
| `session_id` | Unique session identifier |
| `source` | `claude`, `codex`, or `codex_history` |
| `last_activity` | Timestamp of most recent activity |
| `cwd` | Full working directory path |
| `dir` | Directory basename |
| `first_message` | First user message in the session |
| `resume_cmd` | Ready-to-run shell command to resume the session |

Example output:
```json
[
  {
    "session_id": "dc033d01-a276-4c10-bfaf-2b5d4dba7429",
    "source": "claude",
    "last_activity": "2026-02-17T18:20:16.446Z",
    "cwd": "/home/user/my-project",
    "dir": "my-project",
    "first_message": "Help me fix the webpack config",
    "resume_cmd": "claude --resume dc033d01-a276-4c10-bfaf-2b5d4dba7429"
  }
]
```

## Keys
- Typing: update query (search as you type)
- `Backspace`: delete one char
- `Ctrl+u`: clear query
- `↑/↓`: move selection
- `PageUp/PageDown`: move by page
- `Enter`: resume in `codex resume` / `claude --resume` (requires those CLIs)
- `Ctrl+o`: open raw JSONL around the selected item in `$PAGER` (defaults to `less -R`)
- `Esc` / `Ctrl+c`: quit

## Privacy
This tool reads local log files and does not send your data over the network.

## Security
See `SECURITY.md`.

## License
Licensed under either of:

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT license (`LICENSE-MIT`)
