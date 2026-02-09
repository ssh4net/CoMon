<img width="1649" height="862" alt="SOuJoN2ooX" src="https://github.com/user-attachments/assets/a6c27de1-d345-4dc7-a7b6-68c296a3e9f4" />

# comon

Single-binary, cross-platform TUI for:

- Local Codex usage stats (last 7/30 days, chart, top models) by scanning `CODEX_HOME/sessions`.
- Live account limits/credits by spawning `codex app-server` and calling `account/rateLimits/read`.

<img width="1647" height="861" alt="Dkh7CxLgx7" src="https://github.com/user-attachments/assets/edec765d-0924-493b-8c10-cb32bca867a9" />

## Requirements

- Rust toolchain (stable) installed.
- C/C++ compiler toolchain available (needed to build bundled SQLite through `rusqlite`).
- Codex CLI installed and available as `codex` on your `PATH` (required for live limits/credits).
  - Usage stats still work without Codex CLI (they only need the session logs on disk).

## Run

When started inside a git repository, `comon` auto-detects the repo root and:

- Filters usage stats to that project
- Uses it as the default working directory for `codex app-server`

If started outside a git repo, usage is shown as **All workspaces**.

If you start outside a git repo but pass `--cwd` (or `--project`) pointing inside a git repo,
`comon` will auto-detect the git root from that path.

```bash
# If installed (recommended):
comon

# Or run from the repo without installing:
cargo run --release
```

Common flags:

- `--codex-home <path>`: override CODEX_HOME (default: `$CODEX_HOME` or `~/.codex`)
- `--comon-home <path>`: override COMON_HOME for comon state/cache files (default: `$COMON_HOME` or `~/.comon`)
- `--print-config-path`: print effective comon config path and exit
- `--codex-bin <path>`: override Codex CLI binary (default: `codex`)
- `--cwd <path>`: directory to launch `codex app-server` in (default: current directory)
- `--project <path>` / `--workspace <path>`: filter usage stats to a specific project/workspace (also becomes default `--cwd` if `--cwd` not set)
- `--usage-days <n>`: days to scan for usage (clamped 1..=90; default from config)
- `--refresh-usage-secs <n>`: usage refresh interval in seconds (default from config)
- `--refresh-limits-secs <n>`: limits refresh interval in seconds (default from config)
- `--max-session-file-mib <n>`: max size (MiB) of a single session file to scan (default from config)
- `--max-session-total-mib <n>`: max total size (MiB) to scan across session files (default from config)
- `--max-session-files <n>`: max number of session files to scan per refresh (default from config)
- `--full-scan`: scan all files under `CODEX_HOME/sessions`, including old months (ignores mtime cutoff)
- `--no-full-scan`: disable full scan for this run (overrides config)
- `--scan-cache-max-entries <n>`: max entries kept in cache database (`comon.db`) (default from config)
- `--rebuild-cache-on-start`: delete local scan cache DB files (`comon.db`, `comon.db-wal`, `comon.db-shm`) before first usage scan

Config precedence:

- CLI flags
- `~/.comon/config.json` (or `$COMON_HOME/config.json`, or `--comon-home <path>/config.json`)
- built-in defaults

`config.json` is auto-created on first run. Example:

```json
{
  "schema_version": 1,
  "usage_days": 30,
  "refresh_usage_secs": 300,
  "refresh_limits_secs": 60,
  "max_session_file_mib": 256,
  "max_session_total_mib": 256,
  "max_session_files": 10000,
  "full_scan": false,
  "scan_cache_max_entries": 50000
}
```

Example:

```bash
comon --codex-home "C:\\Users\\You\\.codex" --cwd "C:\\Repos\\some-git-repo"
```

## Key bindings

- `Tab` Toggle data (Tokens/Time/Runs)
- `w` Toggle timeframe (Week/Month)
- `f` Toggle layout (Horz/Vert)
- `r` / `F5` Refresh now (usage + limits)
- `?` Help overlay
- `Esc` / `q` Quit
- `Enter` / `y` Continue past "no sessions found" warning (when shown)

## Build from source

### 1) Setup Cargo (Rust)

If you don't have `cargo` yet, install Rust via the official `rustup` installer:

Linux/macOS:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Windows:

- Download and run the installer from https://rustup.rs

Verify:

```bash
cargo --version
```

### 2) Build the app

From the repository root:

```bash
cargo build --release
```

The binary will be at:

- Windows: `target\\release\\comon.exe`
- Linux/macOS: `target/release/comon`

### 3) Install the app (user scope)

To run `comon` from anywhere:

```bash
cargo install --path . --locked --force
```

This installs the binary into:

- Linux/macOS: `~/.cargo/bin`
- Windows: `%USERPROFILE%\\.cargo\\bin`

Make sure that directory is on your `PATH` (the Rust installer typically does this for you).

Optional: install into `~/.local` instead:

```bash
cargo install --path . --locked --force --root ~/.local
```

### 4) Quick install scripts (user scope)

Linux/macOS:

```bash
bash scripts/install-user.sh
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\install-user.ps1
```

Optional custom install root:

- Bash: `bash scripts/install-user.sh ~/.local`
- PowerShell: `.\scripts\install-user.ps1 -Root "$HOME\\.local"`

Install script behavior:

- Installs `comon` into the chosen user root.
- Prepares `COMON_HOME` (default `~/.comon`, or `$COMON_HOME` if set).
- Refuses to use symlink/reparse-point `COMON_HOME` paths.

## Notes

- Usage stats are derived from Codex session JSONL logs. If you have no session data yet, values will be empty.
- Limits/credits require `codex app-server` to start successfully (auth, environment, and a usable working directory).
- comon stores local app state in `~/.comon/state.json` by default (or `$COMON_HOME`, or `--comon-home`).
- comon stores scan cache in `~/.comon/comon.db` to avoid rereading unchanged session files.
- comon uses embedded SQLite (`rusqlite` with bundled SQLite); no system `sqlite3` CLI is required at runtime.
- comon stores user-editable runtime settings in `~/.comon/config.json` by default.
- Privacy: comon stores metadata (workspace paths, timestamps, token/run/time aggregates) and does not persist prompt/completion text.
- File permissions: on Unix-like systems, comon enforces `0700` on `COMON_HOME` and `0600` on files it writes (`config.json`, `state.json`, `comon.db`).
- Symlink hardening: comon refuses symlink targets for `COMON_HOME` files (`config.json`, `state.json`, `comon.db`, `comon.db-wal`, `comon.db-shm`) and rejects symlink/reparse-point `COMON_HOME` during install scripts.
