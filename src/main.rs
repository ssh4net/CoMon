mod app;
mod codex_rpc;
mod locale;
mod read;
mod storage;
mod ui;
mod usage;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;

const USER_CONFIG_SCHEMA_VERSION: u32 = 2;
const USER_CONFIG_FILE_NAME: &str = "config.json";
const DEFAULT_USAGE_DAYS: u32 = 30;
const DEFAULT_REFRESH_USAGE_SECS: u64 = 300;
const DEFAULT_REFRESH_LIMITS_SECS: u64 = 60;
const DEFAULT_MAX_SESSION_FILE_MIB: u64 = 256;
const DEFAULT_MAX_SESSION_TOTAL_MIB: u64 = 256;
const DEFAULT_MAX_SESSION_FILES: usize = 10_000;
const DEFAULT_MAX_JSONL_LINE_KIB: u64 = 512;
const DEFAULT_SCAN_TIME_BUDGET_MS: u64 = 1500;
const DEFAULT_HISTORY_CATALOG_MAX_CANDIDATES: usize = 10_000;
const DEFAULT_HISTORY_CATALOG_SCAN_BUDGET_MS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct UserConfig {
    schema_version: u32,
    usage_days: u32,
    refresh_usage_secs: u64,
    refresh_limits_secs: u64,
    max_session_file_mib: u64,
    max_session_total_mib: u64,
    max_session_files: usize,
    max_jsonl_line_kib: u64,
    scan_time_budget_ms: u64,
    full_scan: bool,
    scan_cache_max_entries: usize,
    history_project_roots: Vec<PathBuf>,
    history_deep_depth: u8,
    history_deep_max_depth: u8,
    history_catalog_max_candidates: usize,
    history_catalog_scan_budget_ms: u64,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            schema_version: USER_CONFIG_SCHEMA_VERSION,
            usage_days: DEFAULT_USAGE_DAYS,
            refresh_usage_secs: DEFAULT_REFRESH_USAGE_SECS,
            refresh_limits_secs: DEFAULT_REFRESH_LIMITS_SECS,
            max_session_file_mib: DEFAULT_MAX_SESSION_FILE_MIB,
            max_session_total_mib: DEFAULT_MAX_SESSION_TOTAL_MIB,
            max_session_files: DEFAULT_MAX_SESSION_FILES,
            max_jsonl_line_kib: DEFAULT_MAX_JSONL_LINE_KIB,
            scan_time_budget_ms: DEFAULT_SCAN_TIME_BUDGET_MS,
            full_scan: false,
            scan_cache_max_entries: usage::DEFAULT_SCAN_CACHE_MAX_ENTRIES,
            history_project_roots: default_history_project_roots(),
            history_deep_depth: read::catalog::DEFAULT_DEEP_DEPTH,
            history_deep_max_depth: read::catalog::MAX_DEEP_DEPTH,
            history_catalog_max_candidates: DEFAULT_HISTORY_CATALOG_MAX_CANDIDATES,
            history_catalog_scan_budget_ms: DEFAULT_HISTORY_CATALOG_SCAN_BUDGET_MS,
        }
    }
}

fn default_history_project_roots() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

fn validate_dir(path: &std::path::Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    // Best-effort canonicalization to normalize `..` and symlinks.
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn resolve_workspace_filter(project_override: Option<&Path>) -> Option<PathBuf> {
    // Codex-native path filter: sessions whose session cwd equals or is under this path.
    // No filesystem .git discovery.
    let project_candidate = project_override?;
    Some(
        std::fs::canonicalize(project_candidate).unwrap_or_else(|_| {
            if project_candidate.is_absolute() {
                project_candidate.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(project_candidate))
                    .unwrap_or_else(|_| project_candidate.to_path_buf())
            }
        }),
    )
}

fn resolve_comon_home(override_home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_home {
        return Some(path);
    }
    if let Ok(value) = std::env::var("COMON_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(value) = std::env::var("HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".comon"));
        }
    }
    if let Ok(value) = std::env::var("USERPROFILE") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".comon"));
        }
    }
    None
}

