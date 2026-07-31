use crate::codex_rpc::{AccountRateLimits, AccountUsage, CodexRpc};
use crate::locale::{DisplayFormatter, DisplayStyle, SystemLocale};
use crate::read;
use crate::usage::{ChartRange, LocalUsageSnapshot, UsageMetric, UsageZone};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
pub struct Config {
    pub codex_bin: Option<String>,
    pub app_server_bin: Option<std::path::PathBuf>,
    pub live_limits_mode: LiveLimitsMode,
    pub comon_home: std::path::PathBuf,
    pub codex_home: std::path::PathBuf,
    pub read_sessions_dir: std::path::PathBuf,
    pub start_in_read_screen: bool,
    pub cwd: std::path::PathBuf,
    pub workspace_path: Option<std::path::PathBuf>,
    pub usage_days: u32,
    pub refresh_usage_secs: u64,
    pub refresh_limits_secs: u64,
    pub usage_scan_limits: crate::usage::ScanLimits,
    pub rebuild_cache_on_start: bool,
    pub(crate) system_locale: SystemLocale,
    pub(crate) history_project_roots: Vec<PathBuf>,
    pub(crate) history_deep_depth: u8,
    pub(crate) history_deep_max_depth: u8,
    pub(crate) history_catalog_max_candidates: usize,
    pub(crate) history_catalog_scan_budget_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveLimitsMode {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiStatGrouping {
    Day,
    Week,
    Month,
}

impl ApiStatGrouping {
    fn toggled(self) -> Self {
        match self {
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Day,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiStatGraph {
    Bars,
    Heat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AccentTheme {
    #[default]
    Cyan,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Gray,
}

impl AccentTheme {
    pub(crate) const ALL: [Self; 7] = [
        Self::Cyan,
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Gray,
    ];

    pub(crate) fn colors(self) -> (Color, Color) {
        match self {
            Self::Cyan => (Color::Cyan, Color::LightCyan),
            Self::Red => (Color::Red, Color::LightRed),
            Self::Green => (Color::Green, Color::LightGreen),
            Self::Yellow => (Color::Yellow, Color::LightYellow),
            Self::Blue => (Color::Blue, Color::LightBlue),
            Self::Magenta => (Color::Magenta, Color::LightMagenta),
            Self::Gray => (Color::Gray, Color::White),
        }
    }

    pub(crate) fn cycled(self) -> Self {
        match self {
            Self::Cyan => Self::Red,
            Self::Red => Self::Green,
            Self::Green => Self::Yellow,
            Self::Yellow => Self::Blue,
            Self::Blue => Self::Magenta,
            Self::Magenta => Self::Gray,
            Self::Gray => Self::Cyan,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum BarFillMode {
    #[default]
    Semigraphic,
    DualColorBackground,
}

impl BarFillMode {
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Semigraphic => Self::DualColorBackground,
            Self::DualColorBackground => Self::Semigraphic,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UsageCommand {
    RefreshAll,
    ToggleMetric,
    ToggleRange,
    ToggleZone,
    ToggleOrientation,
    ScrollOlder,
    ScrollNewer,
    PageOlder,
    PageNewer,
    ScrollOldest,
    ScrollNewest,
    ToggleHelp,
    ConfirmContinue,
}

#[derive(Debug)]
enum AppEvent {
    UsageUpdated(Result<LocalUsageSnapshot>),
    LimitsUpdated(Result<AccountRateLimits>),
    AccountUsageUpdated(Result<AccountUsage>),
    AccountApiUnavailable {
        message: String,
        is_error: bool,
    },
    HistoryCatalogProgress(crate::read::catalog::CatalogProgress),
    HistoryCatalogUpdated {
        strict: crate::read::scan::Catalog,
        snapshot: crate::read::catalog::CatalogSnapshot,
        from_cache: bool,
    },
    HistoryCatalogFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveScreen {
    Usage,
    Activity,
    ApiStat,
    LimitResets,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiClickAction {
    SetScreen(ActiveScreen),
    SetDisplayStyle(DisplayStyle),
    SetAccentTheme(AccentTheme),
    ToggleBarFillMode,
    SetHistoryProjectMode(crate::read::catalog::ProjectViewMode),
    ConfirmHistoryCatalogScan,
    CancelHistoryCatalogScan,
    DecreaseHistoryDepth,
    IncreaseHistoryDepth,
    HistoryDepthWheel,
    SetMetric(UsageMetric),
    SetRange(ChartRange),
    SetUsageZone(UsageZone),
    SetOrientation(ChartOrientation),
    SetApiStatGrouping(ApiStatGrouping),
    SetApiStatGraph(ApiStatGraph),
    SetApiStatOrientation(ChartOrientation),
    ScrollUsageOlder,
    ScrollUsageNewer,
    SetUsageScrollOffset(usize),
    ScrollApiStatOlder,
    ScrollApiStatNewer,
    SetApiStatScrollOffset(usize),
    ScrollActivityOlder,
    ScrollActivityNewer,
    SetActivityScrollOffset(usize),
    DecreaseProjects,
    IncreaseProjects,
    PromptQuit,
    CancelQuit,
    ConfirmQuit,
    ToggleQuitDontAskAgain,
    PromptQuitConfirmationPreference,
    ConfirmQuitConfirmationPreference,
    CancelQuitConfirmationPreference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UiHitTarget {
    pub(crate) area: Rect,
    pub(crate) action: UiClickAction,
}

impl UiHitTarget {
    fn contains(self, column: u16, row: u16) -> bool {
        column >= self.area.x
            && column < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOutcome {
    Continue(bool),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitConfirmationCommand {
    Confirm,
    Cancel,
    QuitImmediately,
    ToggleDontAskAgain,
    SelectYes,
    SelectNo,
}

#[derive(Debug, Clone, Copy)]
enum ActivityCommand {
    RefreshAll,
    ToggleMetric,
    IncreaseProjects,
    DecreaseProjects,
    ScrollOlder,
    ScrollNewer,
    PageOlder,
    PageNewer,
    ScrollOldest,
    ScrollNewest,
    ToggleHelp,
}

#[derive(Debug, Clone, Copy)]
enum LimitResetCommand {
    RefreshLimits,
    ToggleHelp,
}

#[derive(Debug, Clone, Copy)]
enum ApiStatCommand {
    Refresh,
    ToggleGrouping,
    ToggleGraph,
    ToggleOrientation,
    ScrollOlder,
    ScrollNewer,
    PageOlder,
    PageNewer,
    ScrollOldest,
    ScrollNewest,
    ToggleHelp,
}

fn next_active_screen(screen: ActiveScreen) -> ActiveScreen {
    match screen {
        ActiveScreen::Usage => ActiveScreen::ApiStat,
        ActiveScreen::ApiStat => ActiveScreen::Activity,
        ActiveScreen::Activity => ActiveScreen::LimitResets,
        ActiveScreen::LimitResets => ActiveScreen::Read,
        ActiveScreen::Read => ActiveScreen::Usage,
    }
}

#[derive(Debug)]
pub(crate) struct AppState {
    pub(crate) active_screen: ActiveScreen,
    pub(crate) metric: UsageMetric,
    pub(crate) range: ChartRange,
    pub(crate) usage_zone: UsageZone,
    pub(crate) orientation: ChartOrientation,
    pub(crate) api_stat_grouping: ApiStatGrouping,
    pub(crate) api_stat_graph: ApiStatGraph,
    pub(crate) api_stat_orientation: ChartOrientation,
    pub(crate) usage_period_offset: usize,
    pub(crate) usage_visible_periods: usize,
    pub(crate) usage_total_periods: usize,
    pub(crate) usage_scroll_area: Option<Rect>,
    pub(crate) api_stat_period_offset: usize,
    pub(crate) api_stat_visible_periods: usize,
    pub(crate) api_stat_total_periods: usize,
    pub(crate) api_stat_scroll_area: Option<Rect>,
    pub(crate) activity_project_limit: usize,
    pub(crate) activity_week_offset: usize,
    pub(crate) activity_visible_weeks: usize,
    pub(crate) activity_total_weeks: usize,
    pub(crate) activity_scroll_area: Option<Rect>,
    pub(crate) show_help: bool,
    pub(crate) workspace_path: Option<std::path::PathBuf>,
    pub(crate) no_sessions_confirm_open: bool,
    pub(crate) no_sessions_confirm_dismissed: bool,
    pub(crate) quit_confirm_open: bool,
    pub(crate) quit_confirm_yes_selected: bool,
    pub(crate) quit_dont_ask_again: bool,
    pub(crate) skip_quit_confirmation: bool,
    pub(crate) quit_preference_prompt: Option<bool>,
    pub(crate) history_project_roots: Vec<PathBuf>,
    pub(crate) history_catalog_max_depth: u8,
    pub(crate) history_catalog_max_directories: usize,
    pub(crate) history_catalog_config_path: PathBuf,
    pub(crate) history_catalog_scan_prompt: bool,
    pub(crate) display_style: DisplayStyle,
    pub(crate) accent_theme: AccentTheme,
    pub(crate) bar_fill_mode: BarFillMode,
    pub(crate) system_locale: SystemLocale,
    pub(crate) mouse_position: Option<(u16, u16)>,
    pub(crate) ui_hit_targets: Vec<UiHitTarget>,

    pub(crate) usage: Option<LocalUsageSnapshot>,
    pub(crate) usage_updated_at: Option<Instant>,
    pub(crate) usage_error: Option<String>,

    pub(crate) limits: Option<AccountRateLimits>,
    pub(crate) limits_updated_at: Option<Instant>,
    pub(crate) limits_error: Option<String>,
    pub(crate) limits_notice: Option<String>,
    pub(crate) limits_enabled: bool,

    pub(crate) account_usage: Option<AccountUsage>,
    pub(crate) account_usage_updated_at: Option<Instant>,
    pub(crate) account_usage_error: Option<String>,
    pub(crate) account_usage_notice: Option<String>,
    pub(crate) account_usage_enabled: bool,
    pub(crate) read_browser: crate::read::tui::BrowserState,
}

const STATE_STORE_SCHEMA_VERSION: u32 = 2;
const STATE_STORE_FILE_NAME: &str = "state.json";
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
pub(crate) const DEFAULT_ACTIVITY_PROJECT_LIMIT: usize = 5;
pub(crate) const MIN_ACTIVITY_PROJECT_LIMIT: usize = 1;
pub(crate) const MAX_ACTIVITY_PROJECT_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitsWake {
    Poll,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedUiState {
    metric: UsageMetric,
    range: ChartRange,
    usage_zone: UsageZone,
    orientation: ChartOrientation,
    api_stat_grouping: ApiStatGrouping,
    api_stat_graph: ApiStatGraph,
    api_stat_orientation: ChartOrientation,
    activity_project_limit: usize,
    workspace_path: Option<PathBuf>,
    no_sessions_confirm_dismissed: bool,
    display_style: DisplayStyle,
    skip_quit_confirmation: bool,
    history_project_view_mode: crate::read::catalog::ProjectViewMode,
    history_deep_depth: u8,
    history_selected_projects: BTreeSet<String>,
    history_explicitly_excluded_projects: BTreeSet<String>,
    history_expanded_remote_groups: BTreeSet<String>,
}

impl PersistedUiState {
    fn default_for_workspace(workspace_path: Option<PathBuf>) -> Self {
        Self {
            metric: UsageMetric::Tokens,
            range: ChartRange::Day,
            usage_zone: UsageZone::Local,
            orientation: ChartOrientation::Horizontal,
            api_stat_grouping: ApiStatGrouping::Day,
            api_stat_graph: ApiStatGraph::Bars,
            api_stat_orientation: ChartOrientation::Vertical,
            activity_project_limit: DEFAULT_ACTIVITY_PROJECT_LIMIT,
            workspace_path,
            no_sessions_confirm_dismissed: false,
            display_style: DisplayStyle::Classic,
            skip_quit_confirmation: false,
            history_project_view_mode: crate::read::catalog::ProjectViewMode::Strict,
            history_deep_depth: crate::read::catalog::DEFAULT_DEEP_DEPTH,
            history_selected_projects: BTreeSet::new(),
            history_explicitly_excluded_projects: BTreeSet::new(),
            history_expanded_remote_groups: BTreeSet::new(),
        }
    }

    fn from_app_state(state: &AppState) -> Self {
        Self {
            metric: state.metric,
            range: state.range,
            usage_zone: state.usage_zone,
            orientation: state.orientation,
            api_stat_grouping: state.api_stat_grouping,
            api_stat_graph: state.api_stat_graph,
            api_stat_orientation: state.api_stat_orientation,
            activity_project_limit: state.activity_project_limit,
            workspace_path: state.workspace_path.clone(),
            no_sessions_confirm_dismissed: state.no_sessions_confirm_dismissed,
            display_style: state.display_style,
            skip_quit_confirmation: state.skip_quit_confirmation,
            history_project_view_mode: state.read_browser.project_mode(),
            history_deep_depth: state.read_browser.deep_depth(),
            history_selected_projects: state.read_browser.selected_projects().clone(),
            history_explicitly_excluded_projects: state
                .read_browser
                .explicitly_excluded_projects()
                .clone(),
            history_expanded_remote_groups: state.read_browser.expanded_remote_groups().clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateStore {
    schema_version: u32,
    #[serde(default)]
    global: StoredGlobalState,
    #[serde(default)]
    workspaces: BTreeMap<String, StoredWorkspaceState>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self {
            schema_version: STATE_STORE_SCHEMA_VERSION,
            global: StoredGlobalState::default(),
            workspaces: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoredGlobalState {
    metric: Option<String>,
    range: Option<String>,
    usage_zone: Option<String>,
    orientation: Option<String>,
    api_stat_grouping: Option<String>,
    api_stat_graph: Option<String>,
    api_stat_orientation: Option<String>,
    activity_project_limit: Option<usize>,
    display_style: Option<String>,
    #[serde(default)]
    skip_quit_confirmation: bool,
    last_workspace_path: Option<String>,
    history_project_view_mode: Option<String>,
    history_deep_depth: Option<u8>,
    #[serde(default)]
    history_selected_projects: Vec<String>,
    #[serde(default)]
    history_explicitly_excluded_projects: Vec<String>,
    #[serde(default)]
    history_expanded_remote_groups: Vec<String>,
    updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoredWorkspaceState {
    no_sessions_confirm_dismissed: bool,
    updated_at: i64,
}

pub async fn run(config: Config) -> Result<()> {
    let mut terminal = crate::ui::init_terminal()?;
    let result = run_inner(&mut terminal, config).await;
    crate::ui::restore_terminal(&mut terminal)?;
    result
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: Config,
) -> Result<()> {
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(64);
    let (evt_tx, mut evt_rx) = mpsc::channel::<AppEvent>(64);
    let (usage_refresh_tx, usage_refresh_rx) = mpsc::channel::<()>(1);
    let (limits_refresh_tx, limits_refresh_rx) = mpsc::channel::<()>(1);
    let (catalog_refresh_tx, catalog_refresh_rx) = mpsc::channel::<()>(1);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let restored_ui_state = load_persisted_ui_state_with_history_depth(
        &config.comon_home,
        config.workspace_path.as_deref(),
        config.history_deep_depth,
    )
    .unwrap_or_else(|_| {
        let mut defaults = PersistedUiState::default_for_workspace(config.workspace_path.clone());
        defaults.history_deep_depth = config.history_deep_depth;
        defaults
    });

    let scan_cache_db_path = config.comon_home.join("comon.db");
    if config.rebuild_cache_on_start {
        clear_scan_cache_files(&scan_cache_db_path)?;
    }
    let read_config = read::Config {
        sessions_dir: config.read_sessions_dir.clone(),
    };
    let mut read_browser = read::build_browser(&read_config)?;
    read_browser.restore_project_state(
        restored_ui_state.history_project_view_mode,
        restored_ui_state.history_deep_depth,
        restored_ui_state.history_selected_projects.clone(),
        restored_ui_state
            .history_explicitly_excluded_projects
            .clone(),
        restored_ui_state.history_expanded_remote_groups.clone(),
    );

    let history_catalog_max_depth = config
        .history_deep_max_depth
        .max(config.history_deep_depth)
        .clamp(1, crate::read::catalog::MAX_DEEP_DEPTH);
    let history_catalog_max_directories =
        crate::read::catalog::max_directories_for_candidates(config.history_catalog_max_candidates);

    // Load a cached HISTORY catalog on startup. Filesystem discovery waits for
    // an explicit user confirmation so normal startup never crawls project roots.
    {
        let evt_tx = evt_tx.clone();
        let sessions_dir = config.read_sessions_dir.clone();
        let search_roots = config.history_project_roots.clone();
        let excluded_roots = vec![
            config.codex_home.join("sessions"),
            config.comon_home.clone(),
        ];
        let max_depth = history_catalog_max_depth;
        let max_candidates = config.history_catalog_max_candidates;
        let progress_interval_ms = config.history_catalog_scan_budget_ms;
        let cache_db_path = scan_cache_db_path.clone();
        let catalog_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let cancelled = catalog_cancelled.clone();
            let mut cancel_shutdown_rx = shutdown_rx.clone();
            tokio::spawn(async move {
                if !*cancel_shutdown_rx.borrow() {
                    let _ = cancel_shutdown_rx.changed().await;
                }
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            });
        }
        let mut catalog_refresh_rx = catalog_refresh_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        let initial_strict = read_browser.strict_catalog_clone();
        tokio::spawn(async move {
            let initial_cache_path = cache_db_path.clone();
            let initial_cache = tokio::task::spawn_blocking(move || {
                crate::read::catalog::load_catalog_cache(&initial_cache_path)
            })
            .await;
            if let Ok(Ok(Some(snapshot))) = initial_cache {
                let _ = evt_tx
                    .send(AppEvent::HistoryCatalogUpdated {
                        strict: initial_strict,
                        snapshot,
                        from_cache: true,
                    })
                    .await;
            }
            let scan_config = crate::read::catalog::CatalogScanConfig {
                sessions_dir: sessions_dir.clone(),
                search_roots,
                excluded_roots,
                max_depth,
                max_candidates,
                progress_interval_ms,
                cache_db_path,
                cancelled: catalog_cancelled,
            };
            let mut catch_up = false;
            let mut reuse_cached_repositories = false;
            loop {
                if !catch_up {
                    tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        received = catalog_refresh_rx.recv() => {
                            if received.is_none() { break; }
                        }
                    }
                    reuse_cached_repositories = false;
                }
                let progress_tx = evt_tx.clone();
                let scan_config = scan_config.clone();
                let scan_cancelled = scan_config.cancelled.clone();
                let reuse_repositories = reuse_cached_repositories;
                let result = tokio::task::spawn_blocking(move || {
                    let strict = crate::read::scan::build_catalog(&scan_config.sessions_dir)?;
                    let report_progress = |progress| {
                        let _ =
                            progress_tx.blocking_send(AppEvent::HistoryCatalogProgress(progress));
                    };
                    let snapshot = if reuse_repositories {
                        crate::read::catalog::continue_project_catalog(
                            &scan_config,
                            &strict,
                            report_progress,
                        )?
                    } else {
                        crate::read::catalog::scan_project_catalog(
                            &scan_config,
                            &strict,
                            report_progress,
                        )?
                    };
                    Ok::<_, anyhow::Error>((strict, snapshot))
                })
                .await
                .unwrap_or_else(|error| Err(anyhow!("project catalog task failed: {error}")));
                match result {
                    Ok((strict, snapshot)) => {
                        catch_up = snapshot.sessions_scanned < snapshot.sessions_total;
                        reuse_cached_repositories = catch_up;
                        if evt_tx
                            .send(AppEvent::HistoryCatalogUpdated {
                                strict,
                                snapshot,
                                from_cache: false,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        if scan_cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        catch_up = false;
                        reuse_cached_repositories = false;
                        if evt_tx
                            .send(AppEvent::HistoryCatalogFailed(error.to_string()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Spawn usage worker.
    {
        let evt_tx = evt_tx.clone();
        let codex_home = config.codex_home.clone();
        let workspace_path = restored_ui_state.workspace_path.clone();
        let scan_cache_db_path = scan_cache_db_path.clone();
        let usage_days = config.usage_days;
        let usage_scan_limits = config.usage_scan_limits;
        let refresh = Duration::from_secs(config.refresh_usage_secs);
        let mut usage_refresh_rx = usage_refresh_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut first_run = true;
            let mut rapid_catch_up = false;
            let mut previous_scan_progress: Option<(usize, u64)> = None;
            loop {
                if !first_run {
                    let wait = if rapid_catch_up {
                        Duration::from_millis(75)
                    } else {
                        refresh
                    };
                    tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        _ = tokio::time::sleep(wait) => {}
                        recv = usage_refresh_rx.recv() => {
                            if recv.is_none() { break; }
                        }
                    }
                }
                first_run = false;
                let snapshot = tokio::task::spawn_blocking({
                    let codex_home = codex_home.clone();
                    let workspace_path = workspace_path.clone();
                    let scan_cache_db_path = scan_cache_db_path.clone();
                    move || {
                        crate::usage::compute_snapshot(
                            usage_days,
                            &codex_home,
                            workspace_path.as_deref(),
                            usage_scan_limits,
                            Some(scan_cache_db_path.as_path()),
                        )
                    }
                })
                .await
                .unwrap_or_else(|err| Err(anyhow!("usage snapshot task failed: {err}")));

                // Keep filling a fresh/incomplete cache without waiting for the normal
                // five-minute refresh. Stop the rapid loop as soon as a pass makes no
                // file-level progress; an unresolved fork must not cause a CPU spin.
                rapid_catch_up = snapshot.as_ref().is_ok_and(|current| {
                    should_continue_scan_catch_up(
                        current.scan_pending_files,
                        current.scan_indexed_files,
                        current.scan_processed_bytes,
                        previous_scan_progress,
                    )
                });
                if let Ok(current) = snapshot.as_ref() {
                    previous_scan_progress =
                        Some((current.scan_indexed_files, current.scan_processed_bytes));
                } else {
                    previous_scan_progress = None;
                }
                if evt_tx.send(AppEvent::UsageUpdated(snapshot)).await.is_err() {
                    break;
                }
            }
        });
    }

    // Spawn limits worker (Codex app-server).
    {
        let evt_tx = evt_tx.clone();
        let codex_bin = config.codex_bin.clone();
        let app_server_bin = config.app_server_bin.clone();
        let live_limits_mode = config.live_limits_mode;
        let cwd = config.cwd.clone();
        let refresh = Duration::from_secs(config.refresh_limits_secs);
        let mut limits_refresh_rx = limits_refresh_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if live_limits_mode == LiveLimitsMode::Off {
                let _ = evt_tx
                    .send(AppEvent::AccountApiUnavailable {
                        message: "Codex account API disabled (--live-limits off).".to_string(),
                        is_error: false,
                    })
                    .await;
                return;
            }

            let Some(app_server) =
                crate::codex_rpc::resolve_app_server_command(codex_bin, app_server_bin)
            else {
                let _ = evt_tx
                    .send(AppEvent::AccountApiUnavailable {
                        message: missing_app_server_message().to_string(),
                        is_error: live_limits_mode == LiveLimitsMode::On,
                    })
                    .await;
                return;
            };
            let app_server_is_explicit = app_server.is_explicit();

            let rpc = match CodexRpc::spawn(app_server, cwd).await {
                Ok(rpc) => rpc,
                Err(err) => {
                    let _ = evt_tx
                        .send(AppEvent::AccountApiUnavailable {
                            message: err.to_string(),
                            is_error: live_limits_mode == LiveLimitsMode::On
                                || app_server_is_explicit,
                        })
                        .await;
                    return;
                }
            };

            let mut interval = tokio::time::interval(refresh);
            // Immediate first poll
            let first = rpc.read_account_rate_limits().await;
            if evt_tx.send(AppEvent::LimitsUpdated(first)).await.is_err() {
                rpc.kill().await;
                return;
            }
            let first = rpc.read_account_usage().await;
            if evt_tx
                .send(AppEvent::AccountUsageUpdated(first))
                .await
                .is_err()
            {
                rpc.kill().await;
                return;
            }
            interval.tick().await;
            loop {
                let wake = tokio::select! {
                    _ = shutdown_rx.changed() => {
                        LimitsWake::Stop
                    }
                    _ = interval.tick() => {
                        LimitsWake::Poll
                    }
                    recv = limits_refresh_rx.recv() => {
                        if recv.is_none() {
                            LimitsWake::Stop
                        } else {
                            LimitsWake::Poll
                        }
                    }
                    notification = rpc.recv_notification() => {
                        match notification {
                            Some(value)
                                if crate::codex_rpc::is_account_rate_limits_updated_notification(&value) =>
                            {
                                LimitsWake::Poll
                            }
                            Some(_) => continue,
                            None => LimitsWake::Stop,
                        }
                    }
                };
                if wake == LimitsWake::Stop {
                    rpc.kill().await;
                    break;
                }
                let res = rpc.read_account_rate_limits().await;
                if evt_tx.send(AppEvent::LimitsUpdated(res)).await.is_err() {
                    rpc.kill().await;
                    break;
                }
                let res = rpc.read_account_usage().await;
                if evt_tx
                    .send(AppEvent::AccountUsageUpdated(res))
                    .await
                    .is_err()
                {
                    rpc.kill().await;
                    break;
                }
            }
        });
    }

    // Spawn input reader.
    {
        let input_tx = input_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let shutdown_rx = shutdown_rx;
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                let event = tokio::task::spawn_blocking(|| {
                    let timeout = Duration::from_millis(100);
                    if crossterm::event::poll(timeout).ok()? {
                        crossterm::event::read().ok()
                    } else {
                        None
                    }
                })
                .await
                .ok()
                .flatten();

                let Some(event) = event else { continue };

                if input_tx.send(event).await.is_err() {
                    break;
                }
            }
        });
    }

    let mut state = AppState {
        active_screen: if config.start_in_read_screen {
            ActiveScreen::Read
        } else {
            ActiveScreen::Usage
        },
        metric: restored_ui_state.metric,
        range: restored_ui_state.range,
        usage_zone: restored_ui_state.usage_zone,
        orientation: restored_ui_state.orientation,
        api_stat_grouping: restored_ui_state.api_stat_grouping,
        api_stat_graph: restored_ui_state.api_stat_graph,
        api_stat_orientation: restored_ui_state.api_stat_orientation,
        usage_period_offset: 0,
        usage_visible_periods: 0,
        usage_total_periods: 0,
        usage_scroll_area: None,
        api_stat_period_offset: 0,
        api_stat_visible_periods: 0,
        api_stat_total_periods: 0,
        api_stat_scroll_area: None,
        activity_project_limit: restored_ui_state.activity_project_limit,
        activity_week_offset: 0,
        activity_visible_weeks: 0,
        activity_total_weeks: crate::usage::ACTIVITY_TIMELINE_WEEKS,
        activity_scroll_area: None,
        show_help: false,
        workspace_path: restored_ui_state.workspace_path.clone(),
        no_sessions_confirm_open: false,
        no_sessions_confirm_dismissed: restored_ui_state.no_sessions_confirm_dismissed,
        quit_confirm_open: false,
        quit_confirm_yes_selected: false,
        quit_dont_ask_again: false,
        skip_quit_confirmation: restored_ui_state.skip_quit_confirmation,
        quit_preference_prompt: None,
        history_project_roots: config.history_project_roots.clone(),
        history_catalog_max_depth,
        history_catalog_max_directories,
        history_catalog_config_path: config.comon_home.join("config.json"),
        history_catalog_scan_prompt: false,
        display_style: restored_ui_state.display_style,
        accent_theme: AccentTheme::default(),
        bar_fill_mode: BarFillMode::default(),
        system_locale: config.system_locale.clone(),
        mouse_position: None,
        ui_hit_targets: Vec::new(),
        usage: None,
        usage_updated_at: None,
        usage_error: None,
        limits: None,
        limits_updated_at: None,
        limits_error: None,
        limits_notice: None,
        limits_enabled: true,
        account_usage: None,
        account_usage_updated_at: None,
        account_usage_error: None,
        account_usage_notice: None,
        account_usage_enabled: true,
        read_browser,
    };

    // Initial draw.
    terminal.draw(|f| crate::ui::render(f, &mut state))?;

    let mut last_redraw = Instant::now();
    let min_redraw = Duration::from_millis(50);
    let mut last_observed_ui_state = PersistedUiState::from_app_state(&state);
    let mut last_saved_ui_state = last_observed_ui_state.clone();
    let mut state_changed_at: Option<Instant> = None;

    loop {
        let mut dirty = false;

        tokio::select! {
            input = input_rx.recv() => {
                if let Some(input) = input {
                    match handle_input_event(
                        &mut state,
                        input,
                        &usage_refresh_tx,
                        &limits_refresh_tx,
                        &catalog_refresh_tx,
                    )? {
                        InputOutcome::Continue(should_redraw) => {
                            dirty |= should_redraw;
                        }
                        InputOutcome::Quit => {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                    }
                }
            }
            evt = evt_rx.recv() => {
                if let Some(evt) = evt {
                    dirty |= handle_app_event(&mut state, evt);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // periodic UI tick: allow relative-time labels to update
                dirty = true;
            }
        }

        if dirty && last_redraw.elapsed() >= min_redraw {
            terminal.draw(|f| crate::ui::render(f, &mut state))?;
            last_redraw = Instant::now();
        }

        let current_ui_state = PersistedUiState::from_app_state(&state);
        if current_ui_state != last_observed_ui_state {
            last_observed_ui_state = current_ui_state.clone();
            state_changed_at = Some(Instant::now());
        }
        if current_ui_state != last_saved_ui_state
            && state_changed_at
                .map(|changed_at| changed_at.elapsed() >= STATE_SAVE_DEBOUNCE)
                .unwrap_or(true)
        {
            let _ = save_persisted_ui_state(&config.comon_home, &current_ui_state);
            last_saved_ui_state = current_ui_state;
        }
    }

    let final_ui_state = PersistedUiState::from_app_state(&state);
    let _ = save_persisted_ui_state(&config.comon_home, &final_ui_state);

    Ok(())
}

fn should_continue_scan_catch_up(
    pending_files: usize,
    indexed_files: usize,
    processed_bytes: u64,
    previous: Option<(usize, u64)>,
) -> bool {
    pending_files > 0
        && previous.is_none_or(|(files, bytes)| indexed_files > files || processed_bytes > bytes)
}

fn clear_scan_cache_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate_os = path.as_os_str().to_os_string();
        candidate_os.push(suffix);
        let candidate = PathBuf::from(candidate_os);

        let meta = match std::fs::symlink_metadata(&candidate) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Unable to inspect scan cache file {}", candidate.display())
                });
            }
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            anyhow::bail!(
                "Refusing to rebuild cache: symlink is not allowed ({})",
                candidate.display()
            );
        }
        if !ft.is_file() {
            anyhow::bail!(
                "Refusing to rebuild cache: expected regular file ({})",
                candidate.display()
            );
        }
        std::fs::remove_file(&candidate)
            .with_context(|| format!("Unable to remove scan cache file {}", candidate.display()))?;
    }

    Ok(())
}

fn handle_input_event(
    state: &mut AppState,
    event: Event,
    usage_refresh_tx: &mpsc::Sender<()>,
    limits_refresh_tx: &mpsc::Sender<()>,
    catalog_refresh_tx: &mpsc::Sender<()>,
) -> Result<InputOutcome> {
    if state.history_catalog_scan_prompt {
        return match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                match ui_click_action_at(&state.ui_hit_targets, mouse.column, mouse.row) {
                    Some(UiClickAction::ConfirmHistoryCatalogScan) => {
                        state.history_catalog_scan_prompt = false;
                        if catalog_refresh_tx.try_send(()).is_err() {
                            state.read_browser.set_catalog_notice(
                                "Repository discovery is already queued or running.".to_string(),
                            );
                        }
                        Ok(InputOutcome::Continue(true))
                    }
                    Some(UiClickAction::CancelHistoryCatalogScan) => {
                        state.history_catalog_scan_prompt = false;
                        Ok(InputOutcome::Continue(true))
                    }
                    _ => Ok(InputOutcome::Continue(false)),
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    state.history_catalog_scan_prompt = false;
                    if catalog_refresh_tx.try_send(()).is_err() {
                        state.read_browser.set_catalog_notice(
                            "Repository discovery is already queued or running.".to_string(),
                        );
                    }
                    Ok(InputOutcome::Continue(true))
                }
                KeyCode::Esc
                | KeyCode::Char('n')
                | KeyCode::Char('N')
                | KeyCode::Char('q')
                | KeyCode::Char('Q') => {
                    state.history_catalog_scan_prompt = false;
                    Ok(InputOutcome::Continue(true))
                }
                KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                    Ok(InputOutcome::Quit)
                }
                _ => Ok(InputOutcome::Continue(false)),
            },
            Event::Resize(_, _) => Ok(InputOutcome::Continue(true)),
            _ => Ok(InputOutcome::Continue(false)),
        };
    }

    if let Some(desired_skip_confirmation) = state.quit_preference_prompt {
        return match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                match ui_click_action_at(&state.ui_hit_targets, mouse.column, mouse.row) {
                    Some(UiClickAction::ConfirmQuitConfirmationPreference) => {
                        state.skip_quit_confirmation = desired_skip_confirmation;
                        state.quit_preference_prompt = None;
                        Ok(InputOutcome::Continue(true))
                    }
                    Some(UiClickAction::CancelQuitConfirmationPreference) => {
                        state.quit_preference_prompt = None;
                        Ok(InputOutcome::Continue(true))
                    }
                    _ => Ok(InputOutcome::Continue(false)),
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match (key.code, key.modifiers) {
                (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
                    state.skip_quit_confirmation = desired_skip_confirmation;
                    state.quit_preference_prompt = None;
                    Ok(InputOutcome::Continue(true))
                }
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => Ok(InputOutcome::Quit),
                (KeyCode::Enter, _)
                | (KeyCode::Esc, _)
                | (KeyCode::Char('n'), _)
                | (KeyCode::Char('N'), _)
                | (KeyCode::Char('q'), _)
                | (KeyCode::Char('Q'), _) => {
                    state.quit_preference_prompt = None;
                    Ok(InputOutcome::Continue(true))
                }
                _ => Ok(InputOutcome::Continue(false)),
            },
            Event::Resize(_, _) => Ok(InputOutcome::Continue(true)),
            _ => Ok(InputOutcome::Continue(false)),
        };
    }

    if state.quit_confirm_open {
        return match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                match ui_click_action_at(&state.ui_hit_targets, mouse.column, mouse.row) {
                    Some(UiClickAction::ConfirmQuit) => {
                        state.skip_quit_confirmation = state.quit_dont_ask_again;
                        Ok(InputOutcome::Quit)
                    }
                    Some(UiClickAction::CancelQuit) => {
                        state.quit_confirm_open = false;
                        state.quit_confirm_yes_selected = false;
                        state.quit_dont_ask_again = false;
                        Ok(InputOutcome::Continue(true))
                    }
                    Some(UiClickAction::ToggleQuitDontAskAgain) => {
                        state.quit_dont_ask_again = !state.quit_dont_ask_again;
                        Ok(InputOutcome::Continue(true))
                    }
                    _ => Ok(InputOutcome::Continue(false)),
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match quit_confirmation_command(
                    key.code,
                    key.modifiers,
                    state.quit_confirm_yes_selected,
                ) {
                    Some(QuitConfirmationCommand::Confirm) => {
                        state.skip_quit_confirmation = state.quit_dont_ask_again;
                        Ok(InputOutcome::Quit)
                    }
                    Some(QuitConfirmationCommand::QuitImmediately) => Ok(InputOutcome::Quit),
                    Some(QuitConfirmationCommand::ToggleDontAskAgain) => {
                        state.quit_dont_ask_again = !state.quit_dont_ask_again;
                        Ok(InputOutcome::Continue(true))
                    }
                    Some(QuitConfirmationCommand::SelectYes) => {
                        state.quit_confirm_yes_selected = true;
                        Ok(InputOutcome::Continue(true))
                    }
                    Some(QuitConfirmationCommand::SelectNo) => {
                        state.quit_confirm_yes_selected = false;
                        Ok(InputOutcome::Continue(true))
                    }
                    Some(QuitConfirmationCommand::Cancel) => {
                        state.quit_confirm_open = false;
                        state.quit_confirm_yes_selected = false;
                        state.quit_dont_ask_again = false;
                        Ok(InputOutcome::Continue(true))
                    }
                    None => Ok(InputOutcome::Continue(false)),
                }
            }
            Event::Resize(_, _) => Ok(InputOutcome::Continue(true)),
            _ => Ok(InputOutcome::Continue(false)),
        };
    }

    if let Event::Mouse(mouse) = &event {
        let mouse_position = Some((mouse.column, mouse.row));
        let mouse_moved = state.mouse_position != mouse_position;
        state.mouse_position = mouse_position;

        if state.show_help || state.no_sessions_confirm_open {
            return Ok(InputOutcome::Continue(false));
        }
        let wheel_direction = match mouse.kind {
            MouseEventKind::ScrollUp => Some(true),
            MouseEventKind::ScrollDown => Some(false),
            _ => None,
        };
        if let Some(older) = wheel_direction {
            if state.active_screen == ActiveScreen::Read
                && ui_click_action_at(&state.ui_hit_targets, mouse.column, mouse.row)
                    == Some(UiClickAction::HistoryDepthWheel)
            {
                let changed = state
                    .read_browser
                    .change_deep_depth(if older { 1 } else { -1 });
                return Ok(InputOutcome::Continue(changed));
            }
            let changed = match state.active_screen {
                ActiveScreen::Usage
                    if state
                        .usage_scroll_area
                        .is_some_and(|area| rect_contains(area, mouse.column, mouse.row)) =>
                {
                    let command = if older {
                        UsageCommand::ScrollOlder
                    } else {
                        UsageCommand::ScrollNewer
                    };
                    handle_usage_command(state, command, usage_refresh_tx, limits_refresh_tx)
                }
                ActiveScreen::Activity
                    if state
                        .activity_scroll_area
                        .is_some_and(|area| rect_contains(area, mouse.column, mouse.row)) =>
                {
                    let command = if older {
                        ActivityCommand::ScrollOlder
                    } else {
                        ActivityCommand::ScrollNewer
                    };
                    handle_activity_command(state, command, usage_refresh_tx, limits_refresh_tx)
                }
                ActiveScreen::ApiStat
                    if state
                        .api_stat_scroll_area
                        .is_some_and(|area| rect_contains(area, mouse.column, mouse.row)) =>
                {
                    let command = if older {
                        ApiStatCommand::ScrollOlder
                    } else {
                        ApiStatCommand::ScrollNewer
                    };
                    handle_api_stat_command(state, command, limits_refresh_tx)
                }
                _ => false,
            };
            if changed {
                return Ok(InputOutcome::Continue(true));
            }
        }
        if matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
        ) {
            if let Some(action) = ui_click_action_at(&state.ui_hit_targets, mouse.column, mouse.row)
            {
                if action == UiClickAction::ConfirmQuit
                    || (action == UiClickAction::PromptQuit && state.skip_quit_confirmation)
                {
                    return Ok(InputOutcome::Quit);
                }
                let changed = apply_ui_click_action(state, action);
                return Ok(InputOutcome::Continue(changed));
            }
        }
        if mouse.kind == MouseEventKind::Moved {
            return Ok(InputOutcome::Continue(mouse_moved));
        }
    }

    if let Event::Key(key) = &event {
        if key.kind != KeyEventKind::Press {
            return Ok(InputOutcome::Continue(false));
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('Q'), _) => {
                if state.skip_quit_confirmation {
                    return Ok(InputOutcome::Quit);
                }
                state.quit_confirm_open = true;
                state.quit_confirm_yes_selected = false;
                state.quit_dont_ask_again = false;
                return Ok(InputOutcome::Continue(true));
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(InputOutcome::Quit),
            (KeyCode::Char('s'), _) | (KeyCode::Char('S'), _) | (KeyCode::F(2), _) => {
                state.active_screen = next_active_screen(state.active_screen);
                return Ok(InputOutcome::Continue(true));
            }
            (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
                state.display_style = state.display_style.toggled();
                return Ok(InputOutcome::Continue(true));
            }
            (KeyCode::Char('c'), _) | (KeyCode::Char('C'), _) => {
                state.accent_theme = state.accent_theme.cycled();
                return Ok(InputOutcome::Continue(true));
            }
            (KeyCode::Char('r'), _) | (KeyCode::F(5), _)
                if state.active_screen == ActiveScreen::Read =>
            {
                return Ok(InputOutcome::Continue(request_history_catalog_scan(state)));
            }
            _ => {}
        }
    }

    if matches!(event, Event::Resize(_, _)) {
        state.mouse_position = None;
        return Ok(InputOutcome::Continue(true));
    }

    match state.active_screen {
        ActiveScreen::Usage => {
            let Some(command) = map_event_to_usage_cmd(event) else {
                return Ok(InputOutcome::Continue(false));
            };
            let dirty = handle_usage_command(state, command, usage_refresh_tx, limits_refresh_tx);
            Ok(InputOutcome::Continue(dirty))
        }
        ActiveScreen::Activity => {
            let Some(command) = map_event_to_activity_cmd(event) else {
                return Ok(InputOutcome::Continue(false));
            };
            let dirty =
                handle_activity_command(state, command, usage_refresh_tx, limits_refresh_tx);
            Ok(InputOutcome::Continue(dirty))
        }
        ActiveScreen::ApiStat => {
            let Some(command) = map_event_to_api_stat_cmd(event) else {
                return Ok(InputOutcome::Continue(false));
            };
            let dirty = handle_api_stat_command(state, command, limits_refresh_tx);
            Ok(InputOutcome::Continue(dirty))
        }
        ActiveScreen::LimitResets => {
            let Some(command) = map_event_to_limit_reset_cmd(event) else {
                return Ok(InputOutcome::Continue(false));
            };
            let dirty = handle_limit_reset_command(state, command, limits_refresh_tx);
            Ok(InputOutcome::Continue(dirty))
        }
        ActiveScreen::Read => Ok(InputOutcome::Continue(read::tui::handle_event(
            &mut state.read_browser,
            event,
        )?)),
    }
}

fn quit_confirmation_command(
    code: KeyCode,
    modifiers: KeyModifiers,
    yes_selected: bool,
) -> Option<QuitConfirmationCommand> {
    if code == KeyCode::Char('c') && modifiers == KeyModifiers::CONTROL {
        return Some(QuitConfirmationCommand::QuitImmediately);
    }

    match code {
        KeyCode::Left => Some(QuitConfirmationCommand::SelectYes),
        KeyCode::Right => Some(QuitConfirmationCommand::SelectNo),
        KeyCode::Enter if yes_selected => Some(QuitConfirmationCommand::Confirm),
        KeyCode::Enter => Some(QuitConfirmationCommand::Cancel),
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(QuitConfirmationCommand::Confirm),
        KeyCode::Char(' ') => Some(QuitConfirmationCommand::ToggleDontAskAgain),
        KeyCode::Esc
        | KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char('q')
        | KeyCode::Char('Q') => Some(QuitConfirmationCommand::Cancel),
        _ => None,
    }
}

fn ui_click_action_at(targets: &[UiHitTarget], column: u16, row: u16) -> Option<UiClickAction> {
    targets
        .iter()
        .find(|target| target.contains(column, row))
        .map(|target| target.action)
}

fn request_history_catalog_scan(state: &mut AppState) -> bool {
    if state.history_project_roots.is_empty() {
        state.read_browser.set_catalog_notice(format!(
            "Repository discovery is disabled. Add history_project_roots to {}.",
            state.history_catalog_config_path.display()
        ));
        return true;
    }
    state.history_catalog_scan_prompt = true;
    true
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn apply_ui_click_action(state: &mut AppState, action: UiClickAction) -> bool {
    match action {
        UiClickAction::SetScreen(screen) => {
            let changed = state.active_screen != screen;
            state.active_screen = screen;
            changed
        }
        UiClickAction::SetDisplayStyle(style) => {
            let changed = state.display_style != style;
            state.display_style = style;
            changed
        }
        UiClickAction::SetAccentTheme(theme) => {
            let changed = state.accent_theme != theme;
            state.accent_theme = theme;
            changed
        }
        UiClickAction::ToggleBarFillMode => {
            state.bar_fill_mode = state.bar_fill_mode.toggled();
            true
        }
        UiClickAction::SetHistoryProjectMode(mode) => state.read_browser.set_project_mode(mode),
        UiClickAction::ConfirmHistoryCatalogScan | UiClickAction::CancelHistoryCatalogScan => false,
        UiClickAction::DecreaseHistoryDepth => state.read_browser.change_deep_depth(-1),
        UiClickAction::IncreaseHistoryDepth => state.read_browser.change_deep_depth(1),
        UiClickAction::HistoryDepthWheel => false,
        UiClickAction::SetMetric(metric) => {
            let changed = state.metric != metric;
            state.metric = metric;
            changed
        }
        UiClickAction::SetRange(range) => {
            let changed = state.range != range;
            state.range = range;
            if changed {
                state.usage_period_offset = 0;
            }
            changed
        }
        UiClickAction::SetUsageZone(zone) => {
            let changed = state.usage_zone != zone;
            state.usage_zone = zone;
            if changed {
                state.usage_period_offset = 0;
            }
            changed
        }
        UiClickAction::SetOrientation(orientation) => {
            let changed = state.orientation != orientation;
            state.orientation = orientation;
            if changed {
                state.usage_period_offset = 0;
            }
            changed
        }
        UiClickAction::SetApiStatGrouping(grouping) => {
            let changed = state.api_stat_grouping != grouping;
            state.api_stat_grouping = grouping;
            if changed {
                state.api_stat_period_offset = 0;
            }
            changed
        }
        UiClickAction::SetApiStatGraph(graph) => {
            let changed = state.api_stat_graph != graph;
            state.api_stat_graph = graph;
            if changed {
                state.api_stat_period_offset = 0;
            }
            changed
        }
        UiClickAction::SetApiStatOrientation(orientation) => {
            let changed = state.api_stat_orientation != orientation;
            state.api_stat_orientation = orientation;
            if changed {
                state.api_stat_period_offset = 0;
            }
            changed
        }
        UiClickAction::ScrollUsageOlder => scroll_usage_periods(state, 1, true),
        UiClickAction::ScrollUsageNewer => scroll_usage_periods(state, 1, false),
        UiClickAction::SetUsageScrollOffset(offset) => {
            let next = offset.min(
                state
                    .usage_total_periods
                    .saturating_sub(state.usage_visible_periods),
            );
            let changed = state.usage_period_offset != next;
            state.usage_period_offset = next;
            changed
        }
        UiClickAction::ScrollApiStatOlder => scroll_api_stat_periods(state, 1, true),
        UiClickAction::ScrollApiStatNewer => scroll_api_stat_periods(state, 1, false),
        UiClickAction::SetApiStatScrollOffset(offset) => {
            let next = offset.min(
                state
                    .api_stat_total_periods
                    .saturating_sub(state.api_stat_visible_periods),
            );
            let changed = state.api_stat_period_offset != next;
            state.api_stat_period_offset = next;
            changed
        }
        UiClickAction::ScrollActivityOlder => scroll_activity_weeks(state, 1, true),
        UiClickAction::ScrollActivityNewer => scroll_activity_weeks(state, 1, false),
        UiClickAction::SetActivityScrollOffset(offset) => {
            let next = offset.min(
                state
                    .activity_total_weeks
                    .saturating_sub(state.activity_visible_weeks),
            );
            let changed = state.activity_week_offset != next;
            state.activity_week_offset = next;
            changed
        }
        UiClickAction::DecreaseProjects => {
            let next = state
                .activity_project_limit
                .saturating_sub(1)
                .max(MIN_ACTIVITY_PROJECT_LIMIT);
            let changed = next != state.activity_project_limit;
            state.activity_project_limit = next;
            changed
        }
        UiClickAction::IncreaseProjects => {
            let next = state
                .activity_project_limit
                .saturating_add(1)
                .min(MAX_ACTIVITY_PROJECT_LIMIT);
            let changed = next != state.activity_project_limit;
            state.activity_project_limit = next;
            changed
        }
        UiClickAction::PromptQuit => {
            state.quit_confirm_open = true;
            state.quit_confirm_yes_selected = false;
            state.quit_dont_ask_again = false;
            true
        }
        UiClickAction::CancelQuit => {
            state.quit_confirm_open = false;
            state.quit_confirm_yes_selected = false;
            state.quit_dont_ask_again = false;
            true
        }
        UiClickAction::ConfirmQuit => {
            state.skip_quit_confirmation = state.quit_dont_ask_again;
            false
        }
        UiClickAction::ToggleQuitDontAskAgain => {
            state.quit_dont_ask_again = !state.quit_dont_ask_again;
            true
        }
        UiClickAction::PromptQuitConfirmationPreference => {
            state.quit_preference_prompt = Some(!state.skip_quit_confirmation);
            true
        }
        UiClickAction::ConfirmQuitConfirmationPreference => {
            if let Some(desired_skip_confirmation) = state.quit_preference_prompt.take() {
                state.skip_quit_confirmation = desired_skip_confirmation;
            }
            true
        }
        UiClickAction::CancelQuitConfirmationPreference => {
            state.quit_preference_prompt = None;
            true
        }
    }
}

fn map_event_to_usage_cmd(event: Event) -> Option<UsageCommand> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return None;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('r'), _) | (KeyCode::F(5), _) => Some(UsageCommand::RefreshAll),
                (KeyCode::Tab, _) => Some(UsageCommand::ToggleMetric),
                (KeyCode::Char('g'), _)
                | (KeyCode::Char('G'), _)
                | (KeyCode::Char('w'), _)
                | (KeyCode::Char('W'), _) => Some(UsageCommand::ToggleRange),
                (KeyCode::Char('z'), _) | (KeyCode::Char('Z'), _) | (KeyCode::F(6), _) => {
                    Some(UsageCommand::ToggleZone)
                }
                (KeyCode::Char('f'), _) => Some(UsageCommand::ToggleOrientation),
                (KeyCode::Left, _) | (KeyCode::Up, _) => Some(UsageCommand::ScrollOlder),
                (KeyCode::Right, _) | (KeyCode::Down, _) => Some(UsageCommand::ScrollNewer),
                (KeyCode::PageUp, _) => Some(UsageCommand::PageOlder),
                (KeyCode::PageDown, _) => Some(UsageCommand::PageNewer),
                (KeyCode::Home, _) => Some(UsageCommand::ScrollOldest),
                (KeyCode::End, _) => Some(UsageCommand::ScrollNewest),
                (KeyCode::Char('?'), _) => Some(UsageCommand::ToggleHelp),
                (KeyCode::Enter, _) => Some(UsageCommand::ConfirmContinue),
                (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
                    Some(UsageCommand::ConfirmContinue)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn map_event_to_activity_cmd(event: Event) -> Option<ActivityCommand> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return None;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('r'), _) | (KeyCode::F(5), _) => Some(ActivityCommand::RefreshAll),
                (KeyCode::Tab, _) => Some(ActivityCommand::ToggleMetric),
                (KeyCode::Char('+'), _) | (KeyCode::Char('='), _) | (KeyCode::Char(']'), _) => {
                    Some(ActivityCommand::IncreaseProjects)
                }
                (KeyCode::Char('-'), _) | (KeyCode::Char('['), _) => {
                    Some(ActivityCommand::DecreaseProjects)
                }
                (KeyCode::Left, _) => Some(ActivityCommand::ScrollOlder),
                (KeyCode::Right, _) => Some(ActivityCommand::ScrollNewer),
                (KeyCode::PageUp, _) => Some(ActivityCommand::PageOlder),
                (KeyCode::PageDown, _) => Some(ActivityCommand::PageNewer),
                (KeyCode::Home, _) => Some(ActivityCommand::ScrollOldest),
                (KeyCode::End, _) => Some(ActivityCommand::ScrollNewest),
                (KeyCode::Char('?'), _) => Some(ActivityCommand::ToggleHelp),
                _ => None,
            }
        }
        _ => None,
    }
}

fn map_event_to_limit_reset_cmd(event: Event) -> Option<LimitResetCommand> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return None;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('r'), _) | (KeyCode::F(5), _) => {
                    Some(LimitResetCommand::RefreshLimits)
                }
                (KeyCode::Char('?'), _) => Some(LimitResetCommand::ToggleHelp),
                _ => None,
            }
        }
        _ => None,
    }
}

fn map_event_to_api_stat_cmd(event: Event) -> Option<ApiStatCommand> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match (key.code, key.modifiers) {
            (KeyCode::Char('r'), _) | (KeyCode::F(5), _) => Some(ApiStatCommand::Refresh),
            (KeyCode::Char('g'), _) | (KeyCode::Char('G'), _) => {
                Some(ApiStatCommand::ToggleGrouping)
            }
            (KeyCode::Char('b'), _) | (KeyCode::Char('B'), _) => Some(ApiStatCommand::ToggleGraph),
            (KeyCode::Char('f'), _) | (KeyCode::Char('F'), _) => {
                Some(ApiStatCommand::ToggleOrientation)
            }
            (KeyCode::Left, _) | (KeyCode::Up, _) => Some(ApiStatCommand::ScrollOlder),
            (KeyCode::Right, _) | (KeyCode::Down, _) => Some(ApiStatCommand::ScrollNewer),
            (KeyCode::PageUp, _) => Some(ApiStatCommand::PageOlder),
            (KeyCode::PageDown, _) => Some(ApiStatCommand::PageNewer),
            (KeyCode::Home, _) => Some(ApiStatCommand::ScrollOldest),
            (KeyCode::End, _) => Some(ApiStatCommand::ScrollNewest),
            (KeyCode::Char('?'), _) => Some(ApiStatCommand::ToggleHelp),
            _ => None,
        },
        _ => None,
    }
}

fn handle_usage_command(
    state: &mut AppState,
    cmd: UsageCommand,
    usage_refresh_tx: &mpsc::Sender<()>,
    limits_refresh_tx: &mpsc::Sender<()>,
) -> bool {
    if state.no_sessions_confirm_open {
        match cmd {
            UsageCommand::ConfirmContinue => {
                state.no_sessions_confirm_open = false;
                state.no_sessions_confirm_dismissed = true;
                return true;
            }
            _ => return false,
        }
    }
    match cmd {
        UsageCommand::ToggleHelp => {
            state.show_help = !state.show_help;
            true
        }
        UsageCommand::ConfirmContinue => false,
        UsageCommand::ToggleMetric => {
            state.metric = match state.metric {
                UsageMetric::Tokens => UsageMetric::Time,
                UsageMetric::Time => UsageMetric::Runs,
                UsageMetric::Runs => UsageMetric::Tokens,
            };
            true
        }
        UsageCommand::ToggleRange => {
            state.range = state.range.toggled();
            state.usage_period_offset = 0;
            true
        }
        UsageCommand::ToggleZone => {
            state.usage_zone = state.usage_zone.toggled();
            state.usage_period_offset = 0;
            true
        }
        UsageCommand::ToggleOrientation => {
            state.orientation = match state.orientation {
                ChartOrientation::Horizontal => ChartOrientation::Vertical,
                ChartOrientation::Vertical => ChartOrientation::Horizontal,
            };
            state.usage_period_offset = 0;
            true
        }
        UsageCommand::ScrollOlder => scroll_usage_periods(state, 1, true),
        UsageCommand::ScrollNewer => scroll_usage_periods(state, 1, false),
        UsageCommand::PageOlder => {
            scroll_usage_periods(state, state.usage_visible_periods.max(1), true)
        }
        UsageCommand::PageNewer => {
            scroll_usage_periods(state, state.usage_visible_periods.max(1), false)
        }
        UsageCommand::ScrollOldest => {
            let next = state
                .usage_total_periods
                .saturating_sub(state.usage_visible_periods);
            let changed = state.usage_period_offset != next;
            state.usage_period_offset = next;
            changed
        }
        UsageCommand::ScrollNewest => {
            let changed = state.usage_period_offset != 0;
            state.usage_period_offset = 0;
            changed
        }
        UsageCommand::RefreshAll => {
            let _ = usage_refresh_tx.try_send(());
            let _ = limits_refresh_tx.try_send(());
            true
        }
    }
}

fn scroll_usage_periods(state: &mut AppState, amount: usize, older: bool) -> bool {
    let next = scrolled_period_offset(
        state.usage_period_offset,
        state.usage_total_periods,
        state.usage_visible_periods,
        amount,
        older,
    );
    let changed = next != state.usage_period_offset;
    state.usage_period_offset = next;
    changed
}

fn scrolled_period_offset(
    current: usize,
    total: usize,
    visible: usize,
    amount: usize,
    older: bool,
) -> usize {
    let max_offset = total.saturating_sub(visible);
    if older {
        current.saturating_add(amount).min(max_offset)
    } else {
        current.saturating_sub(amount)
    }
}

fn handle_activity_command(
    state: &mut AppState,
    cmd: ActivityCommand,
    usage_refresh_tx: &mpsc::Sender<()>,
    limits_refresh_tx: &mpsc::Sender<()>,
) -> bool {
    match cmd {
        ActivityCommand::ToggleHelp => {
            state.show_help = !state.show_help;
            true
        }
        ActivityCommand::ToggleMetric => {
            state.metric = match state.metric {
                UsageMetric::Tokens => UsageMetric::Time,
                UsageMetric::Time => UsageMetric::Runs,
                UsageMetric::Runs => UsageMetric::Tokens,
            };
            true
        }
        ActivityCommand::IncreaseProjects => {
            let next = state
                .activity_project_limit
                .saturating_add(1)
                .min(MAX_ACTIVITY_PROJECT_LIMIT);
            let changed = next != state.activity_project_limit;
            state.activity_project_limit = next;
            changed
        }
        ActivityCommand::DecreaseProjects => {
            let next = state
                .activity_project_limit
                .saturating_sub(1)
                .max(MIN_ACTIVITY_PROJECT_LIMIT);
            let changed = next != state.activity_project_limit;
            state.activity_project_limit = next;
            changed
        }
        ActivityCommand::ScrollOlder => scroll_activity_weeks(state, 1, true),
        ActivityCommand::ScrollNewer => scroll_activity_weeks(state, 1, false),
        ActivityCommand::PageOlder => {
            scroll_activity_weeks(state, state.activity_visible_weeks.max(1), true)
        }
        ActivityCommand::PageNewer => {
            scroll_activity_weeks(state, state.activity_visible_weeks.max(1), false)
        }
        ActivityCommand::ScrollOldest => {
            let next = state
                .activity_total_weeks
                .saturating_sub(state.activity_visible_weeks);
            let changed = next != state.activity_week_offset;
            state.activity_week_offset = next;
            changed
        }
        ActivityCommand::ScrollNewest => {
            let changed = state.activity_week_offset != 0;
            state.activity_week_offset = 0;
            changed
        }
        ActivityCommand::RefreshAll => {
            let _ = usage_refresh_tx.try_send(());
            let _ = limits_refresh_tx.try_send(());
            true
        }
    }
}

fn scroll_activity_weeks(state: &mut AppState, amount: usize, older: bool) -> bool {
    let max_offset = state
        .activity_total_weeks
        .saturating_sub(state.activity_visible_weeks);
    let next = if older {
        state
            .activity_week_offset
            .saturating_add(amount)
            .min(max_offset)
    } else {
        state.activity_week_offset.saturating_sub(amount)
    };
    let changed = next != state.activity_week_offset;
    state.activity_week_offset = next;
    changed
}

fn handle_limit_reset_command(
    state: &mut AppState,
    cmd: LimitResetCommand,
    limits_refresh_tx: &mpsc::Sender<()>,
) -> bool {
    match cmd {
        LimitResetCommand::ToggleHelp => {
            state.show_help = !state.show_help;
            true
        }
        LimitResetCommand::RefreshLimits => {
            let _ = limits_refresh_tx.try_send(());
            true
        }
    }
}

fn handle_api_stat_command(
    state: &mut AppState,
    cmd: ApiStatCommand,
    account_refresh_tx: &mpsc::Sender<()>,
) -> bool {
    match cmd {
        ApiStatCommand::ToggleHelp => {
            state.show_help = !state.show_help;
            true
        }
        ApiStatCommand::Refresh => {
            let _ = account_refresh_tx.try_send(());
            true
        }
        ApiStatCommand::ToggleGrouping => {
            if state.api_stat_graph == ApiStatGraph::Heat {
                return false;
            }
            state.api_stat_grouping = state.api_stat_grouping.toggled();
            state.api_stat_period_offset = 0;
            true
        }
        ApiStatCommand::ToggleGraph => {
            state.api_stat_graph = match state.api_stat_graph {
                ApiStatGraph::Bars => ApiStatGraph::Heat,
                ApiStatGraph::Heat => ApiStatGraph::Bars,
            };
            state.api_stat_period_offset = 0;
            true
        }
        ApiStatCommand::ToggleOrientation => {
            if state.api_stat_graph == ApiStatGraph::Heat {
                return false;
            }
            state.api_stat_orientation = match state.api_stat_orientation {
                ChartOrientation::Vertical => ChartOrientation::Horizontal,
                ChartOrientation::Horizontal => ChartOrientation::Vertical,
            };
            state.api_stat_period_offset = 0;
            true
        }
        ApiStatCommand::ScrollOlder => scroll_api_stat_periods(state, 1, true),
        ApiStatCommand::ScrollNewer => scroll_api_stat_periods(state, 1, false),
        ApiStatCommand::PageOlder => {
            scroll_api_stat_periods(state, state.api_stat_visible_periods.max(1), true)
        }
        ApiStatCommand::PageNewer => {
            scroll_api_stat_periods(state, state.api_stat_visible_periods.max(1), false)
        }
        ApiStatCommand::ScrollOldest => {
            let next = state
                .api_stat_total_periods
                .saturating_sub(state.api_stat_visible_periods);
            let changed = state.api_stat_period_offset != next;
            state.api_stat_period_offset = next;
            changed
        }
        ApiStatCommand::ScrollNewest => {
            let changed = state.api_stat_period_offset != 0;
            state.api_stat_period_offset = 0;
            changed
        }
    }
}

fn scroll_api_stat_periods(state: &mut AppState, amount: usize, older: bool) -> bool {
    let next = scrolled_period_offset(
        state.api_stat_period_offset,
        state.api_stat_total_periods,
        state.api_stat_visible_periods,
        amount,
        older,
    );
    let changed = next != state.api_stat_period_offset;
    state.api_stat_period_offset = next;
    changed
}

fn handle_app_event(state: &mut AppState, evt: AppEvent) -> bool {
    match evt {
        AppEvent::UsageUpdated(res) => {
            match res {
                Ok(snapshot) => {
                    if state.workspace_path.is_some()
                        && !state.no_sessions_confirm_dismissed
                        && snapshot.matched_session_files == 0
                    {
                        state.no_sessions_confirm_open = true;
                    }
                    state.usage = Some(snapshot);
                    state.usage_error = None;
                    state.usage_updated_at = Some(Instant::now());
                }
                Err(err) => {
                    state.usage_error = Some(err.to_string());
                }
            }
            true
        }
        AppEvent::AccountApiUnavailable { message, is_error } => {
            state.limits_enabled = false;
            state.account_usage_enabled = false;
            if is_error {
                state.limits_error = Some(message.clone());
                state.limits_notice = None;
                state.account_usage_error = Some(message);
                state.account_usage_notice = None;
            } else {
                state.limits_error = None;
                state.limits_notice = Some(message.clone());
                state.account_usage_error = None;
                state.account_usage_notice = Some(message);
            }
            true
        }
        AppEvent::LimitsUpdated(res) => {
            match res {
                Ok(limits) => {
                    state.limits_enabled = true;
                    state.limits = Some(limits);
                    state.limits_error = None;
                    state.limits_notice = None;
                    state.limits_updated_at = Some(Instant::now());
                }
                Err(err) => {
                    state.limits_error = Some(err.to_string());
                }
            }
            true
        }
        AppEvent::AccountUsageUpdated(res) => {
            match res {
                Ok(usage) => {
                    state.account_usage_enabled = true;
                    state.account_usage = Some(usage);
                    state.account_usage_error = None;
                    state.account_usage_notice = None;
                    state.account_usage_updated_at = Some(Instant::now());
                }
                Err(err) => {
                    state.account_usage_error = Some(err.to_string());
                }
            }
            true
        }
        AppEvent::HistoryCatalogProgress(progress) => {
            state.read_browser.set_catalog_progress(progress);
            true
        }
        AppEvent::HistoryCatalogUpdated {
            strict,
            snapshot,
            from_cache,
        } => {
            if from_cache {
                state
                    .read_browser
                    .apply_cached_catalog_snapshot(strict, snapshot);
            } else {
                state.read_browser.apply_catalog_snapshot(strict, snapshot);
            }
            true
        }
        AppEvent::HistoryCatalogFailed(error) => {
            state.read_browser.set_catalog_error(error);
            true
        }
    }
}

fn missing_app_server_message() -> &'static str {
    "Codex App Server not found; usage/history still work. Install Codex CLI or pass --codex-bin/--app-server-bin."
}

#[cfg(test)]
fn load_persisted_ui_state(
    comon_home: &Path,
    workspace_hint: Option<&Path>,
) -> Result<PersistedUiState> {
    load_persisted_ui_state_with_history_depth(
        comon_home,
        workspace_hint,
        crate::read::catalog::DEFAULT_DEEP_DEPTH,
    )
}

fn load_persisted_ui_state_with_history_depth(
    comon_home: &Path,
    workspace_hint: Option<&Path>,
    history_deep_depth: u8,
) -> Result<PersistedUiState> {
    let store = load_or_bootstrap_state_store(comon_home)?;
    let mut state =
        PersistedUiState::default_for_workspace(workspace_hint.map(|path| path.to_path_buf()));
    state.history_deep_depth = history_deep_depth.clamp(1, crate::read::catalog::MAX_DEEP_DEPTH);

    if let Some(metric_text) = store.global.metric.as_deref() {
        if let Some(metric) = usage_metric_from_store(metric_text) {
            state.metric = metric;
        }
    }
    if let Some(range_text) = store.global.range.as_deref() {
        if let Some(range) = chart_range_from_store(range_text) {
            state.range = range;
        }
    }
    if let Some(zone_text) = store.global.usage_zone.as_deref() {
        if let Some(zone) = usage_zone_from_store(zone_text) {
            state.usage_zone = zone;
        }
    }
    if let Some(orientation_text) = store.global.orientation.as_deref() {
        if let Some(orientation) = chart_orientation_from_store(orientation_text) {
            state.orientation = orientation;
        }
    }
    if let Some(grouping_text) = store.global.api_stat_grouping.as_deref() {
        if let Some(grouping) = api_stat_grouping_from_store(grouping_text) {
            state.api_stat_grouping = grouping;
        }
    }
    if let Some(graph_text) = store.global.api_stat_graph.as_deref() {
        if let Some(graph) = api_stat_graph_from_store(graph_text) {
            state.api_stat_graph = graph;
        }
    }
    if let Some(orientation_text) = store.global.api_stat_orientation.as_deref() {
        if let Some(orientation) = chart_orientation_from_store(orientation_text) {
            state.api_stat_orientation = orientation;
        }
    }
    if let Some(limit) = store.global.activity_project_limit {
        state.activity_project_limit =
            limit.clamp(MIN_ACTIVITY_PROJECT_LIMIT, MAX_ACTIVITY_PROJECT_LIMIT);
    }
    if let Some(style_text) = store.global.display_style.as_deref() {
        if let Some(style) = DisplayStyle::from_store(style_text) {
            state.display_style = style;
        }
    }
    state.skip_quit_confirmation = store.global.skip_quit_confirmation;
    if let Some(mode_text) = store.global.history_project_view_mode.as_deref() {
        if let Some(mode) = crate::read::catalog::ProjectViewMode::from_store(mode_text) {
            state.history_project_view_mode = mode;
        }
    }
    if let Some(depth) = store.global.history_deep_depth {
        state.history_deep_depth = depth.clamp(1, crate::read::catalog::MAX_DEEP_DEPTH);
    }
    state.history_selected_projects = store
        .global
        .history_selected_projects
        .iter()
        .cloned()
        .collect();
    state.history_explicitly_excluded_projects = store
        .global
        .history_explicitly_excluded_projects
        .iter()
        .cloned()
        .collect();
    state.history_expanded_remote_groups = store
        .global
        .history_expanded_remote_groups
        .iter()
        .cloned()
        .collect();

    if let Some(workspace_path) = state.workspace_path.as_ref() {
        let workspace_key = workspace_path.to_string_lossy();
        if let Some(workspace_state) = store.workspaces.get(workspace_key.as_ref()) {
            state.no_sessions_confirm_dismissed = workspace_state.no_sessions_confirm_dismissed;
        }
    }

    Ok(state)
}

fn save_persisted_ui_state(comon_home: &Path, state: &PersistedUiState) -> Result<()> {
    let mut store = load_or_bootstrap_state_store(comon_home)?;
    let now = unix_time_seconds();
    let workspace_path_text = state
        .workspace_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());

    store.global.metric = Some(usage_metric_to_store(state.metric).to_string());
    store.global.range = Some(chart_range_to_store(state.range).to_string());
    store.global.usage_zone = Some(usage_zone_to_store(state.usage_zone).to_string());
    store.global.orientation = Some(chart_orientation_to_store(state.orientation).to_string());
    store.global.api_stat_grouping =
        Some(api_stat_grouping_to_store(state.api_stat_grouping).to_string());
    store.global.api_stat_graph = Some(api_stat_graph_to_store(state.api_stat_graph).to_string());
    store.global.api_stat_orientation =
        Some(chart_orientation_to_store(state.api_stat_orientation).to_string());
    store.global.activity_project_limit = Some(
        state
            .activity_project_limit
            .clamp(MIN_ACTIVITY_PROJECT_LIMIT, MAX_ACTIVITY_PROJECT_LIMIT),
    );
    store.global.display_style = Some(state.display_style.store_value().to_string());
    store.global.skip_quit_confirmation = state.skip_quit_confirmation;
    store.global.history_project_view_mode =
        Some(state.history_project_view_mode.store_value().to_string());
    store.global.history_deep_depth = Some(
        state
            .history_deep_depth
            .clamp(1, crate::read::catalog::MAX_DEEP_DEPTH),
    );
    store.global.history_selected_projects =
        state.history_selected_projects.iter().cloned().collect();
    store.global.history_explicitly_excluded_projects = state
        .history_explicitly_excluded_projects
        .iter()
        .cloned()
        .collect();
    store.global.history_expanded_remote_groups = state
        .history_expanded_remote_groups
        .iter()
        .cloned()
        .collect();
    store.global.last_workspace_path = workspace_path_text.clone();
    store.global.updated_at = now;

    if let Some(workspace_path_text) = workspace_path_text {
        store.workspaces.insert(
            workspace_path_text,
            StoredWorkspaceState {
                no_sessions_confirm_dismissed: state.no_sessions_confirm_dismissed,
                updated_at: now,
            },
        );
    }

    write_state_store(comon_home, &store)
}

fn load_or_bootstrap_state_store(comon_home: &Path) -> Result<StateStore> {
    let store_path = comon_home.join(STATE_STORE_FILE_NAME);
    if !store_path.exists() {
        return Ok(StateStore::default());
    }
    crate::storage::enforce_private_file_if_exists(&store_path)?;
    let bytes = std::fs::read(&store_path)
        .with_context(|| format!("Unable to read state store {}", store_path.display()))?;
    let store = serde_json::from_slice::<StateStore>(&bytes)
        .with_context(|| format!("Unable to parse state store {}", store_path.display()))?;
    if store.schema_version > STATE_STORE_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported comon state schema version: {} (maximum {})",
            store.schema_version,
            STATE_STORE_SCHEMA_VERSION
        );
    }
    let mut store = store;
    store.schema_version = STATE_STORE_SCHEMA_VERSION;
    Ok(store)
}

