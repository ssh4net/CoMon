use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime};

const MAX_ACTIVITY_GAP_MS: i64 = 2 * 60 * 1000;
const DEFAULT_MAX_SESSION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_FILES_SCANNED: usize = 10_000;
const MAX_DISTINCT_MODELS: usize = 5_000;
const SCAN_CACHE_DB_SCHEMA_VERSION: i64 = 1;
pub const DEFAULT_SCAN_CACHE_MAX_ENTRIES: usize = 50_000;

#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    pub max_session_file_bytes: u64,
    pub max_session_total_bytes: u64,
    pub max_session_files_scanned: usize,
    pub full_scan: bool,
    pub scan_cache_max_entries: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_session_file_bytes: DEFAULT_MAX_SESSION_FILE_BYTES,
            max_session_total_bytes: DEFAULT_MAX_SESSION_TOTAL_BYTES,
            max_session_files_scanned: DEFAULT_MAX_SESSION_FILES_SCANNED,
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
pub struct LocalUsageSnapshot {
    pub days: Vec<UsageDay>,
    pub totals: UsageTotalsTokens,
    pub top_models: Vec<LocalUsageModel>,
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
                    last7_primary_label: format!("{}", format_duration_compact(last7_agent_ms)),
                    last30_primary_label: format!("{}", format_duration_compact(last30_agent_ms)),
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

#[derive(Default, Clone, Copy)]
struct UsageTotals {
    input: i64,
    cached: i64,
    output: i64,
}

#[derive(Debug, Clone, Default)]
struct ScanCacheStore {
    entries: HashMap<String, CachedFileScanEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CachedFileScanEntry {
    size: u64,
    modified_epoch_secs: Option<u64>,
    session_cwd: Option<String>,
    #[serde(default)]
    daily: HashMap<String, DailyTotals>,
    #[serde(default)]
    model_totals_by_day: HashMap<String, HashMap<String, i64>>,
    updated_at: i64,
}

#[derive(Debug, Clone, Default)]
struct FileScanSummary {
    session_cwd: Option<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals_by_day: HashMap<String, HashMap<String, i64>>,
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
    let day_keys = make_day_keys(days);
    let mut daily: HashMap<String, DailyTotals> = day_keys
        .iter()
        .map(|key| (key.clone(), DailyTotals::default()))
        .collect();
    let mut model_totals: HashMap<String, i64> = HashMap::new();

    if !sessions_root.exists() {
        return Ok(build_snapshot(day_keys, daily, model_totals, 0));
    }

    // Prefer scanning by file mtime instead of directory date: long-running sessions can live in an
    // older day folder but still accrue usage today.
    let cutoff = if limits.full_scan {
        None
    } else {
        SystemTime::now().checked_sub(StdDuration::from_secs(
            (days as u64).saturating_mul(24 * 60 * 60),
        ))
    };
    let mut candidates =
        collect_session_file_candidates(&sessions_root, cutoff, limits.max_session_file_bytes);
    // Newest activity first.
    candidates.sort_by(|a, b| {
        b.modified_epoch_secs
            .cmp(&a.modified_epoch_secs)
            .then_with(|| a.len.cmp(&b.len))
    });

    let mut planned_indices: Vec<usize> = Vec::new();
    let mut planned_total_bytes: u64 = 0;
    let mut planned_files: usize = 0;
    for (idx, candidate) in candidates.iter().enumerate() {
        if planned_files >= limits.max_session_files_scanned
            || planned_total_bytes >= limits.max_session_total_bytes
        {
            break;
        }
        if planned_total_bytes.saturating_add(candidate.len) > limits.max_session_total_bytes {
            continue;
        }
        planned_indices.push(idx);
        planned_files += 1;
        planned_total_bytes = planned_total_bytes.saturating_add(candidate.len);
    }

    let mut matched_session_files: u32 = 0;
    let mut scan_cache_db = scan_cache_db_path
        .map(open_or_init_scan_cache_db)
        .transpose()?;
    let cache_candidate_paths: Vec<String> = planned_indices
        .iter()
        .map(|idx| candidates[*idx].path.to_string_lossy().to_string())
        .collect();
    let (mut scan_cache_store, removed_cache_paths) = if let Some(db) = scan_cache_db.as_ref() {
        load_scan_cache_store_for_candidates(db, &cache_candidate_paths)?
    } else {
        (ScanCacheStore::default(), HashSet::new())
    };
    let mut dirty_cache_paths: HashSet<String> = HashSet::new();