fn load_or_bootstrap_user_config(comon_home: &Path) -> Result<UserConfig> {
    let path = comon_home.join(USER_CONFIG_FILE_NAME);
    if !path.exists() {
        let defaults = UserConfig::default();
        let encoded = serde_json::to_vec_pretty(&defaults).with_context(|| {
            format!("Unable to encode default user config at {}", path.display())
        })?;
        crate::storage::write_private_file(&path, &encoded)?;
        return Ok(defaults);
    }

    crate::storage::enforce_private_file_if_exists(&path)?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("Unable to read user config {}", path.display()))?;
    let mut config = serde_json::from_slice::<UserConfig>(&bytes)
        .with_context(|| format!("Unable to parse user config {}", path.display()))?;
    if config.schema_version > USER_CONFIG_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported comon config schema version: {} (maximum {})",
            config.schema_version,
            USER_CONFIG_SCHEMA_VERSION
        );
    }
    if config.schema_version < USER_CONFIG_SCHEMA_VERSION {
        config.schema_version = USER_CONFIG_SCHEMA_VERSION;
        let encoded = serde_json::to_vec_pretty(&config)
            .with_context(|| format!("Unable to migrate user config {}", path.display()))?;
        crate::storage::write_private_file(&path, &encoded)?;
    }
    Ok(config)
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LiveLimitsArg {
    Auto,
    On,
    Off,
}

impl From<LiveLimitsArg> for app::LiveLimitsMode {
    fn from(value: LiveLimitsArg) -> Self {
        match value {
            LiveLimitsArg::Auto => Self::Auto,
            LiveLimitsArg::On => Self::On,
            LiveLimitsArg::Off => Self::Off,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "comon", version, about = "Codex usage + session browser TUI")]
struct Args {
    /// Launch with the session history screen active.
    #[arg(short = 'r', long = "read")]
    read_mode: bool,

    /// Override path to the Codex CLI binary (default: `codex` in PATH).
    #[arg(long)]
    codex_bin: Option<String>,

    /// Override a standalone Codex App Server executable (spawned directly).
    #[arg(long, conflicts_with = "codex_bin")]
    app_server_bin: Option<PathBuf>,

    /// Live limits behavior: auto tries App Server if found, on requires it, off disables it.
    #[arg(long, value_enum, default_value = "auto")]
    live_limits: LiveLimitsArg,

    /// Override CODEX_HOME (default: $CODEX_HOME or ~/.codex).
    #[arg(long)]
    codex_home: Option<PathBuf>,

    /// Override COMON_HOME for comon-owned state/cache files (default: $COMON_HOME or ~/.comon).
    #[arg(long)]
    comon_home: Option<PathBuf>,

    /// Print effective comon config path and exit.
    #[arg(long)]
    print_config_path: bool,

    /// Override the sessions directory directly for read mode.
    #[arg(long)]
    sessions_dir: Option<PathBuf>,

    /// Print the effective sessions directory for read mode and exit.
    #[arg(long)]
    print_sessions_dir: bool,

    /// Working directory to launch Codex App Server in (default: current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Filter usage stats to sessions whose working directory equals or is under this path.
    ///
    /// If `--cwd` is not provided, this also becomes the default working directory for
    /// launching Codex App Server.
    #[arg(long, alias = "workspace")]
    project: Option<PathBuf>,

    /// Number of days to scan for local usage (clamped to 1..=90; default from config).
    #[arg(long)]
    usage_days: Option<u32>,

    /// Periodic refresh interval for usage stats in seconds (default from config).
    #[arg(long)]
    refresh_usage_secs: Option<u64>,

    /// Periodic refresh interval for limits/credits in seconds (default from config).
    #[arg(long)]
    refresh_limits_secs: Option<u64>,

    /// Per-file scan budget weight in MiB used by planner (default from config).
    ///
    /// Large files are still supported via incremental parsing and cache offsets.
    #[arg(long)]
    max_session_file_mib: Option<u64>,

    /// Max total size in MiB to scan across session files (default from config).
    #[arg(long)]
    max_session_total_mib: Option<u64>,