fn write_state_store(comon_home: &Path, store: &StateStore) -> Result<()> {
    let store_path = comon_home.join(STATE_STORE_FILE_NAME);
    let encoded = serde_json::to_vec_pretty(store)
        .with_context(|| format!("Unable to encode state store {}", store_path.display()))?;
    crate::storage::write_private_file(&store_path, &encoded)?;
    Ok(())
}

fn usage_metric_to_store(metric: UsageMetric) -> &'static str {
    match metric {
        UsageMetric::Tokens => "tokens",
        UsageMetric::Time => "time",
        UsageMetric::Runs => "runs",
    }
}

fn usage_metric_from_store(value: &str) -> Option<UsageMetric> {
    match value {
        "tokens" => Some(UsageMetric::Tokens),
        "time" => Some(UsageMetric::Time),
        "runs" => Some(UsageMetric::Runs),
        _ => None,
    }
}

fn chart_range_to_store(range: ChartRange) -> &'static str {
    match range {
        ChartRange::Day => "day",
        ChartRange::Week => "week",
        ChartRange::Month => "month",
    }
}

fn chart_range_from_store(value: &str) -> Option<ChartRange> {
    match value {
        "day" => Some(ChartRange::Day),
        "week" => Some(ChartRange::Week),
        "month" => Some(ChartRange::Month),
        _ => None,
    }
}

