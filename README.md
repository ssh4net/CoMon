<img width="1649" height="862" alt="SOuJoN2ooX" src="https://github.com/user-attachments/assets/a6c27de1-d345-4dc7-a7b6-68c296a3e9f4" />

# comon

Single-binary, cross-platform TUI for:

- Local Codex usage stats (last 7/30 days, chart, top models) by scanning `CODEX_HOME/sessions`.
- Local session-history browser grouped by project path, with session titles and prompt previews.
- Live account limits/credits by spawning `codex app-server` and calling `account/rateLimits/read`.

See `CHANGELOG.md` for release history.

<img width="1647" height="861" alt="Dkh7CxLgx7" src="https://github.com/user-attachments/assets/edec765d-0924-493b-8c10-cb32bca867a9" />

<img width="2088" height="952" alt="WindowsTerminal_nrizljNtk5" src="https://github.com/user-attachments/assets/cf2ac2e7-3a94-48cd-9d3b-1a4a98016f45" />
<img width="2088" height="952" alt="WindowsTerminal_HjIAOZQKWE" src="https://github.com/user-attachments/assets/4e3eed0a-d3aa-4ae5-bc4e-c6e5400bc1dd" />


## Release 0.3.4

- Fixed inflated usage totals from forked/subagent session logs.
- Reparse stale forked-session cache rows with scan-cache schema v3.
- Merged the session-history browser into the main `comon` TUI.
- Added runtime screen switching between Usage and Session history with `s` / `F2`.
- Added `--read` to start on the Session history screen.
- Added `--sessions-dir` and `--print-sessions-dir` for browsing alternate Codex session trees.
- Added incremental session parsing with persisted offsets and parser state in `comon.db`.
- Reduced restart regressions: unchanged files outside current scan plan now stay visible via cache.
- Fixed full backfill behavior: `--full-scan --scan-time-budget-ms 0` now forces a full reparse and cache refresh.
- Changed default workspace scope: `comon` without `--project` now always shows **All workspaces**.
- Fixed workspace startup behavior: no stale last workspace filter is restored when no explicit project is provided.
- Added `--scan-time-budget-ms` for bounded per-refresh parse time (`0` disables budget).
- Added `--max-jsonl-line-kib` to cap parsed line size without hard-dropping large files.
- Added cache DB schema migration (`v1 -> v2`) for offset/parser-state fields.
- For historical backfill after copying older sessions, run once: `comon --full-scan --scan-time-budget-ms 0`

## Requirements

- Rust toolchain (stable) installed.
- C/C++ compiler toolchain available (needed to build bundled SQLite through `rusqlite`).
- Codex CLI installed and available as `codex` on your `PATH` (required for live limits/credits).
  - Usage stats still work without Codex CLI (they only need the session logs on disk).
- For portable Linux builds (`--musl`), `rustup` and musl target toolchain support are required.

## Run

By default, `comon` shows usage for **All workspaces** (regardless of current directory).

Press `s` / `F2` at runtime to switch between the Usage and Session history screens.

Use `--project <path>` (or `--workspace <path>`) to filter usage stats to a specific git repo.

If `--project` points to a non-git directory, `comon` falls back to **All workspaces**.

`--cwd` controls where `codex app-server` is launched and does not change usage scope.

```bash
# If installed (recommended):
comon

# Start directly on the Session history screen:
comon --read

# Or run from the repo without installing:
cargo run --release
```

Common flags:

- `--codex-home <path>`: override CODEX_HOME (default: `$CODEX_HOME` or `~/.codex`)
- `--comon-home <path>`: override COMON_HOME for comon state/cache files (default: `$COMON_HOME` or `~/.comon`)
- `--print-config-path`: print effective comon config path and exit
- `-r` / `--read`: start on the Session history screen
- `--sessions-dir <path>`: override the Codex sessions directory used by the Session history screen
- `--print-sessions-dir`: print effective sessions directory and exit
- `--codex-bin <path>`: override Codex CLI binary (default: `codex`)
- `--cwd <path>`: directory to launch `codex app-server` in (default: current directory; does not change usage scope)
- `--project <path>` / `--workspace <path>`: filter usage stats to a specific project/workspace (also becomes default `--cwd` if `--cwd` not set)
- `--usage-days <n>`: days to scan for usage (clamped 1..=90; default from config)
- `--refresh-usage-secs <n>`: usage refresh interval in seconds (default from config)
- `--refresh-limits-secs <n>`: limits refresh interval in seconds (default from config)
- `--max-session-file-mib <n>`: per-file planning weight (MiB) for scan budget (default from config)
- `--max-session-total-mib <n>`: max total size (MiB) to scan across session files (default from config)
- `--max-session-files <n>`: max number of session files to scan per refresh (default from config)
- `--max-jsonl-line-kib <n>`: max parsed JSONL line size in KiB (default from config)
- `--scan-time-budget-ms <n>`: max parse time budget per refresh in ms (`0` disables budget)
- `--full-scan`: scan all files under `CODEX_HOME/sessions`, including old months (ignores mtime cutoff and file/byte planning caps)
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
  "max_jsonl_line_kib": 512,
  "scan_time_budget_ms": 1500,
  "full_scan": false,
  "scan_cache_max_entries": 50000
}
```

Example:

```bash
comon --codex-home "C:\\Users\\You\\.codex" --cwd "C:\\Repos\\some-git-repo"
```

Large-log recovery/tuning example:

```bash
# One-time backfill for copied/old sessions (full reparse + cache refresh):
comon --full-scan --scan-time-budget-ms 0