    /// Max number of session files to scan per refresh (default from config).
    #[arg(long)]
    max_session_files: Option<usize>,

    /// Max size in KiB of one JSONL line parsed from session files (default from config).
    #[arg(long)]
    max_jsonl_line_kib: Option<u64>,

    /// Max parse budget in milliseconds per refresh (0 = unlimited; default from config).
    #[arg(long)]
    scan_time_budget_ms: Option<u64>,

    /// Scan all session files under CODEX_HOME/sessions (ignore mtime cutoff; overrides config).
    #[arg(long, conflicts_with = "no_full_scan")]
    full_scan: bool,

    /// Disable full session scan even if enabled in config.
    #[arg(long)]
    no_full_scan: bool,

    /// Max number of entries to keep in scan cache (default from config).
    #[arg(long)]
    scan_cache_max_entries: Option<usize>,

    /// Rebuild local scan cache on startup (delete `comon.db` before first usage scan).
    #[arg(long)]
    rebuild_cache_on_start: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.print_config_path {
        let comon_home = resolve_comon_home(args.comon_home.clone())
            .context("Unable to resolve COMON_HOME (default: ~/.comon)")?;
        println!("{}", comon_home.join(USER_CONFIG_FILE_NAME).display());
        return Ok(());
    }

    if args.print_sessions_dir {
        read::print_sessions_dir(args.codex_home.clone(), args.sessions_dir.clone())?;
        return Ok(());
    }
    let read_config = read::build_config(args.codex_home.clone(), args.sessions_dir.clone())?;

    let launch_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // `--project` controls usage scope.
    let cwd_override = args
        .cwd
        .clone()
        .map(|p| validate_dir(&p, "--cwd"))
        .transpose()?;
    let project_override = args
        .project
        .clone()
        .map(|p| validate_dir(&p, "--project"))
        .transpose()?;

    // Default to all workspaces unless user explicitly provides `--project`.
    let project = resolve_workspace_filter(project_override.as_deref());

    // `cwd` controls where `codex app-server` is launched.
    let cwd = cwd_override
        .or_else(|| project.clone())
        .unwrap_or_else(|| launch_dir.clone());
    let comon_home = resolve_comon_home(args.comon_home.clone())
        .context("Unable to resolve COMON_HOME (default: ~/.comon)")?;
    crate::storage::ensure_private_dir(&comon_home)?;
    let user_config = load_or_bootstrap_user_config(&comon_home)?;
    let codex_home = usage::resolve_codex_home(args.codex_home.clone())
        .context("Unable to resolve CODEX_HOME")?;

    let usage_days = args
        .usage_days
        .unwrap_or(user_config.usage_days)
        .clamp(1, 90);
    let refresh_usage_secs = args
        .refresh_usage_secs
        .unwrap_or(user_config.refresh_usage_secs)
        .max(30);
    let refresh_limits_secs = args
        .refresh_limits_secs
        .unwrap_or(user_config.refresh_limits_secs)
        .max(10);
    let full_scan = if args.full_scan {
        true
    } else if args.no_full_scan {
        false
    } else {
        user_config.full_scan
    };
    let max_session_total_mib = args
        .max_session_total_mib
        .unwrap_or(user_config.max_session_total_mib)
        .max(1);
    let max_session_file_mib = args
        .max_session_file_mib
        .unwrap_or(user_config.max_session_file_mib)
        .max(1)
        .min(max_session_total_mib);
    let max_session_files = args
        .max_session_files
        .unwrap_or(user_config.max_session_files)
        .max(1);
    let max_jsonl_line_kib = args
        .max_jsonl_line_kib
        .unwrap_or(user_config.max_jsonl_line_kib)
        .max(1);
    let scan_time_budget_ms = args
        .scan_time_budget_ms
        .unwrap_or(user_config.scan_time_budget_ms);
    let scan_cache_max_entries = args
        .scan_cache_max_entries
        .unwrap_or(user_config.scan_cache_max_entries)
        .max(1);