fn usage_zone_to_store(zone: UsageZone) -> &'static str {
    match zone {
        UsageZone::Local => "local",
        UsageZone::Utc => "utc",
    }
}

fn usage_zone_from_store(value: &str) -> Option<UsageZone> {
    match value {
        "local" => Some(UsageZone::Local),
        "utc" => Some(UsageZone::Utc),
        _ => None,
    }
}

fn chart_orientation_to_store(orientation: ChartOrientation) -> &'static str {
    match orientation {
        ChartOrientation::Vertical => "vertical",
        ChartOrientation::Horizontal => "horizontal",
    }
}

fn chart_orientation_from_store(value: &str) -> Option<ChartOrientation> {
    match value {
        "vertical" => Some(ChartOrientation::Vertical),
        "horizontal" => Some(ChartOrientation::Horizontal),
        _ => None,
    }
}

fn api_stat_grouping_to_store(grouping: ApiStatGrouping) -> &'static str {
    match grouping {
        ApiStatGrouping::Day => "day",
        ApiStatGrouping::Week => "week",
        ApiStatGrouping::Month => "month",
    }
}

fn api_stat_grouping_from_store(value: &str) -> Option<ApiStatGrouping> {
    match value {
        "day" => Some(ApiStatGrouping::Day),
        "week" => Some(ApiStatGrouping::Week),
        "month" => Some(ApiStatGrouping::Month),
        _ => None,
    }
}

