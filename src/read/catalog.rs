use crate::read::scan::{Catalog, SessionSummary};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const DEFAULT_DEEP_DEPTH: u8 = 2;
pub(crate) const MAX_DEEP_DEPTH: u8 = 8;
const CATALOG_SCHEMA_VERSION: i64 = 1;
const MAX_PATH_BYTES: usize = 4096;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_RELATED_PROJECTS_PER_SESSION: usize = 64;
const EVIDENCE_PASS_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) const SOURCE_OWNER: u32 = 1 << 0;
pub(crate) const SOURCE_REPOSITORY: u32 = 1 << 1;
pub(crate) const SOURCE_WORKDIR: u32 = 1 << 2;
pub(crate) const SOURCE_FILE_TARGET: u32 = 1 << 3;
pub(crate) const SOURCE_COMMAND_PATH: u32 = 1 << 4;
pub(crate) const SOURCE_USER_SELECTED: u32 = 1 << 5;
pub(crate) const SOURCE_NOISY_TREE: u32 = 1 << 6;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectViewMode {
    #[default]
    Strict,
    Deep,
    Full,
    Custom,
}

impl ProjectViewMode {
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Strict => Self::Deep,
            Self::Deep => Self::Full,
            Self::Full => Self::Custom,
            Self::Custom => Self::Strict,
        }
    }

    pub(crate) fn store_value(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Deep => "deep",
            Self::Full => "full",
            Self::Custom => "custom",
        }
    }

    pub(crate) fn from_store(value: &str) -> Option<Self> {
        match value {
            "strict" => Some(Self::Strict),
            "deep" => Some(Self::Deep),
            "full" => Some(Self::Full),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogScanConfig {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) search_roots: Vec<PathBuf>,
    pub(crate) excluded_roots: Vec<PathBuf>,
    pub(crate) max_depth: u8,
    pub(crate) max_candidates: usize,
    pub(crate) progress_interval_ms: u64,
    pub(crate) cache_db_path: PathBuf,
    pub(crate) cancelled: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CatalogSnapshot {
    pub(crate) checkouts: Vec<ProjectCheckout>,
    pub(crate) links: Vec<SessionProjectLink>,
    pub(crate) directories_scanned: usize,
    pub(crate) sessions_scanned: usize,
    pub(crate) sessions_total: usize,
    pub(crate) truncated: bool,
    pub(crate) updated_at: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogProgress {
    pub(crate) phase: CatalogScanPhase,
    pub(crate) completed: usize,
    pub(crate) total: usize,
    pub(crate) projects: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CatalogScanPhase {
    #[default]
    Repositories,
    Sessions,
    Saving,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProjectCheckout {
    pub(crate) stable_id: String,
    pub(crate) checkout_key: String,
    pub(crate) display_path: String,
    pub(crate) remote_key: Option<String>,
    pub(crate) logical_name: String,
    pub(crate) discovery_depth: u8,
    pub(crate) source_flags: u32,
    pub(crate) confidence: u8,
    pub(crate) deep_eligible: bool,
    pub(crate) first_seen: i64,
    pub(crate) last_seen: i64,
    pub(crate) missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionProjectLink {
    pub(crate) session_path: String,
    pub(crate) checkout_key: String,
    pub(crate) evidence_mask: u32,
    pub(crate) evidence_count: usize,
    pub(crate) confidence: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PathEvidence {
    path: String,
    source_flags: u32,
    confidence: u8,
}

#[derive(Debug, Clone)]
struct DiscoveredCheckout {
    checkout_key: String,
    display_path: String,
    remote_key: Option<String>,
    logical_name: String,
    discovery_depth: u8,
    noisy: bool,
}

#[derive(Debug, Clone)]
struct CachedEvidence {
    file_size: u64,
    file_mtime_ms: i64,
    evidence: Vec<PathEvidence>,
}

pub(crate) fn scan_project_catalog<F>(
    config: &CatalogScanConfig,
    strict: &Catalog,
    mut progress: F,
) -> Result<CatalogSnapshot>
where
    F: FnMut(CatalogProgress),
{
    let interval = Duration::from_millis(config.progress_interval_ms.max(25));
    let mut last_progress = Instant::now();
    let mut directory_count = 0usize;
    let mut truncated = false;
    let mut discovered = Vec::new();

    for root in normalized_unique_roots(&config.search_roots) {
        if config.cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("project catalog scan cancelled");
        }
        let excluded = normalized_unique_roots(&config.excluded_roots);
        discover_repositories(
            &root,
            config.max_depth.clamp(1, MAX_DEEP_DEPTH),
            config.max_candidates.max(1),
            &excluded,
            &mut directory_count,
            &mut truncated,
            &mut discovered,
            config.cancelled.as_ref(),
            |scanned, projects| {
                if last_progress.elapsed() >= interval {
                    progress(CatalogProgress {
                        phase: CatalogScanPhase::Repositories,
                        completed: scanned,
                        total: 0,
                        projects,
                    });
                    last_progress = Instant::now();
                }
            },
        )?;
        if truncated {
            break;
        }
    }

    discovered.sort_by(|left, right| left.checkout_key.cmp(&right.checkout_key));
    discovered.dedup_by(|left, right| left.checkout_key == right.checkout_key);

    scan_catalog_evidence(
        config,
        strict,
        discovered,
        directory_count,
        truncated,
        progress,
    )
}

pub(crate) fn continue_project_catalog<F>(
    config: &CatalogScanConfig,
    strict: &Catalog,
    progress: F,
) -> Result<CatalogSnapshot>
where
    F: FnMut(CatalogProgress),
{
    let Some(snapshot) = load_catalog_cache(&config.cache_db_path)? else {
        return scan_project_catalog(config, strict, progress);
    };
    let discovered = snapshot
        .checkouts
        .into_iter()
        .filter(|checkout| !checkout.missing)
        .map(|checkout| DiscoveredCheckout {
            checkout_key: checkout.checkout_key,
            display_path: checkout.display_path,
            remote_key: checkout.remote_key,
            logical_name: checkout.logical_name,
            discovery_depth: checkout.discovery_depth,
            noisy: checkout.source_flags & SOURCE_NOISY_TREE != 0,
        })
        .collect();
    scan_catalog_evidence(config, strict, discovered, 0, snapshot.truncated, progress)
}

fn scan_catalog_evidence<F>(
    config: &CatalogScanConfig,
    strict: &Catalog,
    discovered: Vec<DiscoveredCheckout>,
    directory_count: usize,
    truncated: bool,
    mut progress: F,
) -> Result<CatalogSnapshot>
where
    F: FnMut(CatalogProgress),
{
    let now = unix_time_seconds();
    let interval = Duration::from_millis(config.progress_interval_ms.max(25));
    let mut last_progress = Instant::now();

    let mut connection = open_catalog_db(&config.cache_db_path)?;
    let cached_evidence = load_evidence_cache(&connection)?;
    let mut sessions = strict_sessions(strict);
    sessions.sort_by(|left, right| {
        right
            .started_at_sort_key_ms
            .cmp(&left.started_at_sort_key_ms)
            .then_with(|| left.file_path.cmp(&right.file_path))
    });
    let mut evidence_by_session = BTreeMap::new();
    let mut evidence_updates = BTreeMap::new();
    let mut bytes_scheduled = 0u64;
    let mut sessions_scanned = 0usize;
    for (index, session) in sessions.iter().enumerate() {
        if config.cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("project catalog scan cancelled");
        }
        let path = &session.file_path;
        let (size, mtime_ms) = regular_file_fingerprint(path).unwrap_or((0, 0));
        let cache_key = path.to_string_lossy().into_owned();
        let cached = cached_evidence.get(&cache_key);
        let exact =
            cached.is_some_and(|item| item.file_size == size && item.file_mtime_ms == mtime_ms);
        let parse_bytes = if exact {
            0
        } else if let Some(cached) = cached.filter(|item| size > item.file_size) {
            size.saturating_sub(cached.file_size)
        } else {
            size
        };
        let within_budget = parse_bytes == 0
            || bytes_scheduled == 0
            || bytes_scheduled.saturating_add(parse_bytes) <= EVIDENCE_PASS_BYTES;
        if !within_budget {
            if let Some(cached) = cached {
                evidence_by_session.insert(
                    cache_key,
                    (
                        cached.file_size,
                        cached.file_mtime_ms,
                        cached.evidence.clone(),
                    ),
                );
            }
            continue;
        }
        bytes_scheduled = bytes_scheduled.saturating_add(parse_bytes);
        let evidence = match cached {
            Some(cached) if cached.file_size == size && cached.file_mtime_ms == mtime_ms => {
                cached.evidence.clone()
            }
            Some(cached) if size > cached.file_size => {
                let mut evidence = cached.evidence.clone();
                evidence.extend(extract_evidence_bounded(
                    path,
                    cached.file_size,
                    config.cancelled.as_ref(),
                )?);
                evidence
            }
            _ => extract_evidence_bounded(path, 0, config.cancelled.as_ref())?,
        };
        sessions_scanned = sessions_scanned.saturating_add(1);
        evidence_by_session.insert(cache_key.clone(), (size, mtime_ms, evidence.clone()));
        if !exact {
            evidence_updates.insert(cache_key, (size, mtime_ms, evidence));
        }
        if last_progress.elapsed() >= interval || index + 1 == sessions.len() {
            progress(CatalogProgress {
                phase: CatalogScanPhase::Sessions,
                completed: sessions_scanned,
                total: sessions.len(),
                projects: discovered.len(),
            });
            last_progress = Instant::now();
        }
    }

    let mut links = build_links(&discovered, &evidence_by_session);
    links.sort_by(|left, right| {
        left.session_path
            .cmp(&right.session_path)
            .then_with(|| left.checkout_key.cmp(&right.checkout_key))
    });

    let mut link_rollup: BTreeMap<String, (u32, usize, u8)> = BTreeMap::new();
    for link in &links {
        let entry = link_rollup
            .entry(link.checkout_key.clone())
            .or_insert((0, 0, 0));
        entry.0 |= link.evidence_mask;
        entry.1 = entry.1.saturating_add(link.evidence_count);
        entry.2 = entry.2.max(link.confidence);
    }

    let checkouts = discovered
        .into_iter()
        .map(|checkout| {
            let (link_flags, evidence_count, link_confidence) = link_rollup
                .get(&checkout.checkout_key)
                .copied()
                .unwrap_or((0, 0, 0));
            let source_flags =
                SOURCE_REPOSITORY | link_flags | if checkout.noisy { SOURCE_NOISY_TREE } else { 0 };
            let deep_eligible =
                link_confidence >= 70 || (link_confidence >= 40 && evidence_count >= 2);
            let confidence = link_confidence.max(10);
            let stable_id = checkout
                .remote_key
                .as_ref()
                .map(|remote| format!("remote:{remote}"))
                .unwrap_or_else(|| format!("path:{}", checkout.checkout_key));
            ProjectCheckout {
                stable_id,
                checkout_key: checkout.checkout_key,
                display_path: checkout.display_path,
                remote_key: checkout.remote_key,
                logical_name: checkout.logical_name,
                discovery_depth: checkout.discovery_depth,
                source_flags,
                confidence,
                deep_eligible,
                first_seen: now,
                last_seen: now,
                missing: false,
            }
        })
        .collect::<Vec<_>>();

    progress(CatalogProgress {
        phase: CatalogScanPhase::Saving,
        completed: sessions_scanned,
        total: sessions.len(),
        projects: checkouts.len(),
    });
    if config.cancelled.load(Ordering::Relaxed) {
        anyhow::bail!("project catalog scan cancelled");
    }
    save_catalog_cache(
        &mut connection,
        config,
        &checkouts,
        &links,
        &evidence_updates,
        now,
        truncated,
    )?;

    load_catalog_cache_from_connection(&connection).map(|mut snapshot| {
        snapshot.directories_scanned = directory_count;
        snapshot.sessions_scanned = sessions_scanned;
        snapshot.sessions_total = sessions.len();
        snapshot.truncated = truncated;
        snapshot.updated_at = now;
        snapshot
    })
}

pub(crate) fn load_catalog_cache(path: &Path) -> Result<Option<CatalogSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }
    let connection = open_catalog_db(path)?;
    let version = connection
        .query_row(
            "SELECT value FROM catalog_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<i64>().ok());
    if version != Some(CATALOG_SCHEMA_VERSION) {
        return Ok(None);
    }
    Ok(Some(load_catalog_cache_from_connection(&connection)?))
}

#[allow(clippy::too_many_arguments)]
fn discover_repositories<F>(
    root: &Path,
    max_depth: u8,
    max_candidates: usize,
    excluded_roots: &[PathBuf],
    directory_count: &mut usize,
    truncated: &mut bool,
    out: &mut Vec<DiscoveredCheckout>,
    cancelled: &AtomicBool,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(usize, usize),
{
    let root_meta = match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => meta,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", root.display()))
        }
    };
    let _ = root_meta;
    let mut queue = VecDeque::from([(root.to_path_buf(), 0u8)]);
    let max_directories = max_candidates.saturating_mul(5).clamp(10_000, 50_000);
    while let Some((dir, depth)) = queue.pop_front() {
        if cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("project catalog scan cancelled");
        }
        if *directory_count >= max_directories || out.len() >= max_candidates {
            *truncated = true;
            break;
        }
        *directory_count = directory_count.saturating_add(1);
        on_progress(*directory_count, out.len());

        if depth > 0 && repository_marker(&dir).is_some() {
            if let Some(checkout) = inspect_checkout(&dir, depth, root) {
                out.push(checkout);
            }
        }
        if depth > 0 && should_prune_subtree(&dir) {
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
                continue;
            }
            if excluded_roots
                .iter()
                .any(|excluded| path_starts_with(&path, excluded))
            {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            queue.push_back((path, depth.saturating_add(1)));
        }
    }
    Ok(())
}

fn inspect_checkout(path: &Path, depth: u8, root: &Path) -> Option<DiscoveredCheckout> {
    let display_path = path.to_string_lossy().into_owned();
    if display_path.len() > MAX_PATH_BYTES {
        return None;
    }
    let checkout_key = normalize_local_path(path);
    let raw_remote = read_origin_remote(path);
    let remote_key = raw_remote.as_deref().and_then(normalize_remote_url);
    let logical_name = remote_key
        .as_deref()
        .map(remote_display_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&display_path)
                .to_string()
        });
    Some(DiscoveredCheckout {
        checkout_key,
        display_path,
        remote_key,
        logical_name,
        discovery_depth: depth.max(1),
        noisy: is_noisy_relative_path(path.strip_prefix(root).unwrap_or(path)),
    })
}

