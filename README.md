<img width="1298" height="1050" alt="WindowsTerminal_dhfDXW9bI6" src="https://github.com/user-attachments/assets/15d36486-3406-4680-90c1-18d1d1d918db" />
# comon

Single-binary, cross-platform TUI for:

- Local Codex usage stats (last 7/30 days, chart, top models) by scanning `CODEX_HOME/sessions`.
- Local session-history browser grouped by project path, with session titles and prompt previews.
- Live account limits/credits by spawning Codex App Server and calling `account/rateLimits/read` when an App Server executable is available.

See `CHANGELOG.md` for release history.

<img width="1298" height="1050" alt="WindowsTerminal_HTSsSPVKmE" src="https://github.com/user-attachments/assets/96719893-3cfc-4d3d-8d44-06721df8e14c" />
<img width="1298" height="1050" alt="WindowsTerminal_qoqii76JKv" src="https://github.com/user-attachments/assets/15828746-2591-4871-a84c-f1d70e83ac5a" />
<img width="1298" height="1050" alt="WindowsTerminal_qoqii76JKv" src="https://github.com/user-attachments/assets/748b0ba4-4c1a-4383-8f02-cb1abddacebb" />
<img width="1298" height="1050" alt="WindowsTerminal_yiKe3HUtMO" src="https://github.com/user-attachments/assets/7a1c4ff5-3401-4f97-92f5-acacb3eb7b5b" />


## Requirements

- Rust toolchain (stable) installed.
- C/C++ compiler toolchain available (needed to build bundled SQLite through `rusqlite`).
- Codex CLI installed as `codex` on your `PATH`, or a discoverable/explicit Codex App Server executable (required only for live limits/credits).
  - Usage stats still work without Codex CLI (they only need the session logs on disk).
- For portable Linux builds (`--musl`), install both the Rust musl target and a musl C compiler.
  - Debian/Ubuntu: `sudo apt install musl-tools`
  - Required tool for x86_64 musl builds: `x86_64-linux-musl-gcc`

## Run

By default, `comon` shows usage for **All workspaces** (regardless of current directory).

Press `s` / `F2` at runtime to switch between the Usage and Session history screens.

Use `--project <path>` (or `--workspace <path>`) to filter usage stats to sessions whose
working directory equals or is under that path (Codex session `cwd`).

`--cwd` controls where Codex App Server is launched and does not change usage scope.

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
- `--codex-bin <path>`: override Codex CLI binary (spawned as `<path> app-server`)
- `--app-server-bin <path>`: override a standalone Codex App Server executable (spawned directly)
- `--live-limits <auto|on|off>`: `auto` tries App Server if found, `on` requires it, `off` disables live limits (default: `auto`)
- `--cwd <path>`: directory to launch Codex App Server in (default: current directory; does not change usage scope)
- `--project <path>` / `--workspace <path>`: filter usage stats by session working directory path (also becomes default `--cwd` if `--cwd` not set)
- `--usage-days <n>`: summary/model-share window (clamped 1..=90; default from config); charts index the complete local history
- `--refresh-usage-secs <n>`: usage refresh interval in seconds (default from config)
- `--refresh-limits-secs <n>`: limits refresh interval in seconds (default from config)
- `--max-session-file-mib <n>`: per-file planning weight (MiB) for scan budget (default from config)
- `--max-session-total-mib <n>`: max total size (MiB) to scan across session files (default from config)
- `--max-session-files <n>`: max number of session files to scan per refresh (default from config)
- `--max-jsonl-line-kib <n>`: max parsed JSONL line size in KiB (default from config)
- `--scan-time-budget-ms <n>`: max parse time budget per refresh in ms (`0` disables budget)
- `--full-scan`: process the complete pending session backlog in one refresh (ignores file/byte planning caps)
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

Codex App-only Windows installs:

```bash
# Usage/session history only; avoids App Server probing.
comon --live-limits off

# If auto-detection misses the bundled CLI-style binary:
comon --codex-bin "C:\\Path\\To\\Codex\\codex.exe"

# If the app ships a standalone App Server binary:
comon --app-server-bin "C:\\Path\\To\\Codex\\app-server.exe"
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
- `g` / `w` Toggle grouping (Day/Week/Month)
- `f` Toggle layout (Horz/Vert)
- `z` / `F6` Toggle Usage zone (Local/UTC); APISTAT always uses server UTC
- `n` Cycle display formatting (Classic/System Compact/System Full)
- Mouse wheel or arrow keys Scroll chart history (`PgUp`/`PgDn`, `Home`/`End` also work)
- Mouse: click the top tabs, Usage/Activity controls (including the Usage style selector), or the bottom-right Quit action
- Mouse: hover a filled vertical chart bar to see its exact date and full locale-aware value
- `s` / `F2` Switch between Usage and Session history
- `r` / `F5` Refresh current screen
- `?` Help overlay
- `q` Quit (with confirmation)
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
# Requires musl-tools on Debian/Ubuntu.
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

If a musl build fails with `failed to find tool "x86_64-linux-musl-gcc"`,
install `musl-tools` and retry. Adding the Rust target with `rustup target add`
is necessary but not sufficient because bundled SQLite is compiled through a C
compiler.

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

macOS signed release package:

```bash
# Local verification package with ad-hoc signing and no notarization:
SKIP_NOTARY=1 bash scripts/package-macos.sh

# Developer ID signed package, submitted with a stored notarytool profile:
SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
NOTARY_PROFILE="comon-notary" \
bash scripts/package-macos.sh

# Optional explicit target:
bash scripts/package-macos.sh --target aarch64-apple-darwin
```

The macOS script builds `comon`, signs the executable, bundles Homebrew-linked
dylibs into the package when needed, and creates:

- `dist/comon-v<version>-<apple-target>.zip`

Signing identity and notarization profile values are read from environment
variables only; do not commit credentials or Apple account details into the repo.

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
- Usage charts index the complete local session history and cache completed work incrementally. Until the initial backlog is complete, CoMon shows an indexing status instead of partial totals.
- APISTAT displays the server-owned UTC buckets returned by Codex App Server. USAGE reconstructs local estimates from session logs, so small differences can remain even when the date range and UTC grouping match.
- Limits/credits require Codex App Server to start successfully (auth, environment, and a usable working directory). CoMon auto-detects `codex` on `PATH` and common Windows Codex App bundle locations; use `--codex-bin` or `--app-server-bin` when needed.
- comon stores local app state in `~/.comon/state.json` by default (or `$COMON_HOME`, or `--comon-home`).
- Display formatting starts in Classic mode; press `n` or use `STYLE CLASS/SCOMP/SFULL` in the Usage controls to choose Classic, System Compact, or System Full. Both System modes use the operating system locale for dates, times, decimals, and calendar labels; Compact uses the detected thousands separator and abbreviates dashboard token values, while Full groups expanded integers with regular spaces. The choice is saved in `state.json` without changing stored data.
- Vertical chart labels preserve the selected style when they fit and compact only individual values that exceed their bar width. Hovering a filled bar shows the exact value.
- The quit dialog's `Don't show again` checkbox disables future `q`/`QUIT` confirmations after a confirmed exit. The checkbox beside `QUIT` shows that saved state; clicking it asks before enabling or disabling confirmation.
- comon stores scan cache in `~/.comon/comon.db` to avoid rereading unchanged session files.
- Large session logs are parsed incrementally with persisted parser offsets in `comon.db`; unchanged files are reused from cache.
- If historical days look incomplete after adding old session files, run once with `--full-scan --scan-time-budget-ms 0` to force a full reparse and refresh cached summaries.
- comon uses embedded SQLite (`rusqlite` with bundled SQLite); no system `sqlite3` CLI is required at runtime.
- comon stores user-editable runtime settings in `~/.comon/config.json` by default.
- Privacy: comon stores metadata (workspace paths, timestamps, token/run/time aggregates) and does not persist prompt/completion text.
- File permissions: on Unix-like systems, comon enforces `0700` on `COMON_HOME` and `0600` on files it writes (`config.json`, `state.json`, `comon.db`).
- Symlink hardening: comon refuses symlink targets for `COMON_HOME` files (`config.json`, `state.json`, `comon.db`, `comon.db-wal`, `comon.db-shm`) and rejects symlink/reparse-point `COMON_HOME` during install scripts.
