# Changelog

All notable changes to this project are documented in this file.

## Unreleased

- Added a persisted `n` shortcut to cycle display formatting through Classic, System Compact, and System Full for numbers, dates, times, and calendar labels.
- Clarified token pair charts with the `TOKENS (TOTAL / NON-CACHED)` heading.
- Added compact `K/M/B/T` notation for large dashboard token values in System Compact mode, including values inside vertical chart bars. System Full keeps those values expanded, while Classic output remains unchanged.
- Added mouse controls for selecting the visible statistic, chart timeframe, and chart orientation; clicking the footer cycles the display style on every screen.

## 0.3.6 - 2026-06-05

- Added `--live-limits auto|on|off` so app-only installs can run without a live-limits spawn error.
- Added `--app-server-bin` for standalone Codex App Server executables.
- Added Windows auto-detection for common Codex App bundled App Server locations.
- Project activity token headers now show total/out-of-cache token pairs.
- Documented the musl C compiler requirement for portable Linux builds.

## 0.3.4 - 2026-04-24

- Fixed inflated usage totals from forked/subagent session logs by ignoring replayed parent-session token history while preserving new post-fork usage.
- Bumped the scan-cache schema to reparse stale forked-session cache rows.

## 0.3.3 - 2026-03-18

- Added a built-in Session history screen to the main `comon` TUI:
  - Project list grouped by session `cwd`
  - Session list with derived titles from user prompts
  - Session detail view with prompt previews, token counts, tool-call counts, and git metadata when present
- Added runtime screen switching:
  - `s` / `F2` now toggles between Usage and Session history without restarting the app
  - `r` / `F5` refreshes the active screen and rebuilds the sessions catalog on the Session history screen
- Added session-browser CLI options:
  - `--read` starts directly on the Session history screen
  - `--sessions-dir` overrides the scanned Codex sessions tree
  - `--print-sessions-dir` prints the effective sessions directory and exits
- Hardened startup behavior for session browsing:
  - Missing default `CODEX_HOME/sessions` now opens an empty Session history view instead of failing startup

## 0.3.2 - 2026-02-17

- Fixed workspace startup scoping:
  - `comon` without `--project` now always uses **All workspaces**, even when launched inside a repo.
  - A non-git `--project` now disables repo filtering, even if launch dir is inside a repo.
  - `--cwd` now only controls app-server launch directory and never changes usage scope.
  - `comon` no longer restores a stale last workspace filter when no workspace hint is detected.
- Hardened long-history backfill behavior:
  - `--full-scan --scan-time-budget-ms 0` now forces full reparse instead of trusting unchanged cache rows.
  - Full scan now ignores planner file/byte caps.
- Added regression tests for:
  - workspace selection precedence
  - full-scan stale-cache repair
  - append-only file resume via cached file offsets

## 0.3.0 - 2026-02-16

- Added incremental session parsing with persisted offsets and parser state in `comon.db`.
- Reduced restart regressions: unchanged files outside current scan plan now stay visible via cache.
- Added `--scan-time-budget-ms` for bounded per-refresh parse time (`0` disables budget).
- Added `--max-jsonl-line-kib` to cap parsed line size without hard-dropping large files.
- Added cache DB schema migration (`v1 -> v2`) for offset/parser-state fields.
- For historical backfill after copying older sessions, run once:
  - `comon --full-scan --scan-time-budget-ms 0`