# Normal usage with bounded incremental refresh:
comon --scan-time-budget-ms 1500 --max-jsonl-line-kib 512
```

## Key bindings

- `Tab` Toggle data (Tokens/Time/Runs)
- `w` Toggle timeframe (Week/Month)
- `f` Toggle layout (Horz/Vert)
- `s` / `F2` Switch between Usage and Session history
- `r` / `F5` Refresh current screen
- `?` Help overlay
- `Esc` / `q` Quit
- `Enter` / `y` Continue past "no sessions found" warning (when shown)
- Session history: `Up` / `Down` / mouse wheel navigate, `Enter` / `Right` open project sessions, `Backspace` / `Left` / `Esc` go back

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
# Native install (default):
bash scripts/install-user.sh

# Portable Linux build/install (musl target, auto-detected arch):
bash scripts/install-user.sh --musl

# Explicit target example:
bash scripts/install-user.sh --musl --target x86_64-unknown-linux-musl
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
- Supports optional `--target <triple>` and `--musl` build/install mode on Linux.
- Adds missing Rust target via `rustup target add` when a target is requested.
- Prepares `COMON_HOME` (default `~/.comon`, or `$COMON_HOME` if set).
- Refuses to use symlink/reparse-point `COMON_HOME` paths.

### 5) Build a prebuilt zip package (for GitHub releases)

Portable Linux release order (Debian/Ubuntu example):

```bash
# 1) Build prerequisites
sudo apt update
sudo apt install -y build-essential musl-tools zip

# 2) Rust target for portable Linux builds
rustup toolchain install stable
rustup target add x86_64-unknown-linux-musl

# 3) Build + package from repo root
bash scripts/package-prebuilt.sh --musl

# 4) Upload generated zip to GitHub Release
ls dist/comon-v*-unknown-linux-musl.zip
```

Additional maintainer options:

```bash
# Linux: defaults to portable musl package
# Other OSes: defaults to host target package
bash scripts/package-prebuilt.sh

# Build portable Linux package (musl)
bash scripts/package-prebuilt.sh --musl

# Force host-target package (glibc on Linux)
bash scripts/package-prebuilt.sh --gnu
```

Package output:

- `dist/comon-v<version>-<target>.zip`

On Linux, prefer `*-unknown-linux-musl.zip` for maximum compatibility across distros.

Each zip includes:

- `comon` binary
- `install.sh` (user-scope install, no Cargo needed)
- `LICENSE`, `README.txt`

### 6) Install from prebuilt zip (no compile)

User flow:

```bash
unzip comon-v<version>-<target>.zip
cd comon-v<version>-<target>
bash install.sh
```

Optional custom install root:

```bash
bash install.sh ~/.local
```

## Development checks

ASCII-only guardrails for docs/code/scripts:

```bash
# Run full repository check (tracked files)
bash scripts/check-ascii.sh

# Install local pre-commit hook (checks staged files on commit)
bash scripts/install-pre-commit-hook.sh
```

CI also runs this check on each push and pull request via `.github/workflows/ascii-check.yml`.

## Notes

- Usage stats are derived from Codex session JSONL logs. If you have no session data yet, values will be empty.
- Limits/credits require `codex app-server` to start successfully (auth, environment, and a usable working directory).
- comon stores local app state in `~/.comon/state.json` by default (or `$COMON_HOME`, or `--comon-home`).
- comon stores scan cache in `~/.comon/comon.db` to avoid rereading unchanged session files.
- Large session logs are parsed incrementally with persisted parser offsets in `comon.db`; unchanged files are reused from cache.
- If historical days look incomplete after adding old session files, run once with `--full-scan --scan-time-budget-ms 0` to force a full reparse and refresh cached summaries.
- comon uses embedded SQLite (`rusqlite` with bundled SQLite); no system `sqlite3` CLI is required at runtime.
- comon stores user-editable runtime settings in `~/.comon/config.json` by default.
- Privacy: comon stores metadata (workspace paths, timestamps, token/run/time aggregates) and does not persist prompt/completion text.
- File permissions: on Unix-like systems, comon enforces `0700` on `COMON_HOME` and `0600` on files it writes (`config.json`, `state.json`, `comon.db`).
- Symlink hardening: comon refuses symlink targets for `COMON_HOME` files (`config.json`, `state.json`, `comon.db`, `comon.db-wal`, `comon.db-shm`) and rejects symlink/reparse-point `COMON_HOME` during install scripts.