    let max_session_total_bytes = max_session_total_mib.saturating_mul(1024 * 1024);
    let max_session_file_bytes = max_session_file_mib
        .saturating_mul(1024 * 1024)
        .min(max_session_total_bytes);
    let max_jsonl_line_bytes =
        usize::try_from(max_jsonl_line_kib.saturating_mul(1024)).unwrap_or(usize::MAX);
    let usage_scan_limits = usage::ScanLimits {
        max_session_file_bytes,
        max_session_total_bytes,
        max_session_files_scanned: max_session_files,
        max_jsonl_line_bytes,
        scan_time_budget_ms,
        full_scan,
        scan_cache_max_entries,
    };
    let system_locale = locale::SystemLocale::detect();

    let config = app::Config {
        codex_bin: args.codex_bin.clone(),
        app_server_bin: args.app_server_bin.clone(),
        live_limits_mode: args.live_limits.into(),
        comon_home,
        codex_home,
        read_sessions_dir: read_config.sessions_dir,
        start_in_read_screen: args.read_mode,
        cwd,
        workspace_path: project,
        usage_days,
        refresh_usage_secs,
        refresh_limits_secs,
        usage_scan_limits,
        rebuild_cache_on_start: args.rebuild_cache_on_start,
        system_locale,
        history_project_roots: user_config.history_project_roots,
        history_deep_depth: user_config.history_deep_depth.clamp(
            1,
            user_config
                .history_deep_max_depth
                .clamp(1, read::catalog::MAX_DEEP_DEPTH),
        ),
        history_deep_max_depth: user_config
            .history_deep_max_depth
            .clamp(1, read::catalog::MAX_DEEP_DEPTH),
        history_catalog_max_candidates: user_config.history_catalog_max_candidates.max(1),
        history_catalog_scan_budget_ms: user_config.history_catalog_scan_budget_ms.max(25),
    };

    app::run(config).await
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let dir = test_temp_base_without_git_parent().join(format!("comon-main-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn test_temp_base_without_git_parent() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn resolve_workspace_path_uses_all_workspaces_outside_repo() {
        let root = make_temp_dir("non-repo");
        let workspace = resolve_workspace_filter(None);
        assert!(workspace.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_filter_uses_all_workspaces_without_project_override() {
        let root = make_temp_dir("launch-path");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace dir");

        let filtered = resolve_workspace_filter(None);
        assert!(filtered.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_filter_uses_project_path_as_filter() {
        let root = make_temp_dir("project-override-path");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        let expected = std::fs::canonicalize(&nested).unwrap_or_else(|_| nested.clone());

        let filtered = resolve_workspace_filter(Some(nested.as_path()));
        assert_eq!(filtered, Some(expected));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_filter_accepts_non_git_directory() {
        let root = make_temp_dir("project-override-non-git");
        let plain = root.join("plain-dir");
        std::fs::create_dir_all(&plain).expect("create plain dir");
        let expected = std::fs::canonicalize(&plain).unwrap_or_else(|_| plain.clone());

        let filtered = resolve_workspace_filter(Some(plain.as_path()));
        assert_eq!(filtered, Some(expected));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn user_config_schema_one_migrates_without_losing_scan_settings() {
        let comon_home = make_temp_dir("config-migration");
        let path = comon_home.join(USER_CONFIG_FILE_NAME);
        let legacy = serde_json::json!({
            "schema_version": 1,
            "usage_days": 45,
            "refresh_usage_secs": 600
        });
        crate::storage::write_private_file(
            &path,
            &serde_json::to_vec_pretty(&legacy).expect("encode legacy config"),
        )
        .expect("write legacy config");

        let migrated = load_or_bootstrap_user_config(&comon_home).expect("migrate config");
        assert_eq!(migrated.schema_version, USER_CONFIG_SCHEMA_VERSION);
        assert_eq!(migrated.usage_days, 45);
        assert_eq!(migrated.refresh_usage_secs, 600);
        assert_eq!(
            migrated.history_deep_depth,
            read::catalog::DEFAULT_DEEP_DEPTH
        );
        assert!(!migrated.history_project_roots.is_empty());

        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read migrated config"))
                .expect("parse migrated config");
        assert_eq!(persisted["schema_version"], USER_CONFIG_SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(comon_home);
    }
}
