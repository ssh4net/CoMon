use crate::codex_rpc::{AccountRateLimits, CodexRpc};
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
enum UiCommand {
    RefreshAll,
    ToggleMetric,
    ToggleRange,
    ToggleOrientation,
    ToggleHelp,
    ConfirmContinue,
    Quit,
}

#[derive(Debug)]
enum AppEvent {
    UsageUpdated(Result<LocalUsageSnapshot>),
    LimitsUpdated(Result<AccountRateLimits>),
    LimitsUnavailable(String),
}

#[derive(Debug)]
pub(crate) struct AppState {
    pub(crate) metric: UsageMetric,
    pub(crate) range: ChartRange,
    pub(crate) orientation: ChartOrientation,
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
}

const STATE_STORE_SCHEMA_VERSION: u32 = 1;
const STATE_STORE_FILE_NAME: &str = "state.json";
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedUiState {
    metric: UsageMetric,
    range: ChartRange,
    orientation: ChartOrientation,
    workspace_path: Option<PathBuf>,
    no_sessions_confirm_dismissed: bool,
}

impl PersistedUiState {
    fn default_for_workspace(workspace_path: Option<PathBuf>) -> Self {
        Self {
            metric: UsageMetric::Tokens,
            range: ChartRange::Week,
            orientation: ChartOrientation::Horizontal,
            workspace_path,
            no_sessions_confirm_dismissed: false,
        }
    }

    fn from_app_state(state: &AppState) -> Self {
        Self {
            metric: state.metric,
            range: state.range,
            orientation: state.orientation,
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
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(64);
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
        let cmd_tx = cmd_tx.clone();
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

                if let Some(cmd) = map_event_to_cmd(event) {
                    if cmd_tx.send(cmd).await.is_err() {
                        break;
                    }
                    if matches!(cmd, UiCommand::Quit) {
                        break;
                    }
                }
            }
        });
    }

    let mut state = AppState {
        metric: restored_ui_state.metric,
        range: restored_ui_state.range,
        orientation: restored_ui_state.orientation,
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
    };

    // Initial draw.
    terminal.draw(|f| crate::ui::render(f, &state))?;

    let mut last_redraw = Instant::now();
    let min_redraw = Duration::from_millis(50);
    let mut last_observed_ui_state = PersistedUiState::from_app_state(&state);
    let mut last_saved_ui_state = last_observed_ui_state.clone();
    let mut state_changed_at: Option<Instant> = None;

    loop {
        let mut dirty = false;

        tokio::select! {
            cmd = cmd_rx.recv() => {
                if let Some(cmd) = cmd {
                    dirty |= handle_cmd(&mut state, cmd, &usage_refresh_tx, &limits_refresh_tx).await?;
                    if matches!(cmd, UiCommand::Quit) {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                }
            }
            evt = evt_rx.recv() => {
                if let Some(evt) = evt {
                    dirty |= handle_event(&mut state, evt);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // periodic UI tick: allow relative-time labels to update
                dirty = true;
            }
        }

        if dirty && last_redraw.elapsed() >= min_redraw {
            terminal.draw(|f| crate::ui::render(f, &state))?;
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

fn map_event_to_cmd(event: Event) -> Option<UiCommand> {
    match event {
        Event::Resize(_, _) => Some(UiCommand::RefreshAll),
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return None;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) => Some(UiCommand::Quit),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(UiCommand::Quit),
                (KeyCode::Esc, _) => Some(UiCommand::Quit),
                (KeyCode::Char('r'), _) => Some(UiCommand::RefreshAll),
                (KeyCode::F(5), _) => Some(UiCommand::RefreshAll),
                (KeyCode::Tab, _) => Some(UiCommand::ToggleMetric),
                (KeyCode::Char('w'), _) => Some(UiCommand::ToggleRange),
                (KeyCode::Char('f'), _) => Some(UiCommand::ToggleOrientation),
                (KeyCode::Char('?'), _) => Some(UiCommand::ToggleHelp),
                (KeyCode::Enter, _) => Some(UiCommand::ConfirmContinue),
                (KeyCode::Char('y'), _) => Some(UiCommand::ConfirmContinue),
                (KeyCode::Char('Y'), _) => Some(UiCommand::ConfirmContinue),
                _ => None,
            }
        }
        _ => None,
    }
}

async fn handle_cmd(
    state: &mut AppState,
    cmd: UiCommand,
    usage_refresh_tx: &mpsc::Sender<()>,
    limits_refresh_tx: &mpsc::Sender<()>,
) -> Result<bool> {
    if state.no_sessions_confirm_open {
        match cmd {
            UiCommand::ConfirmContinue => {
                state.no_sessions_confirm_open = false;
                state.no_sessions_confirm_dismissed = true;
                return Ok(true);
            }
            UiCommand::Quit => return Ok(true),
            _ => return Ok(false),
        }
    }
    match cmd {
        UiCommand::Quit => Ok(true),
        UiCommand::ToggleHelp => {
            state.show_help = !state.show_help;
            Ok(true)
        }
        UiCommand::ConfirmContinue => Ok(false),
        UiCommand::ToggleMetric => {
            state.metric = match state.metric {
                UsageMetric::Tokens => UsageMetric::Time,
                UsageMetric::Time => UsageMetric::Runs,
                UsageMetric::Runs => UsageMetric::Tokens,
            };
            Ok(true)
        }
        UiCommand::ToggleRange => {
            state.range = match state.range {
                ChartRange::Week => ChartRange::Month,
                ChartRange::Month => ChartRange::Week,
            };
            Ok(true)
        }
        UiCommand::ToggleOrientation => {
            state.orientation = match state.orientation {
                ChartOrientation::Horizontal => ChartOrientation::Vertical,
                ChartOrientation::Vertical => ChartOrientation::Horizontal,
            };
            Ok(true)
        }
        UiCommand::RefreshAll => {
            let _ = usage_refresh_tx.try_send(());
            let _ = limits_refresh_tx.try_send(());
            Ok(true)
        }
    }
}

fn handle_event(state: &mut AppState, evt: AppEvent) -> bool {
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
    if state.workspace_path.is_none() {
        state.workspace_path = store.global.last_workspace_path.map(PathBuf::from);
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
