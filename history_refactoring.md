# History / project identity refactoring

Status: **implemented on `history`** (cache schema **v14**)
Date: 2026-07-29  
Sources: comon local scan/cache; Codex tree at `/mnt/e/GH/codex`; local audit of `~/.codex/sessions`

## Goal

Replace comon's filesystem `.git` heuristics for "what is a project?" with **Codex-native session identity**.

**Decision: completely remove `.git` as a comon signal/option** for:

- project discovery
- project grouping keys
- display path collapse
- CLI `--project` / workspace filter resolution
- cache identity

The only git-related data comon may keep is **whatever Codex already wrote into session logs or its own state DB** (optional display metadata). Comon must not walk the filesystem for `.git` to invent projects.

---

## Task tracker

### Implemented in the v13 worktree baseline

- [x] Document Codex storage model (thread `cwd`, no project table, sticky session cwd).
- [x] Document sandbox / external approved paths as same project (permissions, not new projects).
- [x] Remove filesystem `.git` as project identity signal in comon.
- [x] Replace `project_identity_from_path` with `session_cwd_identity` (normalize cwd only).
- [x] History catalog groups by normalized session cwd (`src/read/scan.rs`).
- [x] Usage identity no longer walks the filesystem for `.git`.
- [x] CLI `--project` / `--workspace` is a path filter (equals or under), not git-root resolution.
- [x] Non-git directories are valid filter targets (no silent fallback to All workspaces solely for missing git).
- [x] README updated: project = session working directory path filter.
- [x] Cache schema **v13**: rebuild `file_cache` so old git-collapsed keys are dropped.
- [x] Tests updated: no git markers for identity; nested cwd not collapsed to parent `.git`.
- [x] Full unit suite green after refactor (122 tests at time of change).
- [x] Confirm Activity **per-project heatmaps** still built (project key = session cwd; not disabled).
- [x] Document comon scan-cache behavior (`comon.db` incremental parse vs full recompute).
- [x] Audit history 128-line budget against real sessions; script at `internal/audit_history_128.py`.
- [x] Interpret audit: late `thread_settings_applied` cwd after 128 is usually **same** as first cwd (no identity loss on sample corpus).

### Completed v14 work

- [x] Define and persist one immutable `owner_cwd` for each session.
- [x] Use the same owner resolver in both History and Usage.
- [x] Treat `thread_settings_applied.cwd` as mutable resume metadata, never as ownership.
- [x] Recover a missing/reordered header without accepting late operational paths.
- [x] Compare project filters using the same cross-platform identity normalization.
- [x] Rebuild all derived cache rows from schemas v12 and v13.
- [x] Add corrupted-session, permission-path, cache-migration, and audit regressions.

### Deferred / not in this refactor

- [ ] Optional **per-path mode** (heatmap of touched paths under a session; no new projects).
- [ ] Optional soft-merge modes (prefix rollup, remote-URL rollup from Codex-recorded fields only).
- [ ] Optional **Codex `state_5.sqlite`-backed history index** for faster thread lists (see below).

---

## Background: why the old model existed

Original idea:

1. Run comon **inside** a repo.
2. Detect git root.
3. Show project-specific stats for that root.
4. "No project" / all-workspaces mode was expected **not** to need git.

What had shipped:

- `.git` walk in **two** places:
  1. **CLI filter**: `--project` via `detect_git_root`.
  2. **Always-on grouping**: `project_identity_from_path` collapsed session paths to a git root whenever `.git` existed on disk under the recorded cwd.

With full Session history, git-root collapse was the wrong primary abstraction.

---

## How Codex actually stores "projects"

### Codex has no Project entity

There is no `projects` table and no stable project id independent of path.

Primary local state DB: `CODEX_HOME/state_5.sqlite` (`STATE_DB_FILENAME` in Codex state crate).

`threads` (simplified):

| Field | Role |
|---|---|
| `id` | Thread / session id |
| `rollout_path` | JSONL session log path |
| `cwd` | **Primary workspace path for this thread** |
| `sandbox_policy` | Permission policy (string) |
| `git_sha` / `git_branch` / `git_origin_url` | Optional git **metadata** |
| title, preview, tokens, archived, ... | UI / history |

Listing filters use `cwd_filters: Option<Vec<PathBuf>>` (normalized path filters), not project ids.

### Session JSONL is the durable source of truth

