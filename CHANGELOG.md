# Changelog

All notable changes to this project are documented in this file.

## 0.3.0 - 2026-02-16

- Added incremental session parsing with persisted offsets and parser state in `comon.db`.
- Reduced restart regressions: unchanged files outside current scan plan now stay visible via cache.
- Added `--scan-time-budget-ms` for bounded per-refresh parse time (`0` disables budget).
- Added `--max-jsonl-line-kib` to cap parsed line size without hard-dropping large files.
- Added cache DB schema migration (`v1 -> v2`) for offset/parser-state fields.
- For historical backfill after copying older sessions, run once:
  - `comon --full-scan --scan-time-budget-ms 0`