    for idx in planned_indices {
        let candidate = &candidates[idx];
        if scan_cache_db.is_some() {
            let candidate_key = candidate.path.to_string_lossy().to_string();
            let cached_entry = scan_cache_store
                .entries
                .get(&candidate_key)
                .filter(|entry| {
                    entry.size == candidate.len
                        && entry.modified_epoch_secs == candidate.modified_epoch_secs
                })
                .cloned();
            let entry = if let Some(entry) = cached_entry {
                entry
            } else {
                let parsed = parse_file_summary(&candidate.path, limits.max_session_file_bytes)?;
                let entry = CachedFileScanEntry {
                    size: candidate.len,
                    modified_epoch_secs: candidate.modified_epoch_secs,
                    session_cwd: parsed.session_cwd,
                    daily: parsed.daily,
                    model_totals_by_day: parsed.model_totals_by_day,
                    updated_at: unix_time_seconds(),
                };
                scan_cache_store
                    .entries
                    .insert(candidate_key.clone(), entry.clone());
                dirty_cache_paths.insert(candidate_key);
                entry
            };
            apply_cached_file_entry(
                &entry,
                workspace_path,
                &mut daily,
                &mut model_totals,
                &mut matched_session_files,
            );
        } else {
            scan_file(
                &candidate.path,
                &mut daily,
                &mut model_totals,
                workspace_path,
                limits.max_session_file_bytes,
                &mut matched_session_files,
            )?;
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
        day_keys,
        daily,
        model_totals,
        matched_session_files,
    ))
}

#[derive(Debug, Clone)]
struct SessionFileCandidate {
    path: PathBuf,
    len: u64,
    modified_epoch_secs: Option<u64>,
}

fn collect_session_file_candidates(
    sessions_root: &Path,
    cutoff: Option<SystemTime>,
    max_session_file_bytes: u64,
) -> Vec<SessionFileCandidate> {
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
            if len == 0 || len > max_session_file_bytes {
                continue;
            }

            let modified = meta.modified().ok();
            if let (Some(cutoff), Some(modified)) = (cutoff, modified) {
                if modified < cutoff {
                    continue;
                }
            }

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

fn build_snapshot(
    day_keys: Vec<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals: HashMap<String, i64>,
    matched_session_files: u32,
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
        matched_session_files,
    }
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
        if !daily.contains_key(day_key) {
            continue;
        }
        for (model, tokens) in per_day_models {
            add_model_tokens_limited(model_totals, model.clone(), *tokens);
        }
    }
}

fn parse_file_summary(path: &Path, max_session_file_bytes: u64) -> Result<FileScanSummary> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(FileScanSummary::default()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(FileScanSummary::default());
    }
    if meta.len() == 0 || meta.len() > max_session_file_bytes {
        return Ok(FileScanSummary::default());
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(FileScanSummary::default()),
    };