1. **`session_meta`** - `meta.cwd`, id, optional `git` blob
2. **`turn_context`** - turn cwd, workspace_roots, sandbox / permission profile
3. **`thread_settings_applied`** - can update cwd/settings
4. Tool / exec / patch items - paths touched; **not** a project registry

### Codex has both an origin cwd and mutable live metadata

Codex state extraction (`codex-rs/state/src/extract.rs`):

1. Set thread `cwd` from matching `session_meta.cwd`.
2. **`turn_context.cwd` does not override** once session cwd is set.
3. `turn_context` only fills cwd if session cwd was empty.
4. `thread_settings_applied` can later update Codex's live/resume metadata,
   including the state-DB `threads.cwd`, permission profile, and model.

Test: `turn_context_does_not_override_session_cwd`.

Codex needs that fourth rule to resume a thread using its latest settings. It is
not an accounting rule. `comon` must preserve a distinct immutable owner so a
later resume setting, sandbox expansion, or external tool invocation cannot
move already-consumed or future tokens to a different project.

**Implication:** project identity for a comon session is the **initial
session-level owner cwd**, not the latest Codex resume cwd and not every path
edited during the session.

### Sandbox and external approved paths are permissions, not projects

| Path type | Codex meaning | Project for comon? |
|---|---|---|
| Session / thread `cwd` | Thread home | **Yes - primary key** |
| `workspace_roots` / writable roots | Allowed areas for tools | No - same project |
| User-approved path outside sandbox | Extra permission on same thread | No - same project |
| Tool/process working directory | Execution detail | Do not invent project |

**Product rule:**

> External paths approved by the user, even if edited outside the original sandbox root, still belong to the **original session project (`cwd`)**, not a new project.

### Where Codex itself uses git

Codex may collect git info into session meta / state DB and protect `.git` under sandboxes. That is **Codex under the hood**. Comon may **read** recorded fields only. Comon must **not** walk disk for `.git`.

---

## Target model (v14)

### Primary: session-cwd mode

```text
project_key   = normalize(owner_cwd)
display_path  = owner_cwd (normalized absolute form)
session_usage = all tokens / time / runs for that session
project_usage = sum of sessions with the same project_key
```

`owner_cwd` sources, in precedence order:

1. the first valid matching `session_meta.payload.cwd`
2. the first usable `turn_context.payload.cwd`, **only if no usable matching
   session meta exists anywhere in the session file**

Never from: `thread_settings_applied`, apply-patch paths, shell workdir alone,
sandbox/workspace roots, permission grants, or filesystem `.git`.

The owner resolver records whether `session_meta` or the turn-context fallback
won. If neither source is usable, History lists the session under **Unresolved
session owner** and Usage retains unscoped totals without inventing a project.

Helper: `session_cwd_identity` in `src/usage/mod.rs` (replaces `project_identity_from_path`).

### CLI filter (implemented)

| Behavior | Result |
|---|---|
| `--project PATH` | Sessions whose **session cwd equals or is under** that path |
| Non-git directory | **Valid** filter path |
| No `--project` | All workspaces |

### Per-project heatmaps (Activity)

**Not removed.** `project_activity` still builds one heatmap row per project key. After the refactor the key is session cwd, so rows may **split** former git-root groups. Tests still cover `compute_snapshot_builds_project_activity_*`.

---

## comon.db scan cache (usage) - how it works

This is **comon's** cache (`$COMON_HOME/comon.db`), not Codex `state_5.sqlite`.

| Situation | Behavior |
|---|---|
| Cached file unchanged (size + mtime + fully_parsed) | **Skip** reparse; reuse totals |
| Codex **appends** to an existing JSONL | Cache miss match -> plan file; **resume** from `file_offset`, parse new lines only when possible |
| File shrink / rewrite / schema migration | Full reparse from offset 0 |
| Refresh budgets | Only some dirty files advance per cycle |

So: DB exists so old sessions are not fully recomputed every refresh; appends are incremental when resume conditions hold. Schema **v14** one-time rebuilds after the immutable-owner change. A file which was previously unresolved is reparsed from offset zero if a later append finally supplies a resolvable owner.

**History catalog** does **not** use this same offset cache; it head-scans session files for list identity/title.

---

## History 128-line budget (`PROJECT_IDENTITY_LINE_LIMIT`)

