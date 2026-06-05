use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc, Weekday};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant, SystemTime};

const MAX_ACTIVITY_GAP_MS: i64 = 2 * 60 * 1000;
const DEFAULT_MAX_SESSION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_FILES_SCANNED: usize = 10_000;
const DEFAULT_MAX_JSONL_LINE_BYTES: usize = 512 * 1024;
const DEFAULT_SCAN_TIME_BUDGET_MS: u64 = 1500;
const MAX_DISTINCT_MODELS: usize = 5_000;
const SCAN_CACHE_DB_SCHEMA_VERSION: i64 = 3;
const FORK_REPLAY_END_GAP_MS: i64 = 1_000;
const FORK_REPLAY_NO_TOKEN_GRACE_MS: i64 = 2_000;
pub const DEFAULT_SCAN_CACHE_MAX_ENTRIES: usize = 50_000;
pub const ACTIVITY_TIMELINE_WEEKS: usize = 54;
pub const ACTIVITY_TIMELINE_DAYS: usize = ACTIVITY_TIMELINE_WEEKS * 7;

#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    pub max_session_file_bytes: u64,
    pub max_session_total_bytes: u64,
    pub max_session_files_scanned: usize,
    pub max_jsonl_line_bytes: usize,
    pub scan_time_budget_ms: u64,
    pub full_scan: bool,
    pub scan_cache_max_entries: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_session_file_bytes: DEFAULT_MAX_SESSION_FILE_BYTES,
            max_session_total_bytes: DEFAULT_MAX_SESSION_TOTAL_BYTES,
            max_session_files_scanned: DEFAULT_MAX_SESSION_FILES_SCANNED,
            max_jsonl_line_bytes: DEFAULT_MAX_JSONL_LINE_BYTES,
            scan_time_budget_ms: DEFAULT_SCAN_TIME_BUDGET_MS,
            full_scan: false,
            scan_cache_max_entries: DEFAULT_SCAN_CACHE_MAX_ENTRIES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMetric {
    Tokens,
    Time,
    Runs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartRange {
    Week,
    Month,
}

pub fn format_compact_kmb(value: u64, max_width: u16) -> String {
    // Examples (depending on width):
    //  1234 -> 1.23K / 1.2K / 1K
    //  12_345_678 -> 12.35M / 12.3M / 12M
    //  999 -> 999
    if max_width == 0 {
        return String::new();
    }
    if value < 1000 {
        let s = value.to_string();
        return if s.len() <= max_width as usize {
            s
        } else {
            // Worst-case truncate from the left.
            s[s.len().saturating_sub(max_width as usize)..].to_string()
        };
    }

    let (div, suffix) = if value >= 1_000_000_000_000 {
        (1_000_000_000_000f64, "T")
    } else if value >= 1_000_000_000 {
        (1_000_000_000f64, "B")
    } else if value >= 1_000_000 {
        (1_000_000f64, "M")
    } else {
        (1_000f64, "K")
    };
    let scaled = (value as f64) / div;

    // In dense layouts, force integer suffixes (e.g. 27M instead of 27.4M).
    // Heuristic: if the label width is <= 5 cells, decimals tend to hurt readability.
    if max_width <= 5 {
        let s = format_compact_scaled(scaled, suffix, 0);
        return if s.len() <= max_width as usize {
            s
        } else {
            // truncate
            s[..max_width as usize].to_string()
        };
    }

    // Prefer 2 decimals, then reduce precision if it doesn't fit.
    for decimals in [2usize, 1usize, 0usize] {
        let s = format_compact_scaled(scaled, suffix, decimals);
        if s.len() <= max_width as usize {
            return s;
        }
    }

    // Final fallback: "1K/M/B/T"
    let s = format!("{:.0}{suffix}", scaled.max(0.0).round());
    if s.len() <= max_width as usize {
        return s;
    }

    // Last resort: truncate.
    s[..max_width as usize].to_string()
}

fn format_compact_scaled(value: f64, suffix: &str, decimals: usize) -> String {
    // Format with fixed decimals, then trim trailing zeros and a trailing dot.
    let mut s = format!("{:.*}", decimals, value.max(0.0));
    if decimals > 0 {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s.push_str(suffix);
    s
}

#[derive(Debug, Clone)]
pub struct UsageDay {
    pub day: String,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub total_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
}

impl UsageDay {
    pub fn short_label(&self) -> String {
        // Expect YYYY-MM-DD
        if self.day.len() == 10 {
            let month = &self.day[5..7];
            let day = &self.day[8..10];
            let month_name = match month {
                "01" => "Jan",
                "02" => "Feb",
                "03" => "Mar",
                "04" => "Apr",
                "05" => "May",
                "06" => "Jun",
                "07" => "Jul",
                "08" => "Aug",
                "09" => "Sep",
                "10" => "Oct",
                "11" => "Nov",
                "12" => "Dec",
                _ => month,
            };
            return format!("{month_name} {day}");
        }
        self.day.clone()
    }
}

#[derive(Debug, Clone)]
pub struct UsageTotalsTokens {
    pub last7_days_tokens: i64,
    pub last30_days_tokens: i64,
    pub average_daily_tokens: i64,
    pub cache_hit_rate_percent: f64,
    pub peak_day: Option<String>,
    pub peak_day_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct LocalUsageModel {
    pub model: String,
    pub tokens: i64,
    pub share_percent: f64,
}

#[derive(Debug, Clone)]
pub struct ProjectActivity {
    pub display_path: String,
    pub days: Vec<UsageDay>,
    pub last_activity_day: Option<String>,
    pub total_tokens: i64,
    pub cached_input_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
}

#[derive(Debug, Clone)]
pub struct LocalUsageSnapshot {
    pub days: Vec<UsageDay>,
    pub totals: UsageTotalsTokens,
    pub top_models: Vec<LocalUsageModel>,
    pub activity_first_weekday: Weekday,
    pub project_activity: Vec<ProjectActivity>,
    // Number of session files that were identified as belonging to the selected workspace filter.
    // When no workspace filter is used, this is 0.
    pub matched_session_files: u32,
}

#[derive(Debug, Clone)]
pub struct UsageTotalsView {
    pub last7_primary_label: String,
    pub last30_primary_label: String,
    pub avg_primary_label: String,
    pub cache_label: String,
    pub total_label: String,
    pub runs_label: String,
    pub peak_day_label: String,
    pub peak_sub_label: String,
}

impl LocalUsageSnapshot {
    pub fn last7_days(&self) -> Vec<UsageDay> {
        self.days
            .iter()
            .rev()
            .take(7)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn last_n_days(&self, n: usize) -> Vec<UsageDay> {
        self.days
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn totals_view(&self, metric: UsageMetric) -> UsageTotalsView {
        let last7 = self.last7_days();
        let last7_agent_ms: i64 = last7.iter().map(|d| d.agent_time_ms).sum();
        let last30_agent_ms: i64 = self.days.iter().map(|d| d.agent_time_ms).sum();
        let last7_runs: i64 = last7.iter().map(|d| d.agent_runs).sum();
        let last30_runs: i64 = self.days.iter().map(|d| d.agent_runs).sum();

        let (peak_day, _peak_value, peak_sub) = match metric {
            UsageMetric::Tokens => {
                let peak = self
                    .days
                    .iter()
                    .max_by_key(|d| d.total_tokens)
                    .filter(|d| d.total_tokens > 0);
                let peak_day = peak
                    .map(|d| d.short_label())
                    .unwrap_or_else(|| "--".to_string());
                let peak_tokens = peak.map(|d| d.total_tokens).unwrap_or(0);
                let sub = format!("{} tokens", format_tokens_compact(peak_tokens));
                (peak_day, peak_tokens, sub)
            }
            UsageMetric::Time => {
                let peak = self
                    .days
                    .iter()
                    .max_by_key(|d| d.agent_time_ms)
                    .filter(|d| d.agent_time_ms > 0);
                let peak_day = peak
                    .map(|d| d.short_label())
                    .unwrap_or_else(|| "--".to_string());
                let peak_ms = peak.map(|d| d.agent_time_ms).unwrap_or(0);
                let sub = format!("{} agent time", format_duration_compact(peak_ms));
                (peak_day, peak_ms, sub)
            }
            UsageMetric::Runs => {
                let peak = self
                    .days
                    .iter()
                    .max_by_key(|d| d.agent_runs)
                    .filter(|d| d.agent_runs > 0);
                let peak_day = peak
                    .map(|d| d.short_label())
                    .unwrap_or_else(|| "--".to_string());
                let peak_runs = peak.map(|d| d.agent_runs).unwrap_or(0);
                let sub = format!("{} runs", format_count(peak_runs));
                (peak_day, peak_runs, sub)
            }
        };

        match metric {
            UsageMetric::Tokens => UsageTotalsView {
                last7_primary_label: format!(
                    "{} tokens",
                    format_tokens_compact(self.totals.last7_days_tokens)
                ),
                last30_primary_label: format!(
                    "{} tokens",
                    format_tokens_compact(self.totals.last30_days_tokens)
                ),
                avg_primary_label: format_tokens_compact(self.totals.average_daily_tokens),
                cache_label: format!("{:.1}%", self.totals.cache_hit_rate_percent),
                total_label: format_count(self.totals.last30_days_tokens),
                runs_label: format!("{} runs", format_count(last7_runs)),
                peak_day_label: self.totals.peak_day.as_deref().unwrap_or("--").to_string(),
                peak_sub_label: format!(
                    "{} tokens",
                    format_tokens_compact(self.totals.peak_day_tokens)
                ),
            },
            UsageMetric::Time => {
                let avg_ms = if last7.is_empty() {
                    0
                } else {
                    (last7_agent_ms as f64 / last7.len() as f64).round() as i64
                };
                UsageTotalsView {
                    last7_primary_label: format_duration_compact(last7_agent_ms),
                    last30_primary_label: format_duration_compact(last30_agent_ms),
                    avg_primary_label: format_duration_compact(avg_ms),
                    cache_label: "--".to_string(),
                    total_label: format_duration(last30_agent_ms),
                    runs_label: format!("{} runs", format_count(last7_runs)),
                    peak_day_label: peak_day,
                    peak_sub_label: peak_sub,
                }
            }
            UsageMetric::Runs => {
                let avg_7 = if last7.is_empty() {
                    0
                } else {
                    (last7_runs as f64 / last7.len() as f64).round() as i64
                };
                UsageTotalsView {
                    last7_primary_label: format!("{} runs", format_count(last7_runs)),
                    last30_primary_label: format!("{} runs", format_count(last30_runs)),
                    avg_primary_label: format_count(avg_7),
                    cache_label: "--".to_string(),
                    total_label: format_count(last30_runs),
                    runs_label: format!("{} runs", format_count(last7_runs)),
                    peak_day_label: peak_day,
                    peak_sub_label: peak_sub,
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct DailyTotals {
    input: i64,
    cached: i64,
    output: i64,
    agent_ms: i64,
    agent_runs: i64,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct UsageTotals {
    input: i64,
    cached: i64,
    output: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ParserState {
    #[serde(default)]
    previous_totals: Option<UsageTotals>,
    #[serde(default)]
    current_model: Option<String>,
    #[serde(default)]
    last_activity_ms: Option<i64>,
    #[serde(default)]
    first_session_meta_seen: bool,
    #[serde(default)]
    fork_replay: ForkReplayState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
struct ForkReplayState {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    start_ms: Option<i64>,
    #[serde(default)]
    last_event_ms: Option<i64>,
    #[serde(default)]
    token_events: u32,
}

#[derive(Debug, Clone, Default)]
struct ScanCacheStore {
    entries: HashMap<String, CachedFileScanEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CachedFileScanEntry {
    size: u64,
    modified_epoch_secs: Option<u64>,
    #[serde(default)]
    file_offset: u64,
    #[serde(default = "default_true")]
    fully_parsed: bool,
    session_cwd: Option<String>,
    #[serde(default)]
    parser_state: ParserState,
    #[serde(default)]
    daily: HashMap<String, DailyTotals>,
    #[serde(default)]
    model_totals_by_day: HashMap<String, HashMap<String, i64>>,
    updated_at: i64,
}

#[derive(Debug, Clone, Default)]
struct FileScanSummary {
    session_cwd: Option<String>,
    parser_state: ParserState,
    file_offset: u64,
    fully_parsed: bool,
    daily: HashMap<String, DailyTotals>,
    model_totals_by_day: HashMap<String, HashMap<String, i64>>,
}

#[derive(Debug, Clone, Default)]
struct ProjectActivityBuilder {
    display_path: String,
    daily: HashMap<String, DailyTotals>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug)]
struct ScanCacheDb {
    path: PathBuf,
    conn: Connection,
}

pub fn resolve_codex_home(override_home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_home {
        return Some(path);
    }
    if let Ok(value) = std::env::var("CODEX_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(value) = std::env::var("HOME") {
        if !value.trim().is_empty() {
            return Some(PathBuf::from(value).join(".codex"));
        }
    }
    if let Ok(value) = std::env::var("USERPROFILE") {
        if !value.trim().is_empty() {
            return Some(PathBuf::from(value).join(".codex"));
        }
    }
    None
}

pub fn compute_snapshot(
    days: u32,
    codex_home: &Path,
    workspace_path: Option<&Path>,
    limits: ScanLimits,
    scan_cache_db_path: Option<&Path>,
) -> Result<LocalUsageSnapshot> {
    let days = days.clamp(1, 90);

    let sessions_root = codex_home.join("sessions");
    let chart_day_keys = make_day_keys(days);
    let chart_day_filter: HashSet<String> = chart_day_keys.iter().cloned().collect();
    let activity_first_weekday = system_first_weekday();
    let activity_day_keys = make_activity_day_keys(activity_first_weekday);
    let activity_oldest_day = activity_day_keys.first().cloned();
    let mut scan_day_keys = activity_day_keys.clone();
    for day_key in &chart_day_keys {
        if !scan_day_keys.contains(day_key) {
            scan_day_keys.push(day_key.clone());
        }
    }
    let mut daily: HashMap<String, DailyTotals> = scan_day_keys
        .iter()
        .map(|key| (key.clone(), DailyTotals::default()))
        .collect();
    let mut model_totals: HashMap<String, i64> = HashMap::new();
    let mut project_activity: HashMap<String, ProjectActivityBuilder> = HashMap::new();

    if !sessions_root.exists() {
        return Ok(build_snapshot(
            chart_day_keys,
            daily,
            model_totals,
            0,
            activity_first_weekday,
            activity_day_keys,
            project_activity,
        ));
    }

    // Build a full candidate list (metadata-only).
    let mut candidates = collect_session_file_candidates(&sessions_root);
    // Newest activity first.
    candidates.sort_by(|a, b| {
        b.modified_epoch_secs
            .cmp(&a.modified_epoch_secs)
            .then_with(|| a.len.cmp(&b.len))
    });

    // Prefer scanning by file mtime instead of directory date: long-running sessions can live in an
    // older day folder but still accrue usage today.
    let cutoff_epoch_secs = if limits.full_scan {
        None
    } else {
        activity_oldest_day
            .as_deref()
            .and_then(epoch_secs_for_local_day_start)
            .or_else(|| {
                SystemTime::now()
                    .checked_sub(StdDuration::from_secs(
                        (ACTIVITY_TIMELINE_DAYS as u64).saturating_mul(24 * 60 * 60),
                    ))
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs())
            })
    };

    let force_reparse_all = limits.full_scan && limits.scan_time_budget_ms == 0;
    let mut planned_indices: Vec<usize> = Vec::new();
    let mut planned_total_bytes: u64 = 0;
    let mut planned_files: usize = 0;
    for (idx, candidate) in candidates.iter().enumerate() {
        if let (Some(cutoff), Some(modified)) = (cutoff_epoch_secs, candidate.modified_epoch_secs) {
            if modified < cutoff {
                continue;
            }
        }
        if limits.full_scan {
            planned_indices.push(idx);
            continue;
        }
        if planned_files >= limits.max_session_files_scanned
            || planned_total_bytes >= limits.max_session_total_bytes
        {
            break;
        }
        let candidate_weight = candidate.len.min(limits.max_session_file_bytes.max(1));
        if planned_files > 0
            && planned_total_bytes.saturating_add(candidate_weight) > limits.max_session_total_bytes
        {
            continue;
        }
        planned_indices.push(idx);
        planned_files += 1;
        planned_total_bytes = planned_total_bytes.saturating_add(candidate_weight);
    }

    let mut matched_session_files: u32 = 0;
    let mut scan_cache_db = scan_cache_db_path
        .map(open_or_init_scan_cache_db)
        .transpose()?;
    let candidate_paths: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.path.to_string_lossy().to_string())
        .collect();
    let planned_set: HashSet<usize> = planned_indices.iter().copied().collect();
    let (mut scan_cache_store, mut removed_cache_paths) = if let Some(db) = scan_cache_db.as_ref() {
        load_scan_cache_store(db)?
    } else {
        (ScanCacheStore::default(), HashSet::new())
    };
    let mut dirty_cache_paths: HashSet<String> = HashSet::new();
    let scan_deadline = if limits.scan_time_budget_ms == 0 {
        None
    } else {
        Instant::now().checked_add(StdDuration::from_millis(limits.scan_time_budget_ms))
    };

    if scan_cache_db.is_some() {
        let valid_paths: HashSet<&str> =
            candidate_paths.iter().map(|value| value.as_str()).collect();
        scan_cache_store.entries.retain(|file_path, _| {
            let keep = valid_paths.contains(file_path.as_str());
            if !keep {
                removed_cache_paths.insert(file_path.clone());
            }
            keep
        });

        for (idx, candidate) in candidates.iter().enumerate() {
            let candidate_key = &candidate_paths[idx];
            let cached_entry = scan_cache_store.entries.get(candidate_key).cloned();
            let cached_entry_matches = if force_reparse_all {
                None
            } else {
                cached_entry
                    .as_ref()
                    .filter(|entry| cache_entry_matches_candidate(entry, candidate))
                    .cloned()
            };

            if planned_set.contains(&idx) {
                let entry = if let Some(entry) = cached_entry_matches {
                    entry
                } else {
                    if let Some(deadline) = scan_deadline {
                        if Instant::now() >= deadline {
                            if let Some(stale) = cached_entry {
                                apply_cached_file_entry(
                                    &stale,
                                    workspace_path,
                                    &mut daily,
                                    &mut model_totals,
                                    &chart_day_filter,
                                    &mut project_activity,
                                    &mut matched_session_files,
                                );
                            }
                            continue;
                        }
                    }

                    let parsed = match parse_file_summary(
                        &candidate.path,
                        limits.max_jsonl_line_bytes,
                        cached_entry.as_ref(),
                        scan_deadline,
                    ) {
                        Ok(parsed) => parsed,
                        Err(_) => {
                            if let Some(stale) = cached_entry {
                                apply_cached_file_entry(
                                    &stale,
                                    workspace_path,
                                    &mut daily,
                                    &mut model_totals,
                                    &chart_day_filter,
                                    &mut project_activity,
                                    &mut matched_session_files,
                                );
                            }
                            continue;
                        }
                    };
                    let entry = CachedFileScanEntry {
                        size: candidate.len,
                        modified_epoch_secs: candidate.modified_epoch_secs,
                        file_offset: parsed.file_offset.min(candidate.len),
                        fully_parsed: parsed.fully_parsed,
                        session_cwd: parsed.session_cwd,
                        parser_state: parsed.parser_state,
                        daily: parsed.daily,
                        model_totals_by_day: parsed.model_totals_by_day,
                        updated_at: unix_time_seconds(),
                    };
                    scan_cache_store
                        .entries
                        .insert(candidate_key.clone(), entry.clone());
                    dirty_cache_paths.insert(candidate_key.clone());
                    entry
                };
                apply_cached_file_entry(
                    &entry,
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &chart_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
                continue;
            }

            if let Some(entry) = cached_entry_matches {
                apply_cached_file_entry(
                    &entry,
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &chart_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
                continue;
            }

            if let Some(stale) = cached_entry {
                apply_cached_file_entry(
                    &stale,
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &chart_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
            }
        }
    } else {
        for idx in planned_indices {
            let candidate = &candidates[idx];
            let parsed =
                parse_file_summary(&candidate.path, limits.max_jsonl_line_bytes, None, None)?;
            let entry = CachedFileScanEntry {
                size: candidate.len,
                modified_epoch_secs: candidate.modified_epoch_secs,
                file_offset: parsed.file_offset.min(candidate.len),
                fully_parsed: parsed.fully_parsed,
                session_cwd: parsed.session_cwd,
                parser_state: parsed.parser_state,
                daily: parsed.daily,
                model_totals_by_day: parsed.model_totals_by_day,
                updated_at: unix_time_seconds(),
            };
            apply_cached_file_entry(
                &entry,
                workspace_path,
                &mut daily,
                &mut model_totals,
                &chart_day_filter,
                &mut project_activity,
                &mut matched_session_files,
            );
        }
    }

    if let Some(scan_cache_db) = scan_cache_db.as_mut() {
        if !dirty_cache_paths.is_empty() || !removed_cache_paths.is_empty() {
            let _ = persist_scan_cache_changes(
                scan_cache_db,
                &scan_cache_store,
                &removed_cache_paths,
                &dirty_cache_paths,
            );
        }
        let _ = trim_scan_cache_db_entries_to_limit(
            scan_cache_db,
            limits.scan_cache_max_entries.max(1),
        );
    }

    Ok(build_snapshot(
        chart_day_keys,
        daily,
        model_totals,
        matched_session_files,
        activity_first_weekday,
        activity_day_keys,
        project_activity,
    ))
}

#[derive(Debug, Clone)]
struct SessionFileCandidate {
    path: PathBuf,
    len: u64,
    modified_epoch_secs: Option<u64>,
}

fn collect_session_file_candidates(sessions_root: &Path) -> Vec<SessionFileCandidate> {
    let mut out: Vec<SessionFileCandidate> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![sessions_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Never follow symlinks or read special files (FIFOs/devices) from an untrusted sessions tree.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let len = meta.len();
            if len == 0 {
                continue;
            }

            let modified = meta.modified().ok();
            let modified_epoch_secs = modified
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            out.push(SessionFileCandidate {
                path,
                len,
                modified_epoch_secs,
            });
        }
    }

    out
}

fn cache_entry_matches_candidate(
    entry: &CachedFileScanEntry,
    candidate: &SessionFileCandidate,
) -> bool {
    entry.fully_parsed
        && entry.size == candidate.len
        && entry.modified_epoch_secs == candidate.modified_epoch_secs
        && entry.file_offset >= candidate.len
}

fn build_snapshot(
    day_keys: Vec<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals: HashMap<String, i64>,
    matched_session_files: u32,
    activity_first_weekday: Weekday,
    activity_day_keys: Vec<String>,
    project_activity: HashMap<String, ProjectActivityBuilder>,
) -> LocalUsageSnapshot {
    let mut days: Vec<UsageDay> = Vec::with_capacity(day_keys.len());
    let mut total_tokens = 0i64;

    for day_key in &day_keys {
        let totals = daily.get(day_key).copied().unwrap_or_default();
        let total = totals.input + totals.output;
        total_tokens += total;
        days.push(UsageDay {
            day: day_key.clone(),
            input_tokens: totals.input,
            cached_input_tokens: totals.cached,
            total_tokens: total,
            agent_time_ms: totals.agent_ms,
            agent_runs: totals.agent_runs,
        });
    }

    let last7 = days.iter().rev().take(7).cloned().collect::<Vec<_>>();
    let last7_tokens: i64 = last7.iter().map(|day| day.total_tokens).sum();
    let last7_input: i64 = last7.iter().map(|day| day.input_tokens).sum();
    let last7_cached: i64 = last7.iter().map(|day| day.cached_input_tokens).sum();

    let average_daily_tokens = if last7.is_empty() {
        0
    } else {
        ((last7_tokens as f64) / (last7.len() as f64)).round() as i64
    };

    let cache_hit_rate_percent = if last7_input > 0 {
        ((last7_cached as f64) / (last7_input as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let peak = days
        .iter()
        .max_by_key(|day| day.total_tokens)
        .filter(|day| day.total_tokens > 0);
    let peak_day = peak.map(|day| day.day.clone());
    let peak_day_tokens = peak.map(|day| day.total_tokens).unwrap_or(0);

    let mut top_models: Vec<LocalUsageModel> = model_totals
        .into_iter()
        .filter(|(model, tokens)| model != "unknown" && *tokens > 0)
        .map(|(model, tokens)| LocalUsageModel {
            model,
            tokens,
            share_percent: if total_tokens > 0 {
                ((tokens as f64) / (total_tokens as f64) * 1000.0).round() / 10.0
            } else {
                0.0
            },
        })
        .collect();
    top_models.sort_by(|a, b| b.tokens.cmp(&a.tokens));
    top_models.truncate(4);

    LocalUsageSnapshot {
        days,
        totals: UsageTotalsTokens {
            last7_days_tokens: last7_tokens,
            last30_days_tokens: total_tokens,
            average_daily_tokens,
            cache_hit_rate_percent,
            peak_day,
            peak_day_tokens,
        },
        top_models,
        activity_first_weekday,
        project_activity: build_project_activity(activity_day_keys, project_activity),
        matched_session_files,
    }
}

fn build_project_activity(
    day_keys: Vec<String>,
    projects: HashMap<String, ProjectActivityBuilder>,
) -> Vec<ProjectActivity> {
    let mut out: Vec<ProjectActivity> = Vec::with_capacity(projects.len());

    for (_, project) in projects {
        let mut days: Vec<UsageDay> = Vec::with_capacity(day_keys.len());
        let mut total_tokens = 0i64;
        let mut cached_input_tokens = 0i64;
        let mut agent_time_ms = 0i64;
        let mut agent_runs = 0i64;
        let mut last_activity_day: Option<String> = None;

        for day_key in &day_keys {
            let totals = project.daily.get(day_key).copied().unwrap_or_default();
            let total = totals.input + totals.output;
            total_tokens += total;
            cached_input_tokens += totals.cached;
            agent_time_ms += totals.agent_ms;
            agent_runs += totals.agent_runs;
            if daily_has_activity(totals) {
                last_activity_day = Some(day_key.clone());
            }
            days.push(UsageDay {
                day: day_key.clone(),
                input_tokens: totals.input,
                cached_input_tokens: totals.cached,
                total_tokens: total,
                agent_time_ms: totals.agent_ms,
                agent_runs: totals.agent_runs,
            });
        }

        if last_activity_day.is_none() {
            continue;
        }

        out.push(ProjectActivity {
            display_path: project.display_path,
            days,
            last_activity_day,
            total_tokens,
            cached_input_tokens,
            agent_time_ms,
            agent_runs,
        });
    }

    out.sort_by(|left, right| {
        right
            .last_activity_day
            .cmp(&left.last_activity_day)
            .then_with(|| left.display_path.cmp(&right.display_path))
    });
    out
}

fn daily_has_activity(totals: DailyTotals) -> bool {
    totals.input > 0 || totals.output > 0 || totals.agent_ms > 0 || totals.agent_runs > 0
}

fn add_model_tokens_limited(
    model_totals: &mut HashMap<String, i64>,
    model: String,
    delta_tokens: i64,
) {
    if delta_tokens <= 0 {
        return;
    }
    if model_totals.len() <= MAX_DISTINCT_MODELS || model_totals.contains_key(&model) {
        *model_totals.entry(model).or_insert(0) += delta_tokens;
    } else {
        *model_totals.entry("other".to_string()).or_insert(0) += delta_tokens;
    }
}

fn apply_cached_file_entry(
    entry: &CachedFileScanEntry,
    workspace_path: Option<&Path>,
    daily: &mut HashMap<String, DailyTotals>,
    model_totals: &mut HashMap<String, i64>,
    chart_day_filter: &HashSet<String>,
    project_activity: &mut HashMap<String, ProjectActivityBuilder>,
    matched_session_files: &mut u32,
) {
    let matches_workspace = match workspace_path {
        None => true,
        Some(filter) => entry
            .session_cwd
            .as_deref()
            .map(|cwd| path_matches_workspace(cwd, filter))
            .unwrap_or(false),
    };
    if !matches_workspace {
        return;
    }

    if workspace_path.is_some() {
        *matched_session_files = matched_session_files.saturating_add(1);
    }

    for (day_key, totals) in &entry.daily {
        if let Some(dst) = daily.get_mut(day_key) {
            dst.input += totals.input;
            dst.cached += totals.cached;
            dst.output += totals.output;
            dst.agent_ms += totals.agent_ms;
            dst.agent_runs += totals.agent_runs;
        }
    }

    for (day_key, per_day_models) in &entry.model_totals_by_day {
        if !chart_day_filter.contains(day_key) {
            continue;
        }
        for (model, tokens) in per_day_models {
            add_model_tokens_limited(model_totals, model.clone(), *tokens);
        }
    }

    if let Some(cwd) = entry.session_cwd.as_deref() {
        apply_project_activity(cwd, &entry.daily, daily, project_activity);
    }
}

fn apply_project_activity(
    cwd: &str,
    entry_daily: &HashMap<String, DailyTotals>,
    day_filter: &HashMap<String, DailyTotals>,
    project_activity: &mut HashMap<String, ProjectActivityBuilder>,
) {
    if cwd.trim().is_empty() {
        return;
    }
    let key = normalize_project_key(cwd);
    if key.is_empty() {
        return;
    }

    let builder = project_activity.entry(key).or_default();
    if builder.display_path.is_empty()
        || cwd.len() < builder.display_path.len()
        || (cwd.len() == builder.display_path.len() && cwd < builder.display_path.as_str())
    {
        builder.display_path = cwd.to_string();
    }

    for (day_key, totals) in entry_daily {
        if !day_filter.contains_key(day_key) {
            continue;
        }
        let dst = builder.daily.entry(day_key.clone()).or_default();
        dst.input += totals.input;
        dst.cached += totals.cached;
        dst.output += totals.output;
        dst.agent_ms += totals.agent_ms;
        dst.agent_runs += totals.agent_runs;
    }
}

fn parse_file_summary(
    path: &Path,
    max_jsonl_line_bytes: usize,
    existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(FileScanSummary::default()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(FileScanSummary::default());
    }
    if meta.len() == 0 {
        return Ok(FileScanSummary::default());
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(FileScanSummary::default()),
    };
    let file_len = meta.len();
    let current_modified_epoch = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());

    let can_resume = existing
        .filter(|entry| entry.file_offset > 0 && entry.file_offset <= file_len)
        .filter(|entry| {
            if entry.size < file_len {
                return true;
            }
            entry.size == file_len
                && !entry.fully_parsed
                && entry.modified_epoch_secs == current_modified_epoch
        })
        .is_some();
    let mut file_offset: u64 = if can_resume {
        existing.map(|entry| entry.file_offset).unwrap_or(0)
    } else {
        0
    };
    if file_offset > 0 {
        let _ = file.seek(SeekFrom::Start(file_offset));
    }

    let mut daily: HashMap<String, DailyTotals> = if can_resume {
        existing
            .map(|entry| entry.daily.clone())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut model_totals_by_day: HashMap<String, HashMap<String, i64>> = if can_resume {
        existing
            .map(|entry| entry.model_totals_by_day.clone())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut session_cwd: Option<String> = if can_resume {
        existing.and_then(|entry| entry.session_cwd.clone())
    } else {
        None
    };
    let mut parser_state = if can_resume {
        existing
            .map(|entry| entry.parser_state.clone())
            .unwrap_or_default()
    } else {
        ParserState::default()
    };
    let mut reader = BufReader::new(file);
    let mut previous_totals: Option<UsageTotals> = parser_state.previous_totals;
    let mut current_model: Option<String> = parser_state.current_model.clone();
    let mut last_activity_ms: Option<i64> = parser_state.last_activity_ms;
    let mut first_session_meta_seen = parser_state.first_session_meta_seen;
    let mut fork_replay = parser_state.fork_replay;
    let mut seen_runs: HashSet<i64> = HashSet::new();
    let mut line = String::new();
    let mut fully_parsed = true;

    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                fully_parsed = false;
                break;
            }
        }

        line.clear();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        file_offset = file_offset.saturating_add(bytes_read as u64);
        if line.len() > max_jsonl_line_bytes {
            continue;
        }

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let entry_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");

        if (entry_type == "session_meta" || entry_type == "turn_context") && session_cwd.is_none() {
            session_cwd = extract_cwd(&value);
        }

        if entry_type == "session_meta" {
            maybe_start_fork_replay(&value, &mut first_session_meta_seen, &mut fork_replay);
        }

        let event_timestamp_ms = read_timestamp_ms(&value);
        let skip_fork_replay = fork_replay_should_skip_event(&mut fork_replay, event_timestamp_ms);

        if entry_type == "turn_context" {
            if !skip_fork_replay {
                if let Some(model) = extract_model_from_turn_context(&value) {
                    current_model = Some(model);
                }
            }
            continue;
        }

        if entry_type == "session_meta" {
            continue;
        }

        if entry_type == "event_msg" || entry_type.is_empty() {
            let payload = value.get("payload").and_then(|value| value.as_object());
            let payload_type = payload
                .and_then(|payload| payload.get("type"))
                .and_then(|value| value.as_str());

            if skip_fork_replay && payload_type != Some("token_count") {
                continue;
            }

            if payload_type == Some("agent_message") {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    if seen_runs.insert(timestamp_ms) {
                        if let Some(day_key) = day_key_for_timestamp_ms(timestamp_ms) {
                            daily.entry(day_key).or_default().agent_runs += 1;
                        }
                    }
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
                continue;
            }

            if payload_type == Some("agent_reasoning") {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
                continue;
            }

            if payload_type != Some("token_count") {
                continue;
            }

            let info = payload
                .and_then(|payload| payload.get("info"))
                .and_then(|v| v.as_object());
            let (input, cached, output, used_total) = if let Some(info) = info {
                if let Some(total) = find_usage_map(info, &["total_token_usage", "totalTokenUsage"])
                {
                    (
                        read_i64(total, &["input_tokens", "inputTokens"]),
                        read_i64(
                            total,
                            &[
                                "cached_input_tokens",
                                "cache_read_input_tokens",
                                "cachedInputTokens",
                                "cacheReadInputTokens",
                            ],
                        ),
                        read_i64(total, &["output_tokens", "outputTokens"]),
                        true,
                    )
                } else if let Some(last) =
                    find_usage_map(info, &["last_token_usage", "lastTokenUsage"])
                {
                    (
                        read_i64(last, &["input_tokens", "inputTokens"]),
                        read_i64(
                            last,
                            &[
                                "cached_input_tokens",
                                "cache_read_input_tokens",
                                "cachedInputTokens",
                                "cacheReadInputTokens",
                            ],
                        ),
                        read_i64(last, &["output_tokens", "outputTokens"]),
                        false,
                    )
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let mut delta = UsageTotals {
                input,
                cached,
                output,
            };

            if used_total {
                let prev = previous_totals.unwrap_or_default();
                delta = UsageTotals {
                    input: (input - prev.input).max(0),
                    cached: (cached - prev.cached).max(0),
                    output: (output - prev.output).max(0),
                };
                previous_totals = Some(UsageTotals {
                    input,
                    cached,
                    output,
                });
            } else {
                let mut next = previous_totals.unwrap_or_default();
                next.input += delta.input;
                next.cached += delta.cached;
                next.output += delta.output;
                previous_totals = Some(next);
            }

            if skip_fork_replay {
                note_fork_replay_token(&mut fork_replay);
                continue;
            }

            if delta.input == 0 && delta.cached == 0 && delta.output == 0 {
                continue;
            }

            let timestamp_ms = event_timestamp_ms;
            if let Some(day_key) = timestamp_ms.and_then(day_key_for_timestamp_ms) {
                let entry = daily.entry(day_key.clone()).or_default();
                let cached_clamped = delta.cached.min(delta.input);
                entry.input += delta.input;
                entry.cached += cached_clamped;
                entry.output += delta.output;

                let model = current_model
                    .clone()
                    .or_else(|| extract_model_from_token_count(&value))
                    .unwrap_or_else(|| "unknown".to_string());
                let per_day_models = model_totals_by_day.entry(day_key).or_default();
                add_model_tokens_limited(per_day_models, model, delta.input + delta.output);
            }

            if let Some(timestamp_ms) = timestamp_ms {
                track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
            }
            continue;
        }

        if skip_fork_replay {
            continue;
        }

        if entry_type == "response_item" {
            let payload = value.get("payload").and_then(|value| value.as_object());
            let payload_type = payload
                .and_then(|payload| payload.get("type"))
                .and_then(|value| value.as_str());
            let role = payload
                .and_then(|payload| payload.get("role"))
                .and_then(|value| value.as_str())
                .unwrap_or("");

            if role == "assistant" {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    if seen_runs.insert(timestamp_ms) {
                        if let Some(day_key) = day_key_for_timestamp_ms(timestamp_ms) {
                            daily.entry(day_key).or_default().agent_runs += 1;
                        }
                    }
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
            } else if payload_type != Some("message") {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
            }
        }
    }

    parser_state.previous_totals = previous_totals;
    parser_state.current_model = current_model;
    parser_state.last_activity_ms = last_activity_ms;
    parser_state.first_session_meta_seen = first_session_meta_seen;
    parser_state.fork_replay = fork_replay;

    Ok(FileScanSummary {
        session_cwd,
        parser_state,
        file_offset: file_offset.min(file_len),
        fully_parsed: fully_parsed && file_offset >= file_len,
        daily,
        model_totals_by_day,
    })
}

fn open_or_init_scan_cache_db(path: &Path) -> Result<ScanCacheDb> {
    if let Some(parent) = path.parent() {
        crate::storage::ensure_private_dir(parent)?;
        ensure_directory_not_symlink(parent, "cache database parent directory")?;
    }
    ensure_regular_file_or_missing(path, "cache database file")?;
    let mut wal_path: OsString = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let mut shm_path: OsString = path.as_os_str().to_os_string();
    shm_path.push("-shm");
    ensure_regular_file_or_missing(Path::new(&wal_path), "cache database WAL file")?;
    ensure_regular_file_or_missing(Path::new(&shm_path), "cache database SHM file")?;

    let conn = Connection::open(path)
        .with_context(|| format!("Unable to open cache database {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("Unable to set WAL journal mode for {}", path.display()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .with_context(|| format!("Unable to set synchronous mode for {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .with_context(|| format!("Unable to enable foreign keys for {}", path.display()))?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS cache_meta (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS file_cache (
            file_path TEXT PRIMARY KEY,
            file_size INTEGER NOT NULL,
            file_mtime INTEGER,
            file_offset INTEGER NOT NULL DEFAULT 0,
            fully_parsed INTEGER NOT NULL DEFAULT 1,
            session_cwd TEXT,
            parser_state_json TEXT NOT NULL DEFAULT '{}',
            daily_json TEXT NOT NULL,
            model_daily_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_file_cache_updated_at
            ON file_cache(updated_at);
        ",
    )
    .with_context(|| format!("Unable to initialize cache database {}", path.display()))?;
    conn.execute(
        "
        INSERT INTO cache_meta(key, value)
        SELECT 'schema_version', ?1
        WHERE NOT EXISTS (
            SELECT 1 FROM cache_meta WHERE key = 'schema_version'
        );
        ",
        params![SCAN_CACHE_DB_SCHEMA_VERSION],
    )
    .with_context(|| {
        format!(
            "Unable to write schema version metadata for {}",
            path.display()
        )
    })?;

    let schema_version: Option<i64> = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'schema_version';",
            [],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| {
            format!(
                "Unable to read schema version metadata for {}",
                path.display()
            )
        })?;
    let Some(schema_version) = schema_version else {
        anyhow::bail!("Missing scan cache schema version metadata");
    };
    let has_v2_columns = table_has_column(&conn, "file_cache", "file_offset")?
        && table_has_column(&conn, "file_cache", "fully_parsed")?
        && table_has_column(&conn, "file_cache", "parser_state_json")?;
    if schema_version == 1 || !has_v2_columns {
        migrate_scan_cache_db_v1_to_v2(&conn, path)?;
    }
    if schema_version < SCAN_CACHE_DB_SCHEMA_VERSION {
        migrate_scan_cache_db_to_v3(&conn, path)?;
        conn.execute(
            "UPDATE cache_meta SET value = ?1 WHERE key = 'schema_version';",
            params![SCAN_CACHE_DB_SCHEMA_VERSION],
        )
        .with_context(|| {
            format!(
                "Unable to update schema version metadata for {}",
                path.display()
            )
        })?;
    }
    let effective_schema_version: Option<i64> = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'schema_version';",
            [],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| {
            format!(
                "Unable to re-read schema version metadata for {}",
                path.display()
            )
        })?;
    let Some(effective_schema_version) = effective_schema_version else {
        anyhow::bail!("Missing scan cache schema version metadata after migration");
    };
    if effective_schema_version != SCAN_CACHE_DB_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported scan cache schema version: {} (expected {})",
            effective_schema_version,
            SCAN_CACHE_DB_SCHEMA_VERSION
        );
    }

    let db = ScanCacheDb {
        path: path.to_path_buf(),
        conn,
    };
    enforce_scan_cache_db_permissions(&db.path)?;
    Ok(db)
}

fn migrate_scan_cache_db_v1_to_v2(conn: &Connection, path: &Path) -> Result<()> {
    if !table_has_column(conn, "file_cache", "file_offset")? {
        conn.execute(
            "ALTER TABLE file_cache ADD COLUMN file_offset INTEGER NOT NULL DEFAULT 0;",
            [],
        )
        .with_context(|| {
            format!(
                "Unable to add file_offset column while migrating {}",
                path.display()
            )
        })?;
    }
    if !table_has_column(conn, "file_cache", "fully_parsed")? {
        conn.execute(
            "ALTER TABLE file_cache ADD COLUMN fully_parsed INTEGER NOT NULL DEFAULT 1;",
            [],
        )
        .with_context(|| {
            format!(
                "Unable to add fully_parsed column while migrating {}",
                path.display()
            )
        })?;
    }
    if !table_has_column(conn, "file_cache", "parser_state_json")? {
        conn.execute(
            "ALTER TABLE file_cache ADD COLUMN parser_state_json TEXT NOT NULL DEFAULT '{}';",
            [],
        )
        .with_context(|| {
            format!(
                "Unable to add parser_state_json column while migrating {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn migrate_scan_cache_db_to_v3(conn: &Connection, path: &Path) -> Result<()> {
    // v3 changes forked-session accounting semantics. Reusing those old rows would keep
    // inflated totals, but non-forked cache rows are still valid.
    let mut stmt = conn
        .prepare("SELECT file_path FROM file_cache;")
        .with_context(|| {
            format!(
                "Unable to list cache rows while migrating {}",
                path.display()
            )
        })?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .with_context(|| {
            format!(
                "Unable to read cache row paths while migrating {}",
                path.display()
            )
        })?;
    let mut stale_paths = Vec::new();
    for row in rows {
        let file_path = row.with_context(|| {
            format!(
                "Unable to read cache row path while migrating {}",
                path.display()
            )
        })?;
        if cached_session_needs_v3_reparse(&file_path) {
            stale_paths.push(file_path);
        }
    }
    drop(stmt);

    let mut delete_stmt = conn
        .prepare("DELETE FROM file_cache WHERE file_path = ?1;")
        .with_context(|| {
            format!(
                "Unable to prepare stale row delete while migrating {}",
                path.display()
            )
        })?;
    for file_path in stale_paths {
        delete_stmt
            .execute(params![file_path])
            .with_context(|| format!("Unable to clear stale cache row in {}", path.display()))?;
    }
    Ok(())
}

fn cached_session_needs_v3_reparse(file_path: &str) -> bool {
    let file = match File::open(file_path) {
        Ok(file) => file,
        Err(_) => return true,
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .ok()
        .filter(|bytes| *bytes > 0)
        .is_none()
    {
        return true;
    }
    let value = match serde_json::from_str::<Value>(&line) {
        Ok(value) => value,
        Err(_) => return true,
    };
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|entry_type| entry_type == "session_meta")
        && value
            .get("payload")
            .and_then(|payload| payload.get("forked_from_id"))
            .and_then(Value::as_str)
            .is_some()
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table});");
    let mut stmt = conn
        .prepare(&pragma)
        .with_context(|| format!("Unable to inspect table metadata for {table}"))?;
    let mut rows = stmt
        .query([])
        .with_context(|| format!("Unable to query table metadata for {table}"))?;
    while let Some(row) = rows
        .next()
        .with_context(|| format!("Unable to read table metadata row for {table}"))?
    {
        let name: String = row.get(1).with_context(|| {
            format!("Unable to read column name while inspecting table {table}")
        })?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_scan_cache_store(db: &ScanCacheDb) -> Result<(ScanCacheStore, HashSet<String>)> {
    let mut store = ScanCacheStore::default();
    let mut invalid_paths: HashSet<String> = HashSet::new();
    let mut stmt = db
        .conn
        .prepare(
            "
            SELECT
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                session_cwd,
                parser_state_json,
                daily_json,
                model_daily_json,
                updated_at
            FROM file_cache;
            ",
        )
        .with_context(|| format!("Unable to query cache entries from {}", db.path.display()))?;
    let mut rows = stmt
        .query([])
        .with_context(|| format!("Unable to iterate cache entries from {}", db.path.display()))?;
    while let Some(row) = rows
        .next()
        .with_context(|| format!("Unable to read cache row from {}", db.path.display()))?
    {
        let file_path: String = row.get(0).with_context(|| {
            format!(
                "Unable to read file_path from cache row in {}",
                db.path.display()
            )
        })?;
        let file_size_raw: i64 = row.get(1).with_context(|| {
            format!(
                "Unable to read file_size from cache row in {}",
                db.path.display()
            )
        })?;
        let file_mtime_raw: Option<i64> = row.get(2).with_context(|| {
            format!(
                "Unable to read file_mtime from cache row in {}",
                db.path.display()
            )
        })?;
        let file_offset_raw: i64 = row.get(3).with_context(|| {
            format!(
                "Unable to read file_offset from cache row in {}",
                db.path.display()
            )
        })?;
        let fully_parsed_raw: i64 = row.get(4).with_context(|| {
            format!(
                "Unable to read fully_parsed from cache row in {}",
                db.path.display()
            )
        })?;
        let session_cwd: Option<String> = row.get(5).with_context(|| {
            format!(
                "Unable to read session_cwd from cache row in {}",
                db.path.display()
            )
        })?;
        let parser_state_json: String = row.get(6).with_context(|| {
            format!(
                "Unable to read parser_state_json from cache row in {}",
                db.path.display()
            )
        })?;
        let daily_json: String = row.get(7).with_context(|| {
            format!(
                "Unable to read daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let model_daily_json: String = row.get(8).with_context(|| {
            format!(
                "Unable to read model_daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let updated_at: i64 = row.get(9).with_context(|| {
            format!(
                "Unable to read updated_at from cache row in {}",
                db.path.display()
            )
        })?;

        let Ok(file_size) = u64::try_from(file_size_raw.max(0)) else {
            invalid_paths.insert(file_path);
            continue;
        };
        let file_mtime = file_mtime_raw.and_then(|value| u64::try_from(value).ok());
        let file_offset = match u64::try_from(file_offset_raw.max(0)) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let parser_state = match serde_json::from_str::<ParserState>(&parser_state_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let daily = match serde_json::from_str::<HashMap<String, DailyTotals>>(&daily_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let model_totals_by_day = match serde_json::from_str::<HashMap<String, HashMap<String, i64>>>(
            &model_daily_json,
        ) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };

        store.entries.insert(
            file_path,
            CachedFileScanEntry {
                size: file_size,
                modified_epoch_secs: file_mtime,
                file_offset,
                fully_parsed: fully_parsed_raw != 0,
                session_cwd,
                parser_state,
                daily,
                model_totals_by_day,
                updated_at,
            },
        );
    }

    Ok((store, invalid_paths))
}

#[cfg(test)]
fn prune_scan_cache_store(
    store: &mut ScanCacheStore,
    sessions_root: &Path,
    max_session_file_bytes: u64,
    max_entries: usize,
    removed_paths: &mut HashSet<String>,
) -> bool {
    let mut pruned = false;

    store.entries.retain(|entry_path, _| {
        let path = Path::new(entry_path);
        let keep = is_valid_cached_session_file(path, sessions_root, max_session_file_bytes);
        if !keep {
            removed_paths.insert(entry_path.clone());
            pruned = true;
        }
        keep
    });

    trim_scan_cache_to_limit(store, max_entries.max(1), removed_paths) || pruned
}

#[cfg(test)]
fn trim_scan_cache_to_limit(
    store: &mut ScanCacheStore,
    max_entries: usize,
    removed_paths: &mut HashSet<String>,
) -> bool {
    if store.entries.len() <= max_entries {
        return false;
    }
    let mut entries_by_age: Vec<(String, i64)> = store
        .entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.updated_at))
        .collect();
    entries_by_age
        .sort_unstable_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

    let remove_count = entries_by_age.len().saturating_sub(max_entries);
    for (key, _) in entries_by_age.into_iter().take(remove_count) {
        store.entries.remove(&key);
        removed_paths.insert(key);
    }
    true
}

fn persist_scan_cache_changes(
    db: &mut ScanCacheDb,
    store: &ScanCacheStore,
    removed_paths: &HashSet<String>,
    dirty_paths: &HashSet<String>,
) -> Result<()> {
    if removed_paths.is_empty() && dirty_paths.is_empty() {
        return Ok(());
    }

    let tx = db
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .with_context(|| {
            format!(
                "Unable to start cache transaction for {}",
                db.path.display()
            )
        })?;

    let mut delete_stmt = tx
        .prepare("DELETE FROM file_cache WHERE file_path = ?1;")
        .with_context(|| {
            format!(
                "Unable to prepare delete statement for {}",
                db.path.display()
            )
        })?;
    let mut upsert_stmt = tx
        .prepare(
            "
            INSERT INTO file_cache(
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                session_cwd,
                parser_state_json,
                daily_json,
                model_daily_json,
                updated_at
            )
            VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(file_path) DO UPDATE SET
                file_size=excluded.file_size,
                file_mtime=excluded.file_mtime,
                file_offset=excluded.file_offset,
                fully_parsed=excluded.fully_parsed,
                session_cwd=excluded.session_cwd,
                parser_state_json=excluded.parser_state_json,
                daily_json=excluded.daily_json,
                model_daily_json=excluded.model_daily_json,
                updated_at=excluded.updated_at;
            ",
        )
        .with_context(|| {
            format!(
                "Unable to prepare upsert statement for {}",
                db.path.display()
            )
        })?;

    let mut removed_sorted: Vec<&String> = removed_paths.iter().collect();
    removed_sorted.sort();
    for file_path in removed_sorted {
        delete_stmt
            .execute(params![file_path])
            .with_context(|| format!("Unable to delete cache entry in {}", db.path.display()))?;
    }

    let mut dirty_sorted: Vec<&String> = dirty_paths.iter().collect();
    dirty_sorted.sort();
    for file_path in dirty_sorted {
        let Some(entry) = store.entries.get(file_path) else {
            continue;
        };
        let daily_json = serde_json::to_string(&entry.daily)
            .with_context(|| format!("Unable to serialize daily cache JSON for {}", file_path))?;
        let model_daily_json =
            serde_json::to_string(&entry.model_totals_by_day).with_context(|| {
                format!(
                    "Unable to serialize model-daily cache JSON for {}",
                    file_path
                )
            })?;
        let parser_state_json = serde_json::to_string(&entry.parser_state).with_context(|| {
            format!(
                "Unable to serialize parser-state cache JSON for {}",
                file_path
            )
        })?;
        let file_size = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let file_mtime = entry
            .modified_epoch_secs
            .and_then(|value| i64::try_from(value).ok());
        let file_offset = i64::try_from(entry.file_offset).unwrap_or(i64::MAX);
        let fully_parsed = if entry.fully_parsed { 1_i64 } else { 0_i64 };

        upsert_stmt
            .execute(params![
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                entry.session_cwd.as_deref(),
                parser_state_json,
                daily_json,
                model_daily_json,
                entry.updated_at
            ])
            .with_context(|| format!("Unable to upsert cache entry in {}", db.path.display()))?;
    }

    drop(delete_stmt);
    drop(upsert_stmt);
    tx.commit().with_context(|| {
        format!(
            "Unable to commit cache transaction for {}",
            db.path.display()
        )
    })?;
    enforce_scan_cache_db_permissions(&db.path)?;
    Ok(())
}

fn trim_scan_cache_db_entries_to_limit(db: &ScanCacheDb, max_entries: usize) -> Result<bool> {
    let max_entries = max_entries.max(1);
    let max_entries_i64 = i64::try_from(max_entries).unwrap_or(i64::MAX);
    let total_rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM file_cache;", [], |row| row.get(0))
        .with_context(|| format!("Unable to count cache rows in {}", db.path.display()))?;
    if total_rows <= max_entries_i64 {
        return Ok(false);
    }

    let remove_count = total_rows - max_entries_i64;
    db.conn
        .execute(
            "
            DELETE FROM file_cache
            WHERE file_path IN (
                SELECT file_path
                FROM file_cache
                ORDER BY updated_at ASC, file_path ASC
                LIMIT ?1
            );
            ",
            params![remove_count],
        )
        .with_context(|| format!("Unable to trim cache rows in {}", db.path.display()))?;
    enforce_scan_cache_db_permissions(&db.path)?;
    Ok(true)
}

#[cfg(test)]
fn is_valid_cached_session_file(
    path: &Path,
    sessions_root: &Path,
    max_session_file_bytes: u64,
) -> bool {
    if !path.starts_with(sessions_root) {
        return false;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return false;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return false;
    }
    let len = meta.len();
    len > 0 && len <= max_session_file_bytes
}

fn enforce_scan_cache_db_permissions(path: &Path) -> Result<()> {
    ensure_regular_file_or_missing(path, "cache database file")?;
    crate::storage::enforce_private_file_if_exists(path)?;
    let mut wal_path: OsString = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let mut shm_path: OsString = path.as_os_str().to_os_string();
    shm_path.push("-shm");
    ensure_regular_file_or_missing(Path::new(&wal_path), "cache database WAL file")?;
    ensure_regular_file_or_missing(Path::new(&shm_path), "cache database SHM file")?;
    let _ = crate::storage::enforce_private_file_if_exists(Path::new(&wal_path));
    let _ = crate::storage::enforce_private_file_if_exists(Path::new(&shm_path));
    Ok(())
}

fn ensure_directory_not_symlink(path: &Path, label: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Unable to inspect {} {}", label, path.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        anyhow::bail!(
            "Refusing to use {} {}: symlink is not allowed",
            label,
            path.display()
        );
    }
    if !ft.is_dir() {
        anyhow::bail!(
            "Refusing to use {} {}: expected directory",
            label,
            path.display()
        );
    }
    Ok(())
}

fn ensure_regular_file_or_missing(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                anyhow::bail!(
                    "Refusing to use {} {}: symlink is not allowed",
                    label,
                    path.display()
                );
            }
            if !ft.is_file() {
                anyhow::bail!(
                    "Refusing to use {} {}: expected regular file",
                    label,
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Unable to inspect {} {}", label, path.display()))
        }
    }
}

fn extract_model_from_turn_context(value: &Value) -> Option<String> {
    let payload = value.get("payload").and_then(|value| value.as_object())?;
    if let Some(model) = payload.get("model").and_then(|value| value.as_str()) {
        return Some(model.to_string());
    }
    let info = payload.get("info").and_then(|value| value.as_object())?;
    info.get("model")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn extract_model_from_token_count(value: &Value) -> Option<String> {
    let payload = value.get("payload").and_then(|value| value.as_object())?;
    let info = payload.get("info").and_then(|value| value.as_object());
    let model = info
        .and_then(|info| {
            info.get("model")
                .or_else(|| info.get("model_name"))
                .and_then(|value| value.as_str())
        })
        .or_else(|| payload.get("model").and_then(|value| value.as_str()))
        .or_else(|| value.get("model").and_then(|value| value.as_str()));
    model.map(|value| value.to_string())
}

fn find_usage_map<'a>(
    info: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    keys.iter()
        .find_map(|key| info.get(*key).and_then(|value| value.as_object()))
}

fn read_i64(map: &serde_json::Map<String, Value>, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|value| value as i64))
        })
        .unwrap_or(0)
}

fn read_timestamp_ms(value: &Value) -> Option<i64> {
    parse_timestamp_value_ms(value.get("timestamp")?)
}

fn parse_timestamp_value_ms(raw: &Value) -> Option<i64> {
    if let Some(text) = raw.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .map(|value| value.timestamp_millis())
            .ok();
    }
    let numeric = raw
        .as_i64()
        .or_else(|| raw.as_f64().map(|value| value as i64))?;
    if numeric > 0 && numeric < 1_000_000_000_000 {
        return Some(numeric * 1000);
    }
    Some(numeric)
}

fn maybe_start_fork_replay(
    value: &Value,
    first_session_meta_seen: &mut bool,
    replay: &mut ForkReplayState,
) {
    if *first_session_meta_seen {
        return;
    }
    *first_session_meta_seen = true;

    let payload = value.get("payload").and_then(Value::as_object);
    let Some(payload) = payload else {
        return;
    };
    if payload
        .get("forked_from_id")
        .and_then(Value::as_str)
        .is_none()
    {
        return;
    }

    let start_ms = payload
        .get("timestamp")
        .and_then(parse_timestamp_value_ms)
        .or_else(|| read_timestamp_ms(value));
    *replay = ForkReplayState {
        active: true,
        done: false,
        start_ms,
        last_event_ms: start_ms,
        token_events: 0,
    };
}

fn fork_replay_should_skip_event(
    replay: &mut ForkReplayState,
    event_timestamp_ms: Option<i64>,
) -> bool {
    if !replay.active || replay.done {
        return false;
    }

    let Some(timestamp_ms) = event_timestamp_ms else {
        return true;
    };
    let start_ms = replay.start_ms.unwrap_or(timestamp_ms);
    let elapsed_ms = timestamp_ms - start_ms;
    let gap_ms = replay
        .last_event_ms
        .map(|last_ms| timestamp_ms - last_ms)
        .unwrap_or(0);

    if gap_ms >= FORK_REPLAY_END_GAP_MS
        || (replay.token_events == 0 && elapsed_ms >= FORK_REPLAY_NO_TOKEN_GRACE_MS)
    {
        replay.active = false;
        replay.done = true;
        replay.last_event_ms = Some(timestamp_ms);
        return false;
    }

    replay.last_event_ms = Some(timestamp_ms);
    true
}

fn note_fork_replay_token(replay: &mut ForkReplayState) {
    if replay.active && !replay.done {
        replay.token_events = replay.token_events.saturating_add(1);
    }
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn track_activity(
    daily: &mut HashMap<String, DailyTotals>,
    last_activity_ms: &mut Option<i64>,
    timestamp_ms: i64,
) {
    if let Some(prev_ms) = *last_activity_ms {
        let delta = timestamp_ms - prev_ms;
        if delta > 0 && delta <= MAX_ACTIVITY_GAP_MS {
            if let Some(day_key) = day_key_for_timestamp_ms(timestamp_ms) {
                daily.entry(day_key).or_default().agent_ms += delta;
            }
        }
    }
    *last_activity_ms = Some(timestamp_ms);
}

fn day_key_for_timestamp_ms(timestamp_ms: i64) -> Option<String> {
    let utc = Utc.timestamp_millis_opt(timestamp_ms).single()?;
    Some(utc.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

fn epoch_secs_for_local_day_start(day_key: &str) -> Option<u64> {
    let day = chrono::NaiveDate::parse_from_str(day_key, "%Y-%m-%d").ok()?;
    let local = Local
        .with_ymd_and_hms(day.year(), day.month(), day.day(), 0, 0, 0)
        .single()?;
    u64::try_from(local.timestamp()).ok()
}

fn extract_cwd(value: &Value) -> Option<String> {
    value
        .get("payload")
        .and_then(|payload| payload.get("cwd"))
        .and_then(|cwd| cwd.as_str())
        .map(|cwd| cwd.to_string())
}

fn path_matches_workspace(cwd: &str, workspace_path: &Path) -> bool {
    // `cwd` comes from session logs and is untrusted: reject obviously malicious inputs.
    if cwd.is_empty() || cwd.len() > 4096 || cwd.chars().any(|c| c.is_control()) {
        return false;
    }
    let cwd_path = Path::new(cwd);
    // If we're filtering by an absolute workspace path, require absolute `cwd` too.
    if workspace_path.is_absolute() && !cwd_path.is_absolute() {
        return false;
    }
    cwd_path == workspace_path || cwd_path.starts_with(workspace_path)
}

fn normalize_project_key(path: &str) -> String {
    path.trim().to_lowercase()
}

fn make_day_keys(days: u32) -> Vec<String> {
    let today = Local::now().date_naive();
    (0..days)
        .rev()
        .map(|offset| {
            let day = today - Duration::days(offset as i64);
            day.format("%Y-%m-%d").to_string()
        })
        .collect()
}

pub fn system_first_weekday() -> Weekday {
    locale_region_from_env()
        .as_deref()
        .map(first_weekday_for_region)
        .unwrap_or(Weekday::Mon)
}

fn locale_region_from_env() -> Option<String> {
    for key in ["LC_TIME", "LC_ALL", "LANG"] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        return locale_region(&value);
    }
    None
}

fn locale_region(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("c")
        || trimmed.eq_ignore_ascii_case("posix")
    {
        return None;
    }
    let base = trimmed
        .split(['.', '@'])
        .next()
        .unwrap_or(trimmed)
        .replace('-', "_");
    let mut parts = base.split('_');
    let _language = parts.next()?;
    let region = parts.next()?.trim();
    if region.len() != 2 || !region.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    Some(region.to_ascii_uppercase())
}

fn first_weekday_for_region(region: &str) -> Weekday {
    match region.to_ascii_uppercase().as_str() {
        "AE" | "AF" | "BH" | "DJ" | "DZ" | "EG" | "IQ" | "IR" | "JO" | "KW" | "LY" | "OM"
        | "QA" | "SD" | "SY" | "YE" => Weekday::Sat,
        "AG" | "AR" | "AS" | "AU" | "BD" | "BR" | "BS" | "BT" | "BW" | "BZ" | "CA" | "CN"
        | "CO" | "DM" | "DO" | "ET" | "GT" | "GU" | "HK" | "HN" | "ID" | "IL" | "IN" | "JM"
        | "JP" | "KE" | "KH" | "KR" | "LA" | "MH" | "MM" | "MO" | "MT" | "MX" | "MZ" | "NI"
        | "NP" | "PA" | "PE" | "PH" | "PK" | "PR" | "PT" | "PY" | "SA" | "SG" | "SV" | "TH"
        | "TT" | "TW" | "UM" | "US" | "VE" | "VI" | "WS" | "ZA" | "ZW" => Weekday::Sun,
        _ => Weekday::Mon,
    }
}

fn make_activity_day_keys(first_weekday: Weekday) -> Vec<String> {
    let today = Local::now().date_naive();
    let days_from_week_start = days_since_week_start(today.weekday(), first_weekday);
    let current_week_start = today - Duration::days(days_from_week_start);
    let first_day = current_week_start - Duration::weeks((ACTIVITY_TIMELINE_WEEKS - 1) as i64);
    (0..ACTIVITY_TIMELINE_DAYS)
        .map(|offset| {
            let day = first_day + Duration::days(offset as i64);
            day.format("%Y-%m-%d").to_string()
        })
        .collect()
}

fn days_since_week_start(day: Weekday, first_weekday: Weekday) -> i64 {
    let day = day.num_days_from_monday() as i64;
    let first = first_weekday.num_days_from_monday() as i64;
    (7 + day - first) % 7
}

pub fn format_count(value: i64) -> String {
    let mut n = value.max(0) as u64;
    if n < 1000 {
        return n.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    while n >= 1000 {
        parts.push(format!("{:03}", n % 1000));
        n /= 1000;
    }
    parts.push(n.to_string());
    parts.reverse();
    parts.join(",")
}

pub fn format_tokens_compact(value: i64) -> String {
    let v = value.max(0) as u64;
    if v < 1000 {
        return v.to_string();
    }

    let (div, suffix) = if v >= 1_000_000_000_000 {
        (1_000_000_000_000f64, "T")
    } else if v >= 1_000_000_000 {
        (1_000_000_000f64, "B")
    } else if v >= 1_000_000 {
        (1_000_000f64, "M")
    } else {
        (1_000f64, "K")
    };
    let scaled = (v as f64) / div;
    format_compact_scaled(scaled, suffix, 2)
}

pub fn format_duration_compact(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

pub fn format_duration(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    secs %= 60;
    if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("comon-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        let line = serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input_tokens,
                        "cached_input_tokens": 0,
                        "output_tokens": output_tokens
                    },
                    "model": "gpt-test"
                }
            }
        });
        std::fs::write(path, format!("{line}\n")).expect("write token file");
    }

    fn append_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        let line = serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input_tokens,
                        "cached_input_tokens": 0,
                        "output_tokens": output_tokens
                    },
                    "model": "gpt-test"
                }
            }
        });
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open token file for append");
        use std::io::Write as _;
        writeln!(file, "{line}").expect("append token line");
    }

    fn append_json_line(path: &Path, line: serde_json::Value) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open jsonl file for append");
        use std::io::Write as _;
        writeln!(file, "{line}").expect("append json line");
    }

    fn append_total_token_line(
        path: &Path,
        timestamp_ms: i64,
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
    ) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": input_tokens,
                            "cached_input_tokens": cached_input_tokens,
                            "output_tokens": output_tokens
                        },
                        "model": "gpt-test"
                    }
                }
            }),
        );
    }

    fn append_agent_message_line(path: &Path, timestamp_ms: i64) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {
                    "type": "agent_message",
                    "message": "ok"
                }
            }),
        );
    }

    fn append_session_meta_line(path: &Path, timestamp_ms: i64, cwd: &str) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": timestamp,
                "payload": {
                    "id": cwd,
                    "timestamp": timestamp,
                    "cwd": cwd
                }
            }),
        );
    }

    fn write_forked_replay_file(path: &Path, timestamp_ms: i64) {
        let fork_timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": fork_timestamp,
                "payload": {
                    "id": "fork-child",
                    "forked_from_id": "parent",
                    "timestamp": fork_timestamp,
                    "cwd": "/tmp/forked-project"
                }
            }),
        );
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": fork_timestamp,
                "payload": {
                    "id": "parent",
                    "timestamp": fork_timestamp,
                    "cwd": "/tmp/forked-project"
                }
            }),
        );

        append_total_token_line(path, timestamp_ms + 100, 1_000, 800, 100);
        append_agent_message_line(path, timestamp_ms + 200);
        append_total_token_line(path, timestamp_ms + 400, 1_500, 1_300, 130);
        append_total_token_line(path, timestamp_ms + 1_700, 1_700, 1_400, 150);
        append_agent_message_line(path, timestamp_ms + 1_800);
    }

    fn default_test_limits(full_scan: bool) -> ScanLimits {
        ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan,
            scan_cache_max_entries: 1000,
        }
    }

    #[test]
    fn locale_region_parses_common_locale_values() {
        assert_eq!(locale_region("ja_JP.UTF-8").as_deref(), Some("JP"));
        assert_eq!(locale_region("en-US").as_deref(), Some("US"));
        assert_eq!(locale_region("C"), None);
    }

    #[test]
    fn first_weekday_uses_region_defaults() {
        assert_eq!(first_weekday_for_region("JP"), Weekday::Sun);
        assert_eq!(first_weekday_for_region("US"), Weekday::Sun);
        assert_eq!(first_weekday_for_region("GB"), Weekday::Mon);
    }

    #[test]
    fn activity_day_keys_start_on_requested_weekday() {
        let sunday_keys = make_activity_day_keys(Weekday::Sun);
        let monday_keys = make_activity_day_keys(Weekday::Mon);
        let sunday = chrono::NaiveDate::parse_from_str(&sunday_keys[0], "%Y-%m-%d")
            .expect("parse sunday-first activity key");
        let monday = chrono::NaiveDate::parse_from_str(&monday_keys[0], "%Y-%m-%d")
            .expect("parse monday-first activity key");
        assert_eq!(sunday.weekday(), Weekday::Sun);
        assert_eq!(monday.weekday(), Weekday::Mon);
    }

    #[test]
    fn compute_snapshot_ignores_forked_session_replay_without_cache() {
        let root = make_temp_dir("fork-replay-no-cache");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("forked.jsonl");
        write_forked_replay_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
        );

        let snapshot = compute_snapshot(30, &codex_home, None, default_test_limits(false), None)
            .expect("snapshot");
        assert_eq!(
            snapshot.totals.last30_days_tokens, 220,
            "only token deltas after the fork replay should count"
        );
        assert_eq!(
            snapshot.days.iter().map(|day| day.agent_runs).sum::<i64>(),
            1
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_ignores_forked_session_replay_with_cache() {
        let root = make_temp_dir("fork-replay-cache");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("forked.jsonl");
        write_forked_replay_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
        );

        let cache_db_path = root.join("comon.db");
        let first = compute_snapshot(
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 220);

        let cached = compute_snapshot(
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("cached snapshot");
        assert_eq!(cached.totals.last30_days_tokens, 220);
        assert_eq!(cached.days.iter().map(|day| day.agent_runs).sum::<i64>(), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_builds_project_activity_sorted_by_last_activity() {
        let root = make_temp_dir("project-activity");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let older_ms = now_ms - Duration::days(5).num_milliseconds();
        let newer_ms = now_ms - Duration::days(1).num_milliseconds();

        let photonia = sessions_root.join("photonia.jsonl");
        append_session_meta_line(&photonia, older_ms, "/tmp/Photonia");
        append_total_token_line(&photonia, older_ms + 100, 100, 0, 20);

        let sfm = sessions_root.join("sfm.jsonl");
        append_session_meta_line(&sfm, newer_ms, "/tmp/SFM");
        append_total_token_line(&sfm, newer_ms + 100, 200, 150, 50);

        let snapshot = compute_snapshot(30, &codex_home, None, default_test_limits(false), None)
            .expect("snapshot");

        assert_eq!(snapshot.project_activity.len(), 2);
        assert_eq!(
            snapshot.project_activity[0].days.len(),
            ACTIVITY_TIMELINE_DAYS
        );
        assert_eq!(snapshot.project_activity[0].display_path, "/tmp/SFM");
        assert_eq!(snapshot.project_activity[0].total_tokens, 250);
        assert_eq!(snapshot.project_activity[0].cached_input_tokens, 150);
        assert_eq!(snapshot.project_activity[1].display_path, "/tmp/Photonia");
        assert_eq!(snapshot.project_activity[1].total_tokens, 120);
        assert_eq!(snapshot.project_activity[1].cached_input_tokens, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_cached_totals_for_unplanned_unchanged_files() {
        let root = make_temp_dir("cache-unplanned");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let older_ms = now_ms - Duration::days(20).num_milliseconds();
        let newer_ms = now_ms - Duration::days(5).num_milliseconds();

        let older_path = sessions_root.join("older.jsonl");
        let newer_path = sessions_root.join("newer.jsonl");
        write_token_file(&older_path, older_ms, 100, 25);
        write_token_file(&newer_path, newer_ms, 80, 20);

        let cache_db_path = root.join("comon.db");
        let warm_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: true,
            scan_cache_max_entries: 1000,
        };
        let warmed = compute_snapshot(
            30,
            &codex_home,
            None,
            warm_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("warm snapshot");
        assert_eq!(warmed.totals.last30_days_tokens, 225);

        let restrictive_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 1,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let restricted = compute_snapshot(
            30,
            &codex_home,
            None,
            restrictive_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("restricted snapshot");
        assert_eq!(
            restricted.totals.last30_days_tokens, warmed.totals.last30_days_tokens,
            "unchanged files outside current scan plan should still contribute via cache"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_resumes_from_cached_offset_after_append() {
        let root = make_temp_dir("cache-append-resume");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("session.jsonl");
        write_token_file(
            &session_path,
            now_ms - Duration::hours(2).num_milliseconds(),
            100,
            20,
        );

        let cache_db_path = root.join("comon.db");
        let limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 4 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };

        let first = compute_snapshot(30, &codex_home, None, limits, Some(cache_db_path.as_path()))
            .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 120);

        append_token_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
            40,
            10,
        );

        let second = compute_snapshot(30, &codex_home, None, limits, Some(cache_db_path.as_path()))
            .expect("second snapshot");
        assert_eq!(second.totals.last30_days_tokens, 170);

        let third = compute_snapshot(30, &codex_home, None, limits, Some(cache_db_path.as_path()))
            .expect("third snapshot");
        assert_eq!(
            third.totals.last30_days_tokens, 170,
            "unchanged file should not double-count appended usage after resume"
        );

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let (store, _) = load_scan_cache_store(&db).expect("load cache store");
        let key = session_path.to_string_lossy().to_string();
        let entry = store
            .entries
            .get(&key)
            .expect("cache row for appended session");
        assert!(entry.file_offset > 0);
        assert!(entry.fully_parsed);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_full_scan_ignores_file_and_byte_scan_caps() {
        let root = make_temp_dir("full-scan-caps");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let files = [
            (
                "a.jsonl",
                now_ms - Duration::days(3).num_milliseconds(),
                90,
                10,
            ),
            (
                "b.jsonl",
                now_ms - Duration::days(2).num_milliseconds(),
                70,
                5,
            ),
            (
                "c.jsonl",
                now_ms - Duration::days(1).num_milliseconds(),
                40,
                15,
            ),
        ];
        let mut expected_total = 0_i64;
        for (name, ts, input, output) in files {
            write_token_file(&sessions_root.join(name), ts, input, output);
            expected_total += input + output;
        }

        let capped_limits = ScanLimits {
            max_session_file_bytes: 1,
            max_session_total_bytes: 1,
            max_session_files_scanned: 1,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let capped =
            compute_snapshot(30, &codex_home, None, capped_limits, None).expect("capped snapshot");
        assert!(
            capped.totals.last30_days_tokens < expected_total,
            "planner caps should leave some files unscanned in non-full mode"
        );

        let uncapped_limits = ScanLimits {
            full_scan: true,
            ..capped_limits
        };
        let full =
            compute_snapshot(30, &codex_home, None, uncapped_limits, None).expect("full snapshot");
        assert_eq!(
            full.totals.last30_days_tokens, expected_total,
            "full scan should include all files even when scan caps are tiny"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_full_scan_reparses_when_cache_row_is_stale() {
        let root = make_temp_dir("full-scan-reparse");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("session.jsonl");
        write_token_file(
            &session_path,
            now_ms - Duration::hours(6).num_milliseconds(),
            120,
            30,
        );
        let expected_total = 150_i64;

        let cache_db_path = root.join("comon.db");
        let baseline_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 4 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let baseline = compute_snapshot(
            30,
            &codex_home,
            None,
            baseline_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("baseline snapshot");
        assert_eq!(baseline.totals.last30_days_tokens, expected_total);

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let file_key = session_path.to_string_lossy().to_string();
        db.conn
            .execute(
                "UPDATE file_cache SET daily_json='{}', model_daily_json='{}' WHERE file_path = ?1;",
                rusqlite::params![file_key],
            )
            .expect("corrupt cache row");
        drop(db);

        let stale = compute_snapshot(
            30,
            &codex_home,
            None,
            baseline_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("stale snapshot");
        assert_eq!(
            stale.totals.last30_days_tokens, 0,
            "non-full scan should still trust unchanged cache rows"
        );

        let full_limits = ScanLimits {
            full_scan: true,
            ..baseline_limits
        };
        let repaired = compute_snapshot(
            30,
            &codex_home,
            None,
            full_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("repaired snapshot");
        assert_eq!(
            repaired.totals.last30_days_tokens, expected_total,
            "full scan with unlimited time should reparse files and repair stale cache rows"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prune_scan_cache_db_removes_stale_entries() {
        let root = make_temp_dir("cache-prune");
        let sessions_root = root.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let db_path = root.join("comon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");

        let keep_path = sessions_root.join("keep.jsonl");
        std::fs::write(&keep_path, b"{}\n").expect("write keep jsonl");

        let wrong_ext_path = sessions_root.join("not_jsonl.txt");
        std::fs::write(&wrong_ext_path, b"ignored").expect("write txt");

        let outside_root = make_temp_dir("outside");
        let outside_path = outside_root.join("outside.jsonl");
        std::fs::write(&outside_path, b"{}\n").expect("write outside jsonl");

        let missing_path = sessions_root.join("missing.jsonl");

        let keep_key = keep_path.to_string_lossy().to_string();
        let wrong_ext_key = wrong_ext_path.to_string_lossy().to_string();
        let outside_key = outside_path.to_string_lossy().to_string();
        let missing_key = missing_path.to_string_lossy().to_string();

        let base = CachedFileScanEntry {
            size: 4,
            modified_epoch_secs: Some(1),
            file_offset: 4,
            fully_parsed: true,
            session_cwd: None,
            parser_state: ParserState::default(),
            daily: HashMap::new(),
            model_totals_by_day: HashMap::new(),
            updated_at: 1,
        };
        let mut initial_store = ScanCacheStore::default();
        initial_store.entries.insert(
            keep_key.clone(),
            CachedFileScanEntry {
                updated_at: 5,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            wrong_ext_key.clone(),
            CachedFileScanEntry {
                updated_at: 4,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            outside_key.clone(),
            CachedFileScanEntry {
                updated_at: 3,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            missing_key.clone(),
            CachedFileScanEntry {
                updated_at: 2,
                ..base
            },
        );
        let dirty_paths: HashSet<String> = initial_store.entries.keys().cloned().collect();
        persist_scan_cache_changes(&mut db, &initial_store, &HashSet::new(), &dirty_paths)
            .expect("persist initial rows");

        let (mut loaded_store, mut removed_paths) = load_scan_cache_store(&db).expect("load store");
        let pruned = prune_scan_cache_store(
            &mut loaded_store,
            &sessions_root,
            1024,
            100,
            &mut removed_paths,
        );
        assert!(pruned, "expected stale rows to be removed");
        persist_scan_cache_changes(&mut db, &loaded_store, &removed_paths, &HashSet::new())
            .expect("persist pruned rows");

        let (reloaded_store, _) = load_scan_cache_store(&db).expect("reload store");
        assert_eq!(reloaded_store.entries.len(), 1);
        assert!(reloaded_store.entries.contains_key(&keep_key));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[test]
    fn trim_scan_cache_db_to_limit_drops_oldest_entries() {
        let root = make_temp_dir("cache-trim");
        let db_path = root.join("comon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");

        let mut store = ScanCacheStore::default();
        for (path, updated_at) in [("a", 30_i64), ("b", 10_i64), ("c", 20_i64)] {
            store.entries.insert(
                path.to_string(),
                CachedFileScanEntry {
                    size: 1,
                    modified_epoch_secs: Some(1),
                    file_offset: 1,
                    fully_parsed: true,
                    session_cwd: None,
                    parser_state: ParserState::default(),
                    daily: HashMap::new(),
                    model_totals_by_day: HashMap::new(),
                    updated_at,
                },
            );
        }
        let dirty_paths: HashSet<String> = store.entries.keys().cloned().collect();
        persist_scan_cache_changes(&mut db, &store, &HashSet::new(), &dirty_paths)
            .expect("persist initial rows");

        let (mut loaded_store, mut removed_paths) = load_scan_cache_store(&db).expect("load store");
        let pruned = trim_scan_cache_to_limit(&mut loaded_store, 2, &mut removed_paths);
        assert!(pruned, "expected trim to remove one row");
        persist_scan_cache_changes(&mut db, &loaded_store, &removed_paths, &HashSet::new())
            .expect("persist trimmed rows");

        let (reloaded_store, _) = load_scan_cache_store(&db).expect("reload store");
        let mut paths: Vec<String> = reloaded_store.entries.keys().cloned().collect();
        paths.sort();
        assert_eq!(paths, vec!["a".to_string(), "c".to_string()]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_or_init_scan_cache_db_v3_migration_preserves_nonforked_rows() {
        let root = make_temp_dir("cache-migrate-v3");
        let sessions_root = root.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let keep_path = sessions_root.join("keep.jsonl");
        let forked_path = sessions_root.join("forked.jsonl");
        let now_ms = Utc::now().timestamp_millis();
        write_token_file(&keep_path, now_ms, 10, 5);
        write_forked_replay_file(&forked_path, now_ms);

        let db_path = root.join("comon.db");
        let conn = Connection::open(&db_path).expect("open v2 db");
        conn.execute_batch(
            "
            CREATE TABLE cache_meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO cache_meta(key, value) VALUES('schema_version', 2);
            CREATE TABLE file_cache (
                file_path TEXT PRIMARY KEY,
                file_size INTEGER NOT NULL,
                file_mtime INTEGER,
                file_offset INTEGER NOT NULL DEFAULT 0,
                fully_parsed INTEGER NOT NULL DEFAULT 1,
                session_cwd TEXT,
                parser_state_json TEXT NOT NULL DEFAULT '{}',
                daily_json TEXT NOT NULL,
                model_daily_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            ",
        )
        .expect("create v2 schema");
        for path in [&keep_path, &forked_path] {
            let key = path.to_string_lossy().to_string();
            conn.execute(
                "
                INSERT INTO file_cache(
                    file_path, file_size, file_mtime, file_offset, fully_parsed,
                    session_cwd, parser_state_json, daily_json, model_daily_json, updated_at
                ) VALUES(?1, 1, 1, 1, 1, '/tmp', '{}', '{}', '{}', 1);
                ",
                rusqlite::params![key],
            )
            .expect("insert cache row");
        }
        drop(conn);

        let db = open_or_init_scan_cache_db(&db_path).expect("open and migrate");
        let (store, _) = load_scan_cache_store(&db).expect("load migrated cache");
        let keep_key = keep_path.to_string_lossy().to_string();
        let forked_key = forked_path.to_string_lossy().to_string();
        assert!(store.entries.contains_key(&keep_key));
        assert!(!store.entries.contains_key(&forked_key));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_or_init_scan_cache_db_migrates_v1_schema_and_clears_stale_rows() {
        let root = make_temp_dir("cache-migrate");
        let db_path = root.join("comon.db");
        let conn = Connection::open(&db_path).expect("open legacy db");
        conn.execute_batch(
            "
            CREATE TABLE cache_meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO cache_meta(key, value) VALUES('schema_version', 1);
            CREATE TABLE file_cache (
                file_path TEXT PRIMARY KEY,
                file_size INTEGER NOT NULL,
                file_mtime INTEGER,
                session_cwd TEXT,
                daily_json TEXT NOT NULL,
                model_daily_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO file_cache(
                file_path, file_size, file_mtime, session_cwd, daily_json, model_daily_json, updated_at
            ) VALUES(
                'legacy.jsonl', 42, 123, '/tmp', '{}', '{}', 1
            );
            ",
        )
        .expect("create legacy schema");
        drop(conn);

        let db = open_or_init_scan_cache_db(&db_path).expect("open and migrate");
        let schema_version: i64 = db
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'schema_version';",
                [],
                |row| row.get(0),
            )
            .expect("read schema version");
        assert_eq!(schema_version, SCAN_CACHE_DB_SCHEMA_VERSION);
        assert!(table_has_column(&db.conn, "file_cache", "file_offset").expect("file_offset"));
        assert!(table_has_column(&db.conn, "file_cache", "fully_parsed").expect("fully_parsed"));
        assert!(
            table_has_column(&db.conn, "file_cache", "parser_state_json")
                .expect("parser_state_json")
        );

        let (store, _) = load_scan_cache_store(&db).expect("load migrated cache");
        assert!(
            store.entries.is_empty(),
            "v3 migration should clear rows computed with older parser semantics"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