fn repository_marker(path: &Path) -> Option<PathBuf> {
    let marker = path.join(".git");
    let meta = std::fs::symlink_metadata(&marker).ok()?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return None;
    }
    if file_type.is_dir() || file_type.is_file() {
        Some(marker)
    } else {
        None
    }
}

fn read_origin_remote(checkout: &Path) -> Option<String> {
    let marker = repository_marker(checkout)?;
    let meta = std::fs::symlink_metadata(&marker).ok()?;
    let config_path = if meta.file_type().is_dir() {
        marker.join("config")
    } else {
        if meta.len() > 64 * 1024 {
            return None;
        }
        let content = std::fs::read_to_string(&marker).ok()?;
        let gitdir = content.trim().strip_prefix("gitdir:")?.trim();
        if gitdir.len() > MAX_PATH_BYTES {
            return None;
        }
        let gitdir = PathBuf::from(gitdir);
        let gitdir = if gitdir.is_absolute() {
            gitdir
        } else {
            checkout.join(gitdir)
        };
        let direct = gitdir.join("config");
        if direct.is_file() {
            direct
        } else {
            let common_file = gitdir.join("commondir");
            let common_meta = std::fs::symlink_metadata(&common_file).ok()?;
            if common_meta.file_type().is_symlink()
                || !common_meta.file_type().is_file()
                || common_meta.len() > 4096
            {
                return None;
            }
            let common = std::fs::read_to_string(&common_file).ok()?;
            let common = common.trim();
            if common.is_empty() || common.len() > MAX_PATH_BYTES {
                return None;
            }
            let common = PathBuf::from(common);
            let common = if common.is_absolute() {
                common
            } else {
                gitdir.join(common)
            };
            normalize_path(&common).join("config")
        }
    };
    let meta = std::fs::symlink_metadata(&config_path).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let content = std::fs::read_to_string(config_path).ok()?;
    let mut in_origin = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed.eq_ignore_ascii_case("[remote \"origin\"]");
            continue;
        }
        if in_origin {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim().eq_ignore_ascii_case("url") {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn normalize_remote_url(raw: &str) -> Option<String> {
    let mut value = raw.trim();
    if value.is_empty() || value.len() > MAX_PATH_BYTES {
        return None;
    }
    value = value.split(['?', '#']).next().unwrap_or(value);
    let normalized = if let Some((_, rest)) = value.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        let host = host_port
            .trim_matches(['[', ']'])
            .split(':')
            .next()
            .unwrap_or(host_port)
            .to_ascii_lowercase();
        if host.is_empty() || path.is_empty() {
            return None;
        }
        format!("{host}/{}", path.trim_start_matches('/'))
    } else if let Some((left, path)) = value.rsplit_once(':') {
        let host = left.rsplit('@').next().unwrap_or(left).to_ascii_lowercase();
        if host.is_empty() || path.is_empty() || host.contains('/') {
            return None;
        }
        format!("{host}/{}", path.trim_start_matches('/'))
    } else {
        return None;
    };
    let normalized = normalized
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(normalized.trim_end_matches('/'))
        .trim_end_matches('/')
        .to_string();
    (!normalized.is_empty()).then_some(normalized)
}

fn remote_display_name(remote: &str) -> String {
    let parts = remote.split('/').collect::<Vec<_>>();
    if parts.len() >= 3 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        parts.last().copied().unwrap_or(remote).to_string()
    }
}