    let mut daily: HashMap<String, DailyTotals> = HashMap::new();
    let mut model_totals_by_day: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut session_cwd: Option<String> = None;
    let reader = BufReader::new(file);
    let mut previous_totals: Option<UsageTotals> = None;
    let mut current_model: Option<String> = None;
    let mut last_activity_ms: Option<i64> = None;
    let mut seen_runs: HashSet<i64> = HashSet::new();

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        if line.len() > 512_000 {
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

        if entry_type == "turn_context" {
            if let Some(model) = extract_model_from_turn_context(&value) {
                current_model = Some(model);
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

            if payload_type == Some("agent_message") {
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
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
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
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

            if delta.input == 0 && delta.cached == 0 && delta.output == 0 {
                continue;
            }

            let timestamp_ms = read_timestamp_ms(&value);
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
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
                    if seen_runs.insert(timestamp_ms) {
                        if let Some(day_key) = day_key_for_timestamp_ms(timestamp_ms) {
                            daily.entry(day_key).or_default().agent_runs += 1;
                        }
                    }
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
            } else if payload_type != Some("message") {
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
            }
        }
    }

    Ok(FileScanSummary {
        session_cwd,
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
            session_cwd TEXT,
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
    if schema_version != SCAN_CACHE_DB_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported scan cache schema version: {} (expected {})",
            schema_version,
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

#[cfg(test)]
fn load_scan_cache_store(db: &ScanCacheDb) -> Result<(ScanCacheStore, HashSet<String>)> {
    let mut store = ScanCacheStore::default();
    let mut invalid_paths: HashSet<String> = HashSet::new();
    let mut stmt = db
        .conn
        .prepare(
            "
            SELECT file_path, file_size, file_mtime, session_cwd, daily_json, model_daily_json, updated_at
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
        let session_cwd: Option<String> = row.get(3).with_context(|| {
            format!(
                "Unable to read session_cwd from cache row in {}",
                db.path.display()
            )
        })?;
        let daily_json: String = row.get(4).with_context(|| {
            format!(
                "Unable to read daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let model_daily_json: String = row.get(5).with_context(|| {
            format!(
                "Unable to read model_daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let updated_at: i64 = row.get(6).with_context(|| {
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
                session_cwd,
                daily,
                model_totals_by_day,
                updated_at,
            },
        );
    }

    Ok((store, invalid_paths))
}

fn load_scan_cache_store_for_candidates(
    db: &ScanCacheDb,
    candidate_paths: &[String],
) -> Result<(ScanCacheStore, HashSet<String>)> {
    let mut store = ScanCacheStore::default();
    let mut invalid_paths: HashSet<String> = HashSet::new();
    if candidate_paths.is_empty() {
        return Ok((store, invalid_paths));
    }

    let mut seen: HashSet<&str> = HashSet::new();
    let mut stmt = db
        .conn
        .prepare(
            "
            SELECT file_size, file_mtime, session_cwd, daily_json, model_daily_json, updated_at
            FROM file_cache
            WHERE file_path = ?1;
            ",
        )
        .with_context(|| {
            format!(
                "Unable to prepare candidate cache query for {}",
                db.path.display()
            )
        })?;

    for file_path in candidate_paths {
        if !seen.insert(file_path.as_str()) {
            continue;
        }

        let row: Option<(i64, Option<i64>, Option<String>, String, String, i64)> = stmt
            .query_row(params![file_path], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .optional()
            .with_context(|| {
                format!(
                    "Unable to load cache row for candidate {} in {}",
                    file_path,
                    db.path.display()
                )
            })?;
        let Some((
            file_size_raw,
            file_mtime_raw,
            session_cwd,
            daily_json,
            model_daily_json,
            updated_at,
        )) = row
        else {
            continue;
        };

        let Ok(file_size) = u64::try_from(file_size_raw.max(0)) else {
            invalid_paths.insert(file_path.clone());
            continue;
        };
        let file_mtime = file_mtime_raw.and_then(|value| u64::try_from(value).ok());
        let daily = match serde_json::from_str::<HashMap<String, DailyTotals>>(&daily_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path.clone());
                continue;
            }
        };
        let model_totals_by_day = match serde_json::from_str::<HashMap<String, HashMap<String, i64>>>(
            &model_daily_json,
        ) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path.clone());
                continue;
            }
        };

        store.entries.insert(
            file_path.clone(),
            CachedFileScanEntry {
                size: file_size,
                modified_epoch_secs: file_mtime,
                session_cwd,
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
                file_path, file_size, file_mtime, session_cwd, daily_json, model_daily_json, updated_at
            )
            VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(file_path) DO UPDATE SET
                file_size=excluded.file_size,
                file_mtime=excluded.file_mtime,
                session_cwd=excluded.session_cwd,
                daily_json=excluded.daily_json,
                model_daily_json=excluded.model_daily_json,
                updated_at=excluded.updated_at;
            ",
        )
        .with_context(|| format!("Unable to prepare upsert statement for {}", db.path.display()))?;

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
        let file_size = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let file_mtime = entry
            .modified_epoch_secs
            .and_then(|value| i64::try_from(value).ok());

        upsert_stmt
            .execute(params![
                file_path,
                file_size,
                file_mtime,
                entry.session_cwd.as_deref(),
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

fn scan_file(
    path: &Path,
    daily: &mut HashMap<String, DailyTotals>,
    model_totals: &mut HashMap<String, i64>,
    workspace_path: Option<&Path>,
    max_session_file_bytes: u64,
    matched_session_files: &mut u32,
) -> Result<()> {
    // Defensive: do not follow symlinks / special files even if called directly.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(());
    }
    if meta.len() == 0 || meta.len() > max_session_file_bytes {
        return Ok(());
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    let reader = BufReader::new(file);
    let mut previous_totals: Option<UsageTotals> = None;
    let mut current_model: Option<String> = None;
    let mut last_activity_ms: Option<i64> = None;
    let mut seen_runs: HashSet<i64> = HashSet::new();
    let mut match_known = workspace_path.is_none();
    let mut matches_workspace = workspace_path.is_none();
    let mut counted_match = false;

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        if line.len() > 512_000 {
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

        if entry_type == "session_meta" || entry_type == "turn_context" {
            if let Some(cwd) = extract_cwd(&value) {
                if let Some(filter) = workspace_path {
                    matches_workspace = path_matches_workspace(&cwd, filter);
                    match_known = true;
                    if matches_workspace && !counted_match {
                        *matched_session_files = matched_session_files.saturating_add(1);
                        counted_match = true;
                    }
                    if !matches_workspace {
                        break;
                    }
                }
            }
        }

        if entry_type == "turn_context" {
            if let Some(model) = extract_model_from_turn_context(&value) {
                current_model = Some(model);
            }
            continue;
        }

        if entry_type == "session_meta" {
            continue;
        }

        if !matches_workspace {
            if match_known {
                break;
            }
            continue;
        }

        if !match_known {
            continue;
        }

        if entry_type == "event_msg" || entry_type.is_empty() {
            let payload = value.get("payload").and_then(|value| value.as_object());
            let payload_type = payload
                .and_then(|payload| payload.get("type"))
                .and_then(|value| value.as_str());

            if payload_type == Some("agent_message") {
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
                    if seen_runs.insert(timestamp_ms) {
                        if let Some(day_key) = day_key_for_timestamp_ms(timestamp_ms) {
                            if let Some(entry) = daily.get_mut(&day_key) {
                                entry.agent_runs += 1;
                            }
                        }
                    }
                    track_activity(daily, &mut last_activity_ms, timestamp_ms);
                }
                continue;
            }

            if payload_type == Some("agent_reasoning") {
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
                    track_activity(daily, &mut last_activity_ms, timestamp_ms);
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
                // Some streams emit `last_token_usage` deltas between `total_token_usage` snapshots.
                // Treat those as already-counted to avoid double-counting when the next total arrives.
                let mut next = previous_totals.unwrap_or_default();
                next.input += delta.input;
                next.cached += delta.cached;
                next.output += delta.output;
                previous_totals = Some(next);
            }

            if delta.input == 0 && delta.cached == 0 && delta.output == 0 {
                continue;
            }

            let timestamp_ms = read_timestamp_ms(&value);
            if let Some(day_key) = timestamp_ms.and_then(day_key_for_timestamp_ms) {
                if let Some(entry) = daily.get_mut(&day_key) {
                    let cached_clamped = delta.cached.min(delta.input);
                    entry.input += delta.input;
                    entry.cached += cached_clamped;
                    entry.output += delta.output;

                    let model = current_model
                        .clone()
                        .or_else(|| extract_model_from_token_count(&value))
                        .unwrap_or_else(|| "unknown".to_string());
                    add_model_tokens_limited(model_totals, model, delta.input + delta.output);
                }
            }

            if let Some(timestamp_ms) = timestamp_ms {
                track_activity(daily, &mut last_activity_ms, timestamp_ms);
            }
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
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
                    if seen_runs.insert(timestamp_ms) {
                        if let Some(day_key) = day_key_for_timestamp_ms(timestamp_ms) {
                            if let Some(entry) = daily.get_mut(&day_key) {
                                entry.agent_runs += 1;
                            }
                        }
                    }
                    track_activity(daily, &mut last_activity_ms, timestamp_ms);
                }
            } else if payload_type != Some("message") {
                if let Some(timestamp_ms) = read_timestamp_ms(&value) {
                    track_activity(daily, &mut last_activity_ms, timestamp_ms);
                }
            }
        }
    }

    Ok(())
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
    let raw = value.get("timestamp")?;
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
    let mut secs = (ms.max(0) / 1000) as i64;
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
    let mut secs = (ms.max(0) / 1000) as i64;
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
    use std::path::PathBuf;
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
            session_cwd: None,
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
                    session_cwd: None,
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
}
