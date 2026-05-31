use crate::codex_rpc::{AccountRateLimits, CodexRpc};
use crate::read;
use crate::usage::{ChartRange, LocalUsageSnapshot, UsageMetric};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
pub struct Config {
    pub codex_bin: Option<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy)]
enum UsageCommand {
    RefreshAll,
    ToggleMetric,
    ToggleRange,
    ToggleOrientation,
    ToggleHelp,
    ConfirmContinue,
}

#[derive(Debug)]
enum AppEvent {
    UsageUpdated(Result<LocalUsageSnapshot>),
    LimitsUpdated(Result<AccountRateLimits>),
    LimitsUnavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveScreen {
    Usage,
    Activity,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOutcome {
    Continue(bool),
    Quit,
}

#[derive(Debug, Clone, Copy)]
enum ActivityCommand {
    RefreshAll,
    ToggleMetric,
    IncreaseProjects,
    DecreaseProjects,
    ToggleHelp,
}

#[derive(Debug)]
pub(crate) struct AppState {
    pub(crate) active_screen: ActiveScreen,
    pub(crate) metric: UsageMetric,
    pub(crate) range: ChartRange,
    pub(crate) orientation: ChartOrientation,
    pub(crate) activity_project_limit: usize,
    pub(crate) show_help: bool,
    pub(crate) workspace_path: Option<std::path::PathBuf>,
    pub(crate) no_sessions_confirm_open: bool,
    pub(crate) no_sessions_confirm_dismissed: bool,

    pub(crate) usage: Option<LocalUsageSnapshot>,
    pub(crate) usage_updated_at: Option<Instant>,
    pub(crate) usage_error: Option<String>,

    pub(crate) limits: Option<AccountRateLimits>,
    pub(crate) limits_updated_at: Option<Instant>,
    pub(crate) limits_error: Option<String>,
    pub(crate) limits_enabled: bool,
    pub(crate) read_sessions_dir: PathBuf,
    pub(crate) read_browser: crate::read::tui::BrowserState,
}

const STATE_STORE_SCHEMA_VERSION: u32 = 1;
const STATE_STORE_FILE_NAME: &str = "state.json";
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
pub(crate) const DEFAULT_ACTIVITY_PROJECT_LIMIT: usize = 5;
pub(crate) const MIN_ACTIVITY_PROJECT_LIMIT: usize = 1;
pub(crate) const MAX_ACTIVITY_PROJECT_LIMIT: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedUiState {
    metric: UsageMetric,
    range: ChartRange,
    orientation: ChartOrientation,
    activity_project_limit: usize,
    workspace_path: Option<PathBuf>,
    no_sessions_confirm_dismissed: bool,
}

impl PersistedUiState {
    fn default_for_workspace(workspace_path: Option<PathBuf>) -> Self {
        Self {
            metric: UsageMetric::Tokens,
            range: ChartRange::Week,
            orientation: ChartOrientation::Horizontal,
            activity_project_limit: DEFAULT_ACTIVITY_PROJECT_LIMIT,
            workspace_path,
            no_sessions_confirm_dismissed: false,
        }
    }

    fn from_app_state(state: &AppState) -> Self {
        Self {
            metric: state.metric,
            range: state.range,
            orientation: state.orientation,
            activity_project_limit: state.activity_project_limit,
            workspace_path: state.workspace_path.clone(),
            no_sessions_confirm_dismissed: state.no_sessions_confirm_dismissed,
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
    orientation: Option<String>,
    activity_project_limit: Option<usize>,
    last_workspace_path: Option<String>,
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
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let restored_ui_state =
        load_persisted_ui_state(&config.comon_home, config.workspace_path.as_deref())
            .unwrap_or_else(|_| {
                PersistedUiState::default_for_workspace(config.workspace_path.clone())
            });

    let scan_cache_db_path = config.comon_home.join("comon.db");
    if config.rebuild_cache_on_start {
        clear_scan_cache_files(&scan_cache_db_path)?;
    }
    let read_config = read::Config {
        sessions_dir: config.read_sessions_dir.clone(),
    };
    let read_browser = read::build_browser(&read_config)?;

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
            let mut interval = tokio::time::interval(refresh);
            // Immediate first run
            let first = tokio::task::spawn_blocking({
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
            if evt_tx.send(AppEvent::UsageUpdated(first)).await.is_err() {
                return;
            }
            // Consume the immediate first tick so the next one waits `refresh`.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    _ = interval.tick() => {}
                    recv = usage_refresh_rx.recv() => {
                        if recv.is_none() { break; }
                    }
                }
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
        let cwd = config.cwd.clone();
        let refresh = Duration::from_secs(config.refresh_limits_secs);
        let mut limits_refresh_rx = limits_refresh_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let rpc = match CodexRpc::spawn(codex_bin, cwd).await {
                Ok(rpc) => rpc,
                Err(err) => {
                    let _ = evt_tx
                        .send(AppEvent::LimitsUnavailable(err.to_string()))
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
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        rpc.kill().await;
                        break;
                    }
                    _ = interval.tick() => {}
                    recv = limits_refresh_rx.recv() => {
                        if recv.is_none() {
                            rpc.kill().await;
                            break;
                        }
                    }
                }
                let res = rpc.read_account_rate_limits().await;
                if evt_tx.send(AppEvent::LimitsUpdated(res)).await.is_err() {
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
        orientation: restored_ui_state.orientation,
        activity_project_limit: restored_ui_state.activity_project_limit,
        show_help: false,
        workspace_path: restored_ui_state.workspace_path.clone(),
        no_sessions_confirm_open: false,
        no_sessions_confirm_dismissed: restored_ui_state.no_sessions_confirm_dismissed,
        usage: None,
        usage_updated_at: None,
        usage_error: None,
        limits: None,
        limits_updated_at: None,
        limits_error: None,
        limits_enabled: true,
        read_sessions_dir: config.read_sessions_dir.clone(),
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
) -> Result<InputOutcome> {
    if let Event::Key(key) = &event {
        if key.kind != KeyEventKind::Press {
            return Ok(InputOutcome::Continue(false));
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => return Ok(InputOutcome::Quit),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(InputOutcome::Quit),
            (KeyCode::Char('s'), _) | (KeyCode::Char('S'), _) | (KeyCode::F(2), _) => {
                state.active_screen = match state.active_screen {
                    ActiveScreen::Usage => ActiveScreen::Activity,
                    ActiveScreen::Activity => ActiveScreen::Read,
                    ActiveScreen::Read => ActiveScreen::Usage,
                };
                return Ok(InputOutcome::Continue(true));
            }
            (KeyCode::Char('r'), _) | (KeyCode::F(5), _)
                if state.active_screen == ActiveScreen::Read =>
            {
                let read_config = read::Config {
                    sessions_dir: state.read_sessions_dir.clone(),
                };
                state.read_browser = read::build_browser(&read_config)?;
                return Ok(InputOutcome::Continue(true));
            }
            _ => {}
        }
    }

    if matches!(event, Event::Resize(_, _)) {
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
        ActiveScreen::Read => Ok(InputOutcome::Continue(read::tui::handle_event(
            &mut state.read_browser,
            event,
        )?)),
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
                (KeyCode::Char('w'), _) => Some(UsageCommand::ToggleRange),
                (KeyCode::Char('f'), _) => Some(UsageCommand::ToggleOrientation),
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
                (KeyCode::Char('?'), _) => Some(ActivityCommand::ToggleHelp),
                _ => None,
            }
        }
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
            state.range = match state.range {
                ChartRange::Week => ChartRange::Month,
                ChartRange::Month => ChartRange::Week,
            };
            true
        }
        UsageCommand::ToggleOrientation => {
            state.orientation = match state.orientation {
                ChartOrientation::Horizontal => ChartOrientation::Vertical,
                ChartOrientation::Vertical => ChartOrientation::Horizontal,
            };
            true
        }
        UsageCommand::RefreshAll => {
            let _ = usage_refresh_tx.try_send(());
            let _ = limits_refresh_tx.try_send(());
            true
        }
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
        ActivityCommand::RefreshAll => {
            let _ = usage_refresh_tx.try_send(());
            let _ = limits_refresh_tx.try_send(());
            true
        }
    }
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
        AppEvent::LimitsUnavailable(msg) => {
            state.limits_enabled = false;
            state.limits_error = Some(msg);
            true
        }
        AppEvent::LimitsUpdated(res) => {
            match res {
                Ok(limits) => {
                    state.limits = Some(limits);
                    state.limits_error = None;
                    state.limits_updated_at = Some(Instant::now());
                }
                Err(err) => {
                    state.limits_error = Some(err.to_string());
                }
            }
            true
        }
    }
}

fn load_persisted_ui_state(
    comon_home: &Path,
    workspace_hint: Option<&Path>,
) -> Result<PersistedUiState> {
    let store = load_or_bootstrap_state_store(comon_home)?;
    let mut state =
        PersistedUiState::default_for_workspace(workspace_hint.map(|path| path.to_path_buf()));

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
    if let Some(orientation_text) = store.global.orientation.as_deref() {
        if let Some(orientation) = chart_orientation_from_store(orientation_text) {
            state.orientation = orientation;
        }
    }
    if let Some(limit) = store.global.activity_project_limit {
        state.activity_project_limit =
            limit.clamp(MIN_ACTIVITY_PROJECT_LIMIT, MAX_ACTIVITY_PROJECT_LIMIT);
    }

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
    store.global.orientation = Some(chart_orientation_to_store(state.orientation).to_string());
    store.global.activity_project_limit = Some(
        state
            .activity_project_limit
            .clamp(MIN_ACTIVITY_PROJECT_LIMIT, MAX_ACTIVITY_PROJECT_LIMIT),
    );
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
    if store.schema_version != STATE_STORE_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported comon state schema version: {} (expected {})",
            store.schema_version,
            STATE_STORE_SCHEMA_VERSION
        );
    }
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
        ChartRange::Week => "week",
        ChartRange::Month => "month",
    }
}

fn chart_range_from_store(value: &str) -> Option<ChartRange> {
    match value {
        "week" => Some(ChartRange::Week),
        "month" => Some(ChartRange::Month),
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

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl AppState {
    pub(crate) fn usage_updated_label(&self) -> Option<String> {
        let updated_at = self.usage_updated_at?;
        Some(crate::ui::format_updated_label(updated_at))
    }

    pub(crate) fn limits_updated_label(&self) -> Option<String> {
        let updated_at = self.limits_updated_at?;
        Some(crate::ui::format_updated_label(updated_at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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
}