#[cfg(test)]
fn extract_structured_evidence(path: &Path) -> Result<Vec<PathEvidence>> {
    extract_structured_evidence_from(path, 0)
}

#[cfg(test)]
fn extract_structured_evidence_from(path: &Path, offset: u64) -> Result<Vec<PathEvidence>> {
    extract_structured_evidence_from_cancellable(path, offset, None)
}

fn extract_structured_evidence_from_cancellable(
    path: &Path,
    offset: u64,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<PathEvidence>> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Unable to inspect {}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Ok(Vec::new());
    }
    let mut file =
        File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("Unable to seek {}", path.display()))?;
    }
    let reader = BufReader::new(file);
    let mut evidence = Vec::new();
    for line in reader.lines() {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            anyhow::bail!("project catalog scan cancelled");
        }
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload").and_then(Value::as_object) else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(arguments_raw) = payload.get("arguments").and_then(Value::as_str) else {
            continue;
        };
        if arguments_raw.len() > MAX_COMMAND_BYTES.saturating_mul(2) {
            continue;
        }
        let Ok(arguments) = serde_json::from_str::<Value>(arguments_raw) else {
            continue;
        };
        extract_argument_evidence(name, &arguments, &mut evidence);
        if evidence.len() >= MAX_RELATED_PROJECTS_PER_SESSION.saturating_mul(8) {
            break;
        }
    }
    Ok(evidence)
}

