# comon

Single-binary, cross-platform TUI for:

- Local Codex usage stats (last 7/30 days, chart, top models) by scanning `CODEX_HOME/sessions`.
- Live account limits/credits by spawning `codex app-server` and calling `account/rateLimits/read`.

## Requirements

- Rust toolchain (stable) installed.
- Codex CLI installed and available as `codex` on your `PATH` (required for live limits/credits).
  - Usage stats still work without Codex CLI (they only need the session logs on disk).

## Build

From the repository root:

```bash
cargo build --release
```

The binary will be at:

- Windows: `target\\release\\comon.exe`
- Linux/macOS: `target/release/comon`

## Run

Run from a Git repository directory (Codex app-server may require a git repo):

```bash
./target/release/comon
```

Common flags:

- `--codex-home <path>`: override CODEX_HOME (default: `$CODEX_HOME` or `~/.codex`)
- `--codex-bin <path>`: override Codex CLI binary (default: `codex`)
- `--cwd <path>`: directory to launch `codex app-server` in (default: current directory)
- `--usage-days <n>`: days to scan for usage (default: 30; clamped 1..=90)
- `--refresh-usage-secs <n>`: usage refresh interval (default: 300)
- `--refresh-limits-secs <n>`: limits refresh interval (default: 60)

Example:

```bash
./target/release/comon --codex-home "C:\\Users\\You\\.codex" --cwd "C:\\Repos\\some-git-repo"
```

## Key bindings

- `Tab` Toggle data (Tokens/Time/Runs)
- `w` Toggle timeframe (Week/Month)
- `f` Toggle layout (Horz/Vert)
- `r` / `F5` Refresh now (usage + limits)
- `?` Help overlay
- `q` Quit

## Notes

- Usage stats are derived from Codex session JSONL logs. If you have no session data yet, values will be empty.
- Limits/credits require `codex app-server` to start successfully (auth, environment, and a usable working directory).