fn api_stat_graph_to_store(graph: ApiStatGraph) -> &'static str {
    match graph {
        ApiStatGraph::Bars => "bars",
        ApiStatGraph::Heat => "heat",
    }
}

fn api_stat_graph_from_store(value: &str) -> Option<ApiStatGraph> {
    match value {
        "bars" => Some(ApiStatGraph::Bars),
        "heat" => Some(ApiStatGraph::Heat),
        _ => None,
    }
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl AppState {
    pub(crate) fn formatter(&self) -> DisplayFormatter<'_> {
        DisplayFormatter::new(self.display_style, &self.system_locale)
    }

    pub(crate) fn accent_colors(&self) -> (Color, Color) {
        self.accent_theme.colors()
    }

    pub(crate) fn usage_updated_label(&self) -> Option<String> {
        let updated_at = self.usage_updated_at?;
        Some(crate::ui::format_updated_label(updated_at))
    }

    pub(crate) fn limits_updated_label(&self) -> Option<String> {
        let updated_at = self.limits_updated_at?;
        Some(crate::ui::format_updated_label(updated_at))
    }

    pub(crate) fn account_usage_updated_label(&self) -> Option<String> {
        let updated_at = self.account_usage_updated_at?;
        Some(crate::ui::format_updated_label(updated_at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn accent_theme_exposes_requested_normal_and_bright_pairs() {
        let expected = [
            (AccentTheme::Cyan, Color::Cyan, Color::LightCyan),
            (AccentTheme::Red, Color::Red, Color::LightRed),
            (AccentTheme::Green, Color::Green, Color::LightGreen),
            (AccentTheme::Yellow, Color::Yellow, Color::LightYellow),
            (AccentTheme::Blue, Color::Blue, Color::LightBlue),
            (AccentTheme::Magenta, Color::Magenta, Color::LightMagenta),
            (AccentTheme::Gray, Color::Gray, Color::White),
        ];

        assert_eq!(AccentTheme::ALL.len(), expected.len());
        for (index, (theme, accent, accent_bright)) in expected.iter().enumerate() {
            assert_eq!(AccentTheme::ALL[index], *theme);
            assert_eq!(theme.colors(), (*accent, *accent_bright));
        }
    }

    #[test]
    fn accent_theme_cycles_through_every_available_theme() {
        let mut theme = AccentTheme::Cyan;
        for expected in AccentTheme::ALL.iter().copied().skip(1) {
            theme = theme.cycled();
            assert_eq!(theme, expected);
        }
        assert_eq!(theme.cycled(), AccentTheme::Cyan);
    }

    #[test]
    fn bar_fill_mode_toggles_between_semigraphic_and_background() {
        assert_eq!(
            BarFillMode::Semigraphic.toggled(),
            BarFillMode::DualColorBackground
        );
        assert_eq!(
            BarFillMode::DualColorBackground.toggled(),
            BarFillMode::Semigraphic
        );
    }

    #[test]
    fn ui_hit_targets_include_left_top_and_exclude_right_bottom_edges() {
        let targets = [UiHitTarget {
            area: Rect::new(10, 5, 4, 2),
            action: UiClickAction::SetMetric(UsageMetric::Runs),
        }];

        assert_eq!(
            ui_click_action_at(&targets, 10, 5),
            Some(UiClickAction::SetMetric(UsageMetric::Runs))
        );
        assert_eq!(
            ui_click_action_at(&targets, 13, 6),
            Some(UiClickAction::SetMetric(UsageMetric::Runs))
        );
        assert_eq!(ui_click_action_at(&targets, 14, 5), None);
        assert_eq!(ui_click_action_at(&targets, 10, 7), None);
    }

    #[test]
    fn quit_confirmation_arrows_select_and_enter_uses_the_selected_button() {
        assert_eq!(
            quit_confirmation_command(KeyCode::Left, KeyModifiers::NONE, false),
            Some(QuitConfirmationCommand::SelectYes)
        );
        assert_eq!(
            quit_confirmation_command(KeyCode::Right, KeyModifiers::NONE, true),
            Some(QuitConfirmationCommand::SelectNo)
        );
        assert_eq!(
            quit_confirmation_command(KeyCode::Enter, KeyModifiers::NONE, true),
            Some(QuitConfirmationCommand::Confirm)
        );
        assert_eq!(
            quit_confirmation_command(KeyCode::Enter, KeyModifiers::NONE, false),
            Some(QuitConfirmationCommand::Cancel)
        );
    }

    #[test]
    fn period_scrolling_clamps_at_oldest_and_newest() {
        assert_eq!(scrolled_period_offset(0, 30, 10, 1, true), 1);
        assert_eq!(scrolled_period_offset(19, 30, 10, 5, true), 20);
        assert_eq!(scrolled_period_offset(3, 30, 10, 2, false), 1);
        assert_eq!(scrolled_period_offset(1, 30, 10, 5, false), 0);
        assert_eq!(scrolled_period_offset(4, 5, 10, 1, true), 0);
    }

    #[test]
    fn rapid_scan_catch_up_stops_when_pending_work_cannot_advance() {
        assert!(should_continue_scan_catch_up(4, 10, 1_000, None));
        assert!(should_continue_scan_catch_up(
            3,
            11,
            1_000,
            Some((10, 1_000))
        ));
        assert!(should_continue_scan_catch_up(
            3,
            10,
            1_500,
            Some((10, 1_000))
        ));
        assert!(!should_continue_scan_catch_up(
            1,
            10,
            1_000,
            Some((10, 1_000))
        ));
        assert!(!should_continue_scan_catch_up(0, 10, 1_000, Some((9, 900))));
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("comon-app-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn screen_cycle_places_api_stats_after_usage() {
        assert_eq!(
            next_active_screen(ActiveScreen::Usage),
            ActiveScreen::ApiStat
        );
        assert_eq!(
            next_active_screen(ActiveScreen::ApiStat),
            ActiveScreen::Activity
        );
        assert_eq!(
            next_active_screen(ActiveScreen::Activity),
            ActiveScreen::LimitResets
        );
        assert_eq!(
            next_active_screen(ActiveScreen::LimitResets),
            ActiveScreen::Read
        );
        assert_eq!(next_active_screen(ActiveScreen::Read), ActiveScreen::Usage);
    }

    #[test]
    fn load_persisted_ui_state_does_not_restore_last_workspace_without_hint() {
        let comon_home = make_temp_dir("state-no-hint");
        let mut store = StateStore::default();
        store.global.last_workspace_path = Some("/tmp/old-workspace".to_string());
        write_state_store(&comon_home, &store).expect("write state store");

        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(loaded.workspace_path, None);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn load_persisted_ui_state_uses_workspace_hint_and_workspace_state() {
        let comon_home = make_temp_dir("state-hint");
        let workspace_path = PathBuf::from("/tmp/repo-workspace");
        let mut store = StateStore::default();
        store.global.last_workspace_path = Some("/tmp/other-workspace".to_string());
        store.workspaces.insert(
            workspace_path.to_string_lossy().into_owned(),
            StoredWorkspaceState {
                no_sessions_confirm_dismissed: true,
                updated_at: 1,
            },
        );
        write_state_store(&comon_home, &store).expect("write state store");

        let loaded = load_persisted_ui_state(&comon_home, Some(workspace_path.as_path()))
            .expect("load persisted ui state");
        assert_eq!(loaded.workspace_path, Some(workspace_path));
        assert!(loaded.no_sessions_confirm_dismissed);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn legacy_state_without_display_style_defaults_to_classic() {
        let comon_home = make_temp_dir("legacy-display-style");
        let store = StateStore::default();
        write_state_store(&comon_home, &store).expect("write state store");

        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(loaded.display_style, DisplayStyle::Classic);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn display_style_round_trips_through_state_store() {
        let comon_home = make_temp_dir("system-display-style");
        let mut state = PersistedUiState::default_for_workspace(None);
        state.display_style = DisplayStyle::SystemFull;

        save_persisted_ui_state(&comon_home, &state).expect("save persisted ui state");
        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(loaded.display_style, DisplayStyle::SystemFull);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn api_stat_controls_round_trip_through_state_store() {
        let comon_home = make_temp_dir("api-stat-controls");
        let mut state = PersistedUiState::default_for_workspace(None);
        state.api_stat_grouping = ApiStatGrouping::Month;
        state.api_stat_graph = ApiStatGraph::Heat;
        state.api_stat_orientation = ChartOrientation::Horizontal;

        save_persisted_ui_state(&comon_home, &state).expect("save persisted ui state");
        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(loaded.api_stat_grouping, ApiStatGrouping::Month);
        assert_eq!(loaded.api_stat_graph, ApiStatGraph::Heat);
        assert_eq!(loaded.api_stat_orientation, ChartOrientation::Horizontal);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn usage_grouping_and_zone_round_trip_through_state_store() {
        let comon_home = make_temp_dir("usage-zone-controls");
        let mut state = PersistedUiState::default_for_workspace(None);
        state.range = ChartRange::Month;
        state.usage_zone = UsageZone::Utc;

        save_persisted_ui_state(&comon_home, &state).expect("save persisted ui state");
        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(loaded.range, ChartRange::Month);
        assert_eq!(loaded.usage_zone, UsageZone::Utc);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn quit_confirmation_preference_round_trips_through_state_store() {
        let comon_home = make_temp_dir("quit-confirmation-preference");
        let mut state = PersistedUiState::default_for_workspace(None);
        state.skip_quit_confirmation = true;

        save_persisted_ui_state(&comon_home, &state).expect("save persisted ui state");
        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert!(loaded.skip_quit_confirmation);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn history_project_controls_round_trip_through_state_store() {
        let comon_home = make_temp_dir("history-project-controls");
        let mut state = PersistedUiState::default_for_workspace(None);
        state.history_project_view_mode = crate::read::catalog::ProjectViewMode::Custom;
        state.history_deep_depth = 5;
        state
            .history_selected_projects
            .insert("remote:example.com/team/project".to_string());
        state
            .history_explicitly_excluded_projects
            .insert("path:/home/example/hidden".to_string());

        save_persisted_ui_state(&comon_home, &state).expect("save persisted ui state");
        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(
            loaded.history_project_view_mode,
            crate::read::catalog::ProjectViewMode::Custom
        );
        assert_eq!(loaded.history_deep_depth, 5);
        assert_eq!(
            loaded.history_selected_projects,
            state.history_selected_projects
        );
        assert_eq!(
            loaded.history_explicitly_excluded_projects,
            state.history_explicitly_excluded_projects
        );

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn state_schema_one_migrates_by_applying_catalog_defaults() {
        let comon_home = make_temp_dir("state-schema-one");
        let mut store = StateStore {
            schema_version: 1,
            ..StateStore::default()
        };
        store.global.display_style = Some(DisplayStyle::SystemFull.store_value().to_string());
        write_state_store(&comon_home, &store).expect("write legacy state");

        let loaded = load_persisted_ui_state_with_history_depth(&comon_home, None, 4)
            .expect("load legacy state");
        assert_eq!(loaded.display_style, DisplayStyle::SystemFull);
        assert_eq!(
            loaded.history_project_view_mode,
            crate::read::catalog::ProjectViewMode::Strict
        );
        assert_eq!(loaded.history_deep_depth, 4);

        let _ = std::fs::remove_dir_all(comon_home);
    }

    #[test]
    fn legacy_system_style_restores_as_system_compact() {
        let comon_home = make_temp_dir("legacy-system-display-style");
        let mut store = StateStore::default();
        store.global.display_style = Some("system".to_string());
        write_state_store(&comon_home, &store).expect("write state store");

        let loaded = load_persisted_ui_state(&comon_home, None).expect("load persisted ui state");
        assert_eq!(loaded.display_style, DisplayStyle::SystemCompact);

        let _ = std::fs::remove_dir_all(comon_home);
    }
}