fn extract_evidence_bounded(
    path: &Path,
    offset: u64,
    cancelled: &AtomicBool,
) -> Result<Vec<PathEvidence>> {
    match extract_structured_evidence_from_cancellable(path, offset, Some(cancelled)) {
        Ok(evidence) => Ok(evidence),
        Err(error) if cancelled.load(Ordering::Relaxed) => Err(error),
        Err(_) => Ok(Vec::new()),
    }
}

fn extract_argument_evidence(name: &str, arguments: &Value, out: &mut Vec<PathEvidence>) {
    if !is_evidence_tool(name) {
        return;
    }
    let Some(object) = arguments.as_object() else {
        return;
    };
    let trusted_workdir = ["workdir", "cwd", "working_directory"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|path| valid_absolute_path(path));
    if let Some(workdir) = trusted_workdir {
        push_evidence(out, workdir, SOURCE_WORKDIR, 80);
    }

    let modifying = name.contains("patch")
        || name.contains("write")
        || name.contains("edit")
        || name.contains("create");
    for key in ["path", "file_path", "target_path"] {
        if let Some(raw) = object.get(key).and_then(Value::as_str) {
            if let Some(path) = resolve_evidence_path(raw, trusted_workdir) {
                push_evidence(
                    out,
                    &path,
                    if modifying {
                        SOURCE_FILE_TARGET
                    } else {
                        SOURCE_COMMAND_PATH
                    },
                    if modifying { 70 } else { 40 },
                );
            }
        }
    }
    for key in ["paths", "files"] {
        if let Some(values) = object.get(key).and_then(Value::as_array) {
            for raw in values.iter().filter_map(Value::as_str).take(64) {
                if let Some(path) = resolve_evidence_path(raw, trusted_workdir) {
                    push_evidence(
                        out,
                        &path,
                        if modifying {
                            SOURCE_FILE_TARGET
                        } else {
                            SOURCE_COMMAND_PATH
                        },
                        if modifying { 70 } else { 40 },
                    );
                }
            }
        }
    }
    for key in ["cmd", "command"] {
        let Some(command) = object.get(key).and_then(Value::as_str) else {
            continue;
        };
        if command.len() > MAX_COMMAND_BYTES {
            continue;
        }
        for token in command.split_whitespace().take(2048) {
            let token = token.trim_matches(|ch: char| {
                matches!(ch, '\'' | '"' | '`' | ',' | ';' | '(' | ')' | '[' | ']')
            });
            if !looks_like_command_path(token) {
                continue;
            }
            if let Some(path) = resolve_evidence_path(token, trusted_workdir) {
                push_evidence(out, &path, SOURCE_COMMAND_PATH, 40);
            }
        }
    }
    if name.to_ascii_lowercase().contains("patch") {
        for key in ["patch", "input"] {
            let Some(patch) = object.get(key).and_then(Value::as_str) else {
                continue;
            };
            if patch.len() > MAX_COMMAND_BYTES {
                continue;
            }
            for line in patch.lines() {
                let raw = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                    .iter()
                    .find_map(|prefix| line.strip_prefix(prefix));
                let Some(raw) = raw else { continue };
                if let Some(path) = resolve_evidence_path(raw.trim(), trusted_workdir) {
                    push_evidence(out, &path, SOURCE_FILE_TARGET, 70);
                }
            }
        }
    }
}

