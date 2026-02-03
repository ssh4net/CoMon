use crate::codex_rpc::{AccountRateLimits, CodexRpc};
use crate::usage::{ChartRange, LocalUsageSnapshot, UsageMetric};
use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
pub struct Config {
    pub codex_bin: Option<String>,
    pub codex_home: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
    pub usage_days: u32,
    pub refresh_usage_secs: u64,
    pub refresh_limits_secs: u64,
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

    pub(crate) usage: Option<LocalUsageSnapshot>,
    pub(crate) usage_updated_at: Option<Instant>,
    pub(crate) usage_error: Option<String>,

    pub(crate) limits: Option<AccountRateLimits>,
    pub(crate) limits_updated_at: Option<Instant>,
    pub(crate) limits_error: Option<String>,
    pub(crate) limits_enabled: bool,
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

    // Spawn usage worker.
    {
        let evt_tx = evt_tx.clone();
        let codex_home = config.codex_home.clone();
        let usage_days = config.usage_days;
        let refresh = Duration::from_secs(config.refresh_usage_secs);
        let mut usage_refresh_rx = usage_refresh_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(refresh);
            // Immediate first run
            let first = tokio::task::spawn_blocking({
                let codex_home = codex_home.clone();
                move || crate::usage::compute_snapshot(usage_days, &codex_home, None)
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
                    move || crate::usage::compute_snapshot(usage_days, &codex_home, None)
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
        metric: UsageMetric::Tokens,
        range: ChartRange::Week,
        orientation: ChartOrientation::Horizontal,
        show_help: false,
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
                (KeyCode::Char('r'), _) => Some(UiCommand::RefreshAll),
                (KeyCode::F(5), _) => Some(UiCommand::RefreshAll),
                (KeyCode::Tab, _) => Some(UiCommand::ToggleMetric),
                (KeyCode::Char('w'), _) => Some(UiCommand::ToggleRange),
                (KeyCode::Char('f'), _) => Some(UiCommand::ToggleOrientation),
                (KeyCode::Char('?'), _) => Some(UiCommand::ToggleHelp),
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
    match cmd {
        UiCommand::Quit => Ok(true),
        UiCommand::ToggleHelp => {
            state.show_help = !state.show_help;
            Ok(true)
        }
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