### Two separate bounded responsibilities

The lightweight **History presentation probe** (`src/read/scan.rs` ->
`scan_session_summary`) reads at most **128** JSONL lines for title, timestamp,
and optional display metadata. It never changes the owner selected below.

The shared **owner resolver** first inspects the first 128 lines for a valid
matching `session_meta.cwd`. That is the normal, cheap path. If none exists,
it performs one full streaming recovery pass that considers only:

1. matching `session_meta.cwd` (preferred), or
2. the first valid `turn_context.cwd` after EOF proves meta is unavailable.

It never considers a `thread_settings_applied.cwd` or any operational/reference
path. Thus a late authoritative header can repair a damaged/reordered session,
but late settings cannot rehome a resolved session.

Usage calls the same owner resolver before parsing token deltas, so History,
Usage, and Activity all use one owner. The 128-line presentation limit is not
a token-accounting limit.

### What can be incomplete

| Risk | Severity on audited corpus |
|---|---|
| id/cwd only after line 128 | **0 / 234** missing in first 128 |
| Title only after line 128 | **0** in audit |
| `thread_settings_applied` cwd after 128 | **10 sessions / 809 events** observed |

### Audit tool

```bash
python3 internal/audit_history_128.py
python3 internal/audit_history_128.py /path/to/sessions
```

Sample run (`~/.codex/sessions`, 2026-07-29):

```text
files 234
settings cwd after 128 -> ... (9 files listed)
issues 9 of 234
```

No `MISSING id/cwd in 128`, no `title after 128`.

### Finding: late settings cwd is usually not a real move

Follow-up check on files with settings cwd after line 128:

| Outcome | Count |
|---|---|
| Settings `cwd` **identical** to first `session_meta.cwd` | **10** |
| Settings `cwd` **different** (real reassignment) | **0** |

Codex re-emits settings (including the same cwd) mid-session. History catalog already has the correct home from early `session_meta`.

**Conclusion:**

- The 128-line presentation probe deliberately ignores later paths for
  ownership.
- A missing/reordered header is recovered by a source-restricted full scan;
  no valid session is hidden merely because its header is late.
- The observed late settings are redundant resume events, not identity loss.
- The audit reports all settings occurrences and distinguishes a differing
  resume cwd from a missing owner; neither changes ownership.

---

## What was removed (code)

- [x] `detect_git_root` in `src/main.rs`
- [x] `project_identity_from_path` git walk in `src/usage/mod.rs`
- [x] History rewrite of cwd via git root
- [x] README "git repo" project language

### Kept

- [x] Display of Codex-recorded `git_branch` / commit / url in history UI when present
- [x] No disk probe if missing

### Cache

- [x] Schema v14 rebuild message: immutable owner keys only

---

## Optional future work (not done)

### Per-path mode

Analytics under a session (touched paths, external approved), still roll up to session cwd.

### Soft-merge modes

Prefix rollup or remote-URL rollup from **Codex-recorded** fields only - never disk `.git`.

### state_db-backed history index

Use Codex `state_5.sqlite` `threads` (`cwd`, title, `rollout_path`) for faster history listing; still open JSONL for detail/usage. Separate from `comon.db` incremental usage cache (which already avoids re-parsing unchanged files).

---

## Success criteria

| Criterion | Status |
|---|---|
| No disk `.git` walk for identity/filters | **Done** |
| Project list = normalized session cwds | **Done** |
| No-git sessions appear under absolute cwd | **Done** |
| External approved edits stay on original session project | **Done** |
| All workspaces does not git-collapse | **Done** |
| Git branch/remote display only if Codex stored it | **Done** |
| Heatmaps still per project (session cwd key) | **Done** |
| Late header recovery never accepts operational paths | **Done** |

---

## Summary

| Topic | Decision / result |
|---|---|
| Codex project model | Thread/session **`cwd`** |
| Sandbox / approved external paths | Same session/project |
| Filesystem `.git` in comon | **Removed** as signal |
| Codex-recorded git fields | Optional display only |
| Default grouping | Exact normalized session cwd |
| Usage cache | `comon.db` incremental; v14 rebuild after immutable-owner change |
| History 128-line limit | 128-line presentation probe plus source-restricted owner recovery |
| Per-path / state_db history | Still optional later |

This document is the record of the history/project-identity refactor and the 128-line audit findings.