fn looks_like_command_path(token: &str) -> bool {
    if token.is_empty() || token.starts_with('-') || token.len() > MAX_PATH_BYTES {
        return false;
    }
    token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.contains('/')
        || token.contains('\\')
        || Path::new(token).extension().is_some()
}

fn is_evidence_tool(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "exec_command",
        "shell_command",
        "apply_patch",
        "write_file",
        "edit_file",
        "create_file",
        "read_file",
        "read_text_file",
        "view_image",
    ]
    .iter()
    .any(|allowed| name == *allowed || name.ends_with(&format!("__{allowed}")))
}

fn resolve_evidence_path(raw: &str, workdir: Option<&str>) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES || raw.contains('\0') {
        return None;
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Some(normalize_local_path(path));
    }
    let workdir = workdir?;
    Some(normalize_local_path(&Path::new(workdir).join(path)))
}

fn valid_absolute_path(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_PATH_BYTES
        && !raw.contains('\0')
        && Path::new(raw).is_absolute()
}

fn push_evidence(out: &mut Vec<PathEvidence>, path: &str, source_flags: u32, confidence: u8) {
    if path.len() <= MAX_PATH_BYTES {
        out.push(PathEvidence {
            path: path.to_string(),
            source_flags,
            confidence,
        });
    }
}

fn build_links(
    checkouts: &[DiscoveredCheckout],
    evidence_by_session: &BTreeMap<String, (u64, i64, Vec<PathEvidence>)>,
) -> Vec<SessionProjectLink> {
    let mut links = Vec::new();
    for (session_path, (_, _, evidence)) in evidence_by_session {
        let mut grouped: BTreeMap<String, (u32, usize, u8)> = BTreeMap::new();
        for item in evidence {
            let Some(checkout) = nearest_checkout(&item.path, checkouts) else {
                continue;
            };
            let entry = grouped
                .entry(checkout.checkout_key.clone())
                .or_insert((0, 0, 0));
            entry.0 |= item.source_flags;
            entry.1 = entry.1.saturating_add(1);
            entry.2 = entry.2.max(item.confidence);
        }
        for (checkout_key, (evidence_mask, evidence_count, confidence)) in
            grouped.into_iter().take(MAX_RELATED_PROJECTS_PER_SESSION)
        {
            links.push(SessionProjectLink {
                session_path: session_path.clone(),
                checkout_key,
                evidence_mask,
                evidence_count,
                confidence,
            });
        }
    }
    links
}

fn nearest_checkout<'a>(
    evidence_path: &str,
    checkouts: &'a [DiscoveredCheckout],
) -> Option<&'a DiscoveredCheckout> {
    checkouts
        .iter()
        .filter(|checkout| string_path_starts_with(evidence_path, &checkout.checkout_key))
        .max_by_key(|checkout| checkout.checkout_key.len())
}

fn open_catalog_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        crate::storage::ensure_private_dir(parent)?;
    }
    crate::storage::enforce_private_file_if_exists(path)?;
    let connection = Connection::open(path)
        .with_context(|| format!("Unable to open catalog cache {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS catalog_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS project_checkout (
             checkout_key TEXT PRIMARY KEY,
             stable_id TEXT NOT NULL,
             display_path TEXT NOT NULL,
             remote_key TEXT,
             logical_name TEXT NOT NULL,
             discovery_depth INTEGER NOT NULL,
             source_flags INTEGER NOT NULL,
             confidence INTEGER NOT NULL,
             deep_eligible INTEGER NOT NULL,
             first_seen INTEGER NOT NULL,
             last_seen INTEGER NOT NULL,
             missing INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS session_project_link (
             session_path TEXT NOT NULL,
             checkout_key TEXT NOT NULL,
             evidence_mask INTEGER NOT NULL,
             evidence_count INTEGER NOT NULL,
             confidence INTEGER NOT NULL,
             last_seen INTEGER NOT NULL,
             PRIMARY KEY(session_path, checkout_key)
         );
         CREATE TABLE IF NOT EXISTS project_evidence_file_cache (
             session_path TEXT PRIMARY KEY,
             file_size INTEGER NOT NULL,
             file_mtime_ms INTEGER NOT NULL,
             file_offset INTEGER NOT NULL,
             fully_parsed INTEGER NOT NULL,
             evidence_json TEXT NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS catalog_scan_root (
             root_path TEXT PRIMARY KEY,
             max_depth INTEGER NOT NULL,
             last_scan INTEGER NOT NULL
         );",
    )?;
    let stored_version = connection
        .query_row(
            "SELECT value FROM catalog_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match stored_version {
        None => {
            connection.execute(
                "INSERT INTO catalog_meta(key, value) VALUES('schema_version', ?1)",
                [CATALOG_SCHEMA_VERSION.to_string()],
            )?;
        }
        Some(value) if value == CATALOG_SCHEMA_VERSION.to_string() => {}
        Some(value) => anyhow::bail!(
            "Unsupported project catalog schema version: {value} (expected {CATALOG_SCHEMA_VERSION})"
        ),
    }
    crate::storage::enforce_private_file_if_exists(path)?;
    Ok(connection)
}

fn save_catalog_cache(
    connection: &mut Connection,
    config: &CatalogScanConfig,
    checkouts: &[ProjectCheckout],
    links: &[SessionProjectLink],
    evidence_by_session: &BTreeMap<String, (u64, i64, Vec<PathEvidence>)>,
    now: i64,
    truncated: bool,
) -> Result<()> {
    let tx = connection.transaction()?;
    for checkout in checkouts {
        tx.execute(
            "INSERT INTO project_checkout(
                checkout_key, stable_id, display_path, remote_key, logical_name,
                discovery_depth, source_flags, confidence, deep_eligible,
                first_seen, last_seen, missing
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)
             ON CONFLICT(checkout_key) DO UPDATE SET
                stable_id = excluded.stable_id,
                display_path = excluded.display_path,
                remote_key = excluded.remote_key,
                logical_name = excluded.logical_name,
                discovery_depth = excluded.discovery_depth,
                source_flags = excluded.source_flags,
                confidence = excluded.confidence,
                deep_eligible = excluded.deep_eligible,
                last_seen = excluded.last_seen,
                missing = 0",
            params![
                checkout.checkout_key,
                checkout.stable_id,
                checkout.display_path,
                checkout.remote_key,
                checkout.logical_name,
                i64::from(checkout.discovery_depth),
                i64::from(checkout.source_flags),
                i64::from(checkout.confidence),
                i64::from(checkout.deep_eligible),
                checkout.first_seen,
                checkout.last_seen,
            ],
        )?;
    }
    tx.execute("DELETE FROM session_project_link", [])?;
    for link in links {
        tx.execute(
            "INSERT INTO session_project_link(
                session_path, checkout_key, evidence_mask, evidence_count, confidence, last_seen
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                link.session_path,
                link.checkout_key,
                i64::from(link.evidence_mask),
                i64::try_from(link.evidence_count).unwrap_or(i64::MAX),
                i64::from(link.confidence),
                now,
            ],
        )?;
    }
    for (session_path, (file_size, file_mtime_ms, evidence)) in evidence_by_session {
        let encoded = serde_json::to_string(evidence)?;
        tx.execute(
            "INSERT INTO project_evidence_file_cache(
                session_path, file_size, file_mtime_ms, file_offset,
                fully_parsed, evidence_json, updated_at
             ) VALUES(?1, ?2, ?3, ?2, 1, ?4, ?5)
             ON CONFLICT(session_path) DO UPDATE SET
                file_size = excluded.file_size,
                file_mtime_ms = excluded.file_mtime_ms,
                file_offset = excluded.file_offset,
                fully_parsed = 1,
                evidence_json = excluded.evidence_json,
                updated_at = excluded.updated_at",
            params![
                session_path,
                i64::try_from(*file_size).unwrap_or(i64::MAX),
                file_mtime_ms,
                encoded,
                now,
            ],
        )?;
    }
    for root in normalized_unique_roots(&config.search_roots) {
        tx.execute(
            "INSERT INTO catalog_scan_root(root_path, max_depth, last_scan)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(root_path) DO UPDATE SET
                max_depth = excluded.max_depth,
                last_scan = excluded.last_scan",
            params![root.to_string_lossy(), i64::from(config.max_depth), now],
        )?;
    }
    tx.execute(
        "INSERT INTO catalog_meta(key, value) VALUES('updated_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [now.to_string()],
    )?;
    tx.execute(
        "INSERT INTO catalog_meta(key, value) VALUES('truncated', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [if truncated { "1" } else { "0" }],
    )?;
    tx.commit()?;
    Ok(())
}

fn load_catalog_cache_from_connection(connection: &Connection) -> Result<CatalogSnapshot> {
    let mut checkout_statement = connection.prepare(
        "SELECT stable_id, checkout_key, display_path, remote_key, logical_name,
                discovery_depth, source_flags, confidence, deep_eligible,
                first_seen, last_seen, missing
         FROM project_checkout
         ORDER BY logical_name, display_path",
    )?;
    let checkouts = checkout_statement
        .query_map([], |row| {
            Ok(ProjectCheckout {
                stable_id: row.get(0)?,
                checkout_key: row.get(1)?,
                display_path: row.get(2)?,
                remote_key: row.get(3)?,
                logical_name: row.get(4)?,
                discovery_depth: row.get::<_, i64>(5)?.clamp(0, 255) as u8,
                source_flags: row.get::<_, i64>(6)?.max(0) as u32,
                confidence: row.get::<_, i64>(7)?.clamp(0, 255) as u8,
                deep_eligible: row.get::<_, i64>(8)? != 0,
                first_seen: row.get(9)?,
                last_seen: row.get(10)?,
                missing: row.get::<_, i64>(11)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut link_statement = connection.prepare(
        "SELECT session_path, checkout_key, evidence_mask, evidence_count, confidence
         FROM session_project_link
         ORDER BY session_path, checkout_key",
    )?;
    let links = link_statement
        .query_map([], |row| {
            Ok(SessionProjectLink {
                session_path: row.get(0)?,
                checkout_key: row.get(1)?,
                evidence_mask: row.get::<_, i64>(2)?.max(0) as u32,
                evidence_count: row.get::<_, i64>(3)?.max(0) as usize,
                confidence: row.get::<_, i64>(4)?.clamp(0, 255) as u8,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let updated_at = connection
        .query_row(
            "SELECT value FROM catalog_meta WHERE key = 'updated_at'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let truncated = connection
        .query_row(
            "SELECT value FROM catalog_meta WHERE key = 'truncated'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some_and(|value| value == "1");
    let checkouts = checkouts
        .into_iter()
        .map(|mut checkout| {
            checkout.missing |= repository_marker(Path::new(&checkout.display_path)).is_none();
            checkout
        })
        .collect();
    Ok(CatalogSnapshot {
        checkouts,
        links,
        updated_at,
        truncated,
        ..CatalogSnapshot::default()
    })
}

fn load_evidence_cache(connection: &Connection) -> Result<BTreeMap<String, CachedEvidence>> {
    let mut statement = connection.prepare(
        "SELECT session_path, file_size, file_mtime_ms, evidence_json
         FROM project_evidence_file_cache
         WHERE fully_parsed = 1",
    )?;
    let rows = statement.query_map([], |row| {
        let session_path = row.get::<_, String>(0)?;
        let file_size = row.get::<_, i64>(1)?.max(0) as u64;
        let file_mtime_ms = row.get::<_, i64>(2)?;
        let evidence_json = row.get::<_, String>(3)?;
        Ok((session_path, file_size, file_mtime_ms, evidence_json))
    })?;
    let mut cache = BTreeMap::new();
    for row in rows {
        let (session_path, file_size, file_mtime_ms, evidence_json) = row?;
        let evidence = serde_json::from_str(&evidence_json).unwrap_or_default();
        cache.insert(
            session_path,
            CachedEvidence {
                file_size,
                file_mtime_ms,
                evidence,
            },
        );
    }
    Ok(cache)
}

fn strict_sessions(strict: &Catalog) -> Vec<&SessionSummary> {
    strict
        .projects
        .iter()
        .flat_map(|project| project.sessions.iter())
        .collect()
}

fn regular_file_fingerprint(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(system_time_millis)
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

fn system_time_millis(value: SystemTime) -> Option<i64> {
    let duration = value.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

fn normalized_unique_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for root in roots {
        let normalized = std::fs::canonicalize(root).unwrap_or_else(|_| normalize_path(root));
        let key = normalize_local_path(&normalized);
        if seen.insert(key) {
            output.push(normalized);
        }
    }
    output
}

fn normalize_local_path(path: &Path) -> String {
    normalize_path(path).to_string_lossy().into_owned()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

fn path_starts_with(path: &Path, base: &Path) -> bool {
    normalize_path(path).starts_with(normalize_path(base))
}

fn string_path_starts_with(path: &str, base: &str) -> bool {
    if path == base {
        return true;
    }
    path.strip_prefix(base)
        .is_some_and(|rest| rest.starts_with(std::path::MAIN_SEPARATOR))
}

fn is_noisy_relative_path(path: &Path) -> bool {
    const NOISY: &[&str] = &[
        ".cache",
        ".cargo",
        ".rustup",
        "build",
        "dist",
        "dl",
        "feeds",
        "node_modules",
        "openwrt-sdk",
        "sdk",
        "target",
        "vendor",
    ];
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        NOISY.iter().any(|candidate| {
            value == *candidate || (candidate.ends_with("sdk") && value.contains("sdk"))
        })
    })
}

fn should_prune_subtree(path: &Path) -> bool {
    const PRUNED: &[&str] = &[
        ".cache",
        ".cargo",
        ".rustup",
        ".venv",
        "__pycache__",
        "bin",
        "build",
        "build_dir",
        "dl",
        "node_modules",
        "staging_dir",
        "target",
        "tmp",
    ];
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())
        .is_some_and(|name| PRUNED.contains(&name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "comon-catalog-{label}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp directory");
        path
    }

    #[test]
    fn project_mode_round_trips_and_cycles() {
        let mut mode = ProjectViewMode::Strict;
        for expected in [
            ProjectViewMode::Deep,
            ProjectViewMode::Full,
            ProjectViewMode::Custom,
            ProjectViewMode::Strict,
        ] {
            mode = mode.toggled();
            assert_eq!(mode, expected);
            assert_eq!(ProjectViewMode::from_store(mode.store_value()), Some(mode));
        }
    }

    #[test]
    fn remote_normalization_groups_ssh_and_https_without_credentials() {
        assert_eq!(
            normalize_remote_url("git@GitHub.com:ssh4net/CoMon.git"),
            Some("github.com/ssh4net/CoMon".to_string())
        );
        assert_eq!(
            normalize_remote_url("https://token@example.com/ssh4net/CoMon.git?x=1"),
            Some("example.com/ssh4net/CoMon".to_string())
        );
    }

    #[test]
    fn discovery_honors_depth_and_worktree_git_files() {
        let root = temp_dir("depth");
        let direct = root.join("direct");
        let nested = root.join("group/nested");
        std::fs::create_dir_all(direct.join(".git")).expect("direct git");
        std::fs::create_dir_all(nested.parent().expect("parent")).expect("nested parent");
        std::fs::write(nested.parent().expect("parent").join("unused"), "x").expect("write");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::write(nested.join(".git"), "gitdir: ../meta/worktrees/nested\n")
            .expect("worktree marker");

        let mut found = Vec::new();
        let mut dirs = 0;
        let mut truncated = false;
        discover_repositories(
            &root,
            1,
            100,
            &[],
            &mut dirs,
            &mut truncated,
            &mut found,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .expect("depth one scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].display_path, direct.display().to_string());

        found.clear();
        dirs = 0;
        discover_repositories(
            &root,
            2,
            100,
            &[],
            &mut dirs,
            &mut truncated,
            &mut found,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .expect("depth two scan");
        assert_eq!(found.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("symlink");
        let outside = temp_dir("outside");
        std::fs::create_dir_all(outside.join("repo/.git")).expect("outside repo");
        symlink(&outside, root.join("linked")).expect("create symlink");
        let mut found = Vec::new();
        let mut dirs = 0;
        let mut truncated = false;
        discover_repositories(
            &root,
            3,
            100,
            &[],
            &mut dirs,
            &mut truncated,
            &mut found,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .expect("scan");
        assert!(found.is_empty());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn structured_evidence_ignores_prose_and_outputs() {
        let root = temp_dir("evidence");
        let session = root.join("session.jsonl");
        let body = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{
                    "type": "input_text", "text": "/secret/prose/project"
                }]}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "function_call_output", "output": "/secret/output/project"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": serde_json::json!({
                        "workdir": "/safe/project",
                        "cmd": "sed -n 1,10p src/main.rs"
                    }).to_string()
                }
            })
        );
        std::fs::write(&session, body).expect("write session");
        let evidence = extract_structured_evidence(&session).expect("evidence");
        assert!(evidence.iter().any(|item| item.path == "/safe/project"));
        assert!(evidence
            .iter()
            .any(|item| item.path == "/safe/project/src/main.rs"));
        assert!(!evidence.iter().any(|item| item.path == "/safe/project/sed"));
        assert!(!evidence.iter().any(|item| item.path.contains("secret")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn command_path_filter_rejects_plain_words_but_keeps_common_paths() {
        assert!(!looks_like_command_path("cargo"));
        assert!(!looks_like_command_path("test"));
        assert!(!looks_like_command_path("--manifest-path"));
        assert!(looks_like_command_path("src/main.rs"));
        assert!(looks_like_command_path("Cargo.toml"));
        assert!(looks_like_command_path("../shared"));
        assert!(looks_like_command_path("C:\\src\\main.rs"));
    }

    #[test]
    fn catalog_scan_links_structured_workdir_and_round_trips_sqlite() {
        let root = temp_dir("round-trip");
        let project = root.join("project");
        let sessions = root.join("sessions/2026/07/29");
        std::fs::create_dir_all(project.join(".git")).expect("project git dir");
        std::fs::write(
            project.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:team/project.git\n",
        )
        .expect("git config");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let session_path = sessions.join("session.jsonl");
        let body = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": "session",
                    "timestamp": "2026-07-29T10:00:00Z",
                    "cwd": root.join("launcher")
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": serde_json::json!({
                        "workdir": project,
                        "cmd": "cargo test"
                    }).to_string()
                }
            })
        );
        std::fs::write(&session_path, body).expect("session file");
        let sessions_root = root.join("sessions");
        let strict = crate::read::scan::build_catalog(&sessions_root).expect("strict catalog");
        let config = CatalogScanConfig {
            sessions_dir: sessions_root.clone(),
            search_roots: vec![root.clone()],
            excluded_roots: vec![sessions_root],
            max_depth: 2,
            max_candidates: 100,
            progress_interval_ms: 25,
            cache_db_path: root.join("cache/comon.db"),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let snapshot = scan_project_catalog(&config, &strict, |_| {}).expect("catalog scan");
        assert_eq!(snapshot.checkouts.len(), 1);
        assert_eq!(
            snapshot.checkouts[0].stable_id,
            "remote:example.com/team/project"
        );
        assert!(snapshot.checkouts[0].deep_eligible);
        assert_eq!(snapshot.links.len(), 1);
        assert_eq!(snapshot.links[0].confidence, 80);

        let cached = load_catalog_cache(&config.cache_db_path)
            .expect("load cache")
            .expect("cached snapshot");
        assert_eq!(cached.checkouts.len(), 1);
        assert_eq!(cached.links.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
