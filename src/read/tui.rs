use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::read::catalog::{
    CatalogProgress, CatalogScanPhase, CatalogSnapshot, ProjectViewMode, SOURCE_NOISY_TREE,
};
use crate::read::scan::{
    load_session_detail, truncate_single_line, Catalog, ProjectRecord, SessionDetail,
    SessionSummary,
};
use crate::usage::{
    format_compact_kmb, format_duration, normalize_project_key, LocalUsageSnapshot,
};
use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Projects,
    Sessions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserClickTarget {
    Project(usize),
    Parent,
    Session(usize),
}

#[derive(Debug, Clone, Copy, Default)]
struct UiLayout {
    project_list_area: Rect,
    session_list_area: Rect,
}

#[derive(Debug)]
pub(crate) struct BrowserState {
    strict_catalog: Catalog,
    catalog: Catalog,
    project_mode: ProjectViewMode,
    deep_depth: u8,
    selected_projects: BTreeSet<String>,
    explicitly_excluded_projects: BTreeSet<String>,
    expanded_remote_groups: BTreeSet<String>,
    discovery: Option<CatalogSnapshot>,
    scan_progress: Option<CatalogProgress>,
    scan_error: Option<String>,
    view: ViewMode,
    project_state: ListState,
    session_state: ListState,
    session_details: BTreeMap<PathBuf, SessionDetail>,
    error: Option<String>,
    layout: UiLayout,
    last_click: Option<(BrowserClickTarget, Instant)>,
}

impl BrowserState {
    pub(crate) fn new(catalog: Catalog) -> Self {
        let mut project_state = ListState::default();
        project_state.select(if catalog.projects.is_empty() {
            None
        } else {
            Some(0)
        });

        let mut session_state = ListState::default();
        session_state.select(Some(
            if catalog
                .projects
                .first()
                .map(|project| project.sessions.is_empty())
                .unwrap_or(true)
            {
                0
            } else {
                1
            },
        ));

        Self {
            strict_catalog: catalog.clone(),
            catalog,
            project_mode: ProjectViewMode::Strict,
            deep_depth: crate::read::catalog::DEFAULT_DEEP_DEPTH,
            selected_projects: BTreeSet::new(),
            explicitly_excluded_projects: BTreeSet::new(),
            expanded_remote_groups: BTreeSet::new(),
            discovery: None,
            scan_progress: None,
            scan_error: None,
            view: ViewMode::Projects,
            project_state,
            session_state,
            session_details: BTreeMap::new(),
            error: None,
            layout: UiLayout::default(),
            last_click: None,
        }
    }

    pub(crate) fn restore_project_state(
        &mut self,
        mode: ProjectViewMode,
        depth: u8,
        selected: BTreeSet<String>,
        excluded: BTreeSet<String>,
        expanded: BTreeSet<String>,
    ) {
        self.project_mode = mode;
        self.deep_depth = depth.clamp(1, crate::read::catalog::MAX_DEEP_DEPTH);
        self.selected_projects = selected;
        self.explicitly_excluded_projects = excluded;
        self.expanded_remote_groups = expanded;
        self.seed_custom_from_strict();
        self.rebuild_visible_catalog();
    }

    pub(crate) fn project_mode(&self) -> ProjectViewMode {
        self.project_mode
    }

    pub(crate) fn deep_depth(&self) -> u8 {
        self.deep_depth
    }

    pub(crate) fn selected_projects(&self) -> &BTreeSet<String> {
        &self.selected_projects
    }

    pub(crate) fn explicitly_excluded_projects(&self) -> &BTreeSet<String> {
        &self.explicitly_excluded_projects
    }

    pub(crate) fn expanded_remote_groups(&self) -> &BTreeSet<String> {
        &self.expanded_remote_groups
    }

    pub(crate) fn strict_catalog_clone(&self) -> Catalog {
        self.strict_catalog.clone()
    }

    pub(crate) fn set_project_mode(&mut self, mode: ProjectViewMode) -> bool {
        if self.project_mode == mode {
            return false;
        }
        self.project_mode = mode;
        self.rebuild_visible_catalog();
        true
    }

    pub(crate) fn cycle_project_mode(&mut self) -> bool {
        self.set_project_mode(self.project_mode.toggled())
    }

    pub(crate) fn change_deep_depth(&mut self, delta: i8) -> bool {
        let current = i16::from(self.deep_depth);
        let next = (current + i16::from(delta))
            .clamp(1, i16::from(crate::read::catalog::MAX_DEEP_DEPTH)) as u8;
        if next == self.deep_depth {
            return false;
        }
        self.deep_depth = next;
        if self.project_mode == ProjectViewMode::Deep {
            self.rebuild_visible_catalog();
        }
        true
    }

    pub(crate) fn apply_catalog_snapshot(&mut self, strict: Catalog, snapshot: CatalogSnapshot) {
        self.strict_catalog = strict;
        self.discovery = Some(snapshot);
        self.scan_progress = None;
        self.scan_error = None;
        self.seed_custom_from_strict();
        self.rebuild_visible_catalog();
    }

    pub(crate) fn set_catalog_progress(&mut self, progress: CatalogProgress) {
        self.scan_progress = Some(progress);
        self.scan_error = None;
    }

    pub(crate) fn set_catalog_error(&mut self, error: String) {
        self.scan_progress = None;
        self.scan_error = Some(error);
    }

    fn seed_custom_from_strict(&mut self) {
        for project in &self.strict_catalog.projects {
            let stable_id = self
                .discovery
                .as_ref()
                .and_then(|snapshot| {
                    snapshot.checkouts.iter().find(|checkout| {
                        checkout.checkout_key == normalize_project_key(&project.display_path)
                    })
                })
                .map(|checkout| checkout.stable_id.as_str())
                .unwrap_or(&project.stable_id);
            if !self.explicitly_excluded_projects.contains(stable_id) {
                self.selected_projects.insert(stable_id.to_string());
            }
        }
    }

    fn toggle_selected_project(&mut self, index: usize) -> bool {
        if self.project_mode != ProjectViewMode::Full {
            return false;
        }
        let Some(stable_id) = self
            .catalog
            .projects
            .get(index)
            .map(|project| project.stable_id.clone())
        else {
            return false;
        };
        if self.selected_projects.remove(&stable_id) {
            self.explicitly_excluded_projects.insert(stable_id);
        } else {
            self.explicitly_excluded_projects.remove(&stable_id);
            self.selected_projects.insert(stable_id);
        }
        true
    }

    fn rebuild_visible_catalog(&mut self) {
        let selected_id = self
            .selected_project()
            .map(|project| project.stable_id.clone());
        self.catalog = match self.project_mode {
            ProjectViewMode::Strict => self.strict_catalog.clone(),
            mode => build_discovery_catalog(
                &self.strict_catalog,
                self.discovery.as_ref(),
                mode,
                self.deep_depth,
                &self.selected_projects,
            ),
        };
        let next_index = selected_id
            .as_deref()
            .and_then(|stable_id| {
                self.catalog
                    .projects
                    .iter()
                    .position(|project| project.stable_id == stable_id)
            })
            .or_else(|| (!self.catalog.projects.is_empty()).then_some(0));
        self.project_state.select(next_index);
        sync_session_selection(self);
    }

    fn selected_project_index(&self) -> Option<usize> {
        self.project_state.selected()
    }

    fn selected_project(&self) -> Option<&ProjectRecord> {
        let index = self.selected_project_index()?;
        self.catalog.projects.get(index)
    }

    fn selected_session_index(&self) -> Option<usize> {
        self.session_state.selected()?.checked_sub(1)
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        let project = self.selected_project()?;
        let index = self.selected_session_index()?;
        project.sessions.get(index)
    }

    fn selected_session_detail(&self) -> Option<&SessionDetail> {
        let session = self.selected_session()?;
        self.session_details.get(&session.file_path)
    }

    fn ensure_selected_session_detail(&mut self) -> Result<()> {
        let Some(path) = self
            .selected_session()
            .map(|session| session.file_path.clone())
        else {
            return Ok(());
        };
        if self.session_details.contains_key(&path) {
            return Ok(());
        }
        let detail = load_session_detail(&path)?;
        self.session_details.insert(path, detail);
        Ok(())
    }

    fn register_click(&mut self, target: BrowserClickTarget, now: Instant) -> bool {
        let is_double = self.last_click.is_some_and(|(previous, at)| {
            previous == target
                && now
                    .checked_duration_since(at)
                    .is_some_and(|elapsed| elapsed <= DOUBLE_CLICK_WINDOW)
        });
        self.last_click = if is_double { None } else { Some((target, now)) };
        is_double
    }
}

#[derive(Default)]
struct LogicalProjectBuilder {
    stable_id: String,
    logical_name: String,
    checkouts: Vec<String>,
    checkout_keys: BTreeSet<String>,
    owner_sessions: Vec<SessionSummary>,
    related_sessions: Vec<SessionSummary>,
    confidence: u8,
    source_flags: u32,
    missing: bool,
}

fn build_discovery_catalog(
    strict: &Catalog,
    discovery: Option<&CatalogSnapshot>,
    mode: ProjectViewMode,
    deep_depth: u8,
    selected_projects: &BTreeSet<String>,
) -> Catalog {
    let Some(discovery) = discovery else {
        return strict.clone();
    };
    let mut sessions_by_path = BTreeMap::new();
    let mut strict_by_checkout = BTreeMap::new();
    for (project_index, project) in strict.projects.iter().enumerate() {
        strict_by_checkout.insert(normalize_project_key(&project.display_path), project_index);
        for session in &project.sessions {
            sessions_by_path.insert(
                session.file_path.to_string_lossy().into_owned(),
                session.clone(),
            );
        }
    }

    let mut groups: BTreeMap<String, LogicalProjectBuilder> = BTreeMap::new();
    let mut checkout_to_group = BTreeMap::new();
    for checkout in &discovery.checkouts {
        let include = match mode {
            ProjectViewMode::Strict => false,
            ProjectViewMode::Deep => {
                !checkout.missing
                    && checkout.discovery_depth <= deep_depth
                    && (checkout.deep_eligible || selected_projects.contains(&checkout.stable_id))
            }
            ProjectViewMode::Full => true,
            ProjectViewMode::Custom => selected_projects.contains(&checkout.stable_id),
        };
        if !include {
            continue;
        }
        let group =
            groups
                .entry(checkout.stable_id.clone())
                .or_insert_with(|| LogicalProjectBuilder {
                    stable_id: checkout.stable_id.clone(),
                    logical_name: checkout.logical_name.clone(),
                    missing: true,
                    ..LogicalProjectBuilder::default()
                });
        group.checkouts.push(checkout.display_path.clone());
        group.checkout_keys.insert(checkout.checkout_key.clone());
        group.confidence = group.confidence.max(checkout.confidence);
        group.source_flags |= checkout.source_flags;
        group.missing &= checkout.missing;
        checkout_to_group.insert(checkout.checkout_key.clone(), checkout.stable_id.clone());
    }

    let mut consumed_strict = BTreeSet::new();
    for (stable_id, group) in groups.iter_mut() {
        for checkout_key in &group.checkout_keys {
            let Some(index) = strict_by_checkout.get(checkout_key).copied() else {
                continue;
            };
            consumed_strict.insert(index);
            group
                .owner_sessions
                .extend(strict.projects[index].sessions.iter().cloned());
            group.source_flags |= crate::read::catalog::SOURCE_OWNER;
            group.confidence = 100;
        }
        if selected_projects.contains(stable_id) {
            group.source_flags |= crate::read::catalog::SOURCE_USER_SELECTED;
        }
    }

    for link in &discovery.links {
        let Some(stable_id) = checkout_to_group.get(&link.checkout_key) else {
            continue;
        };
        let Some(session) = sessions_by_path.get(&link.session_path) else {
            continue;
        };
        let Some(group) = groups.get_mut(stable_id) else {
            continue;
        };
        if group
            .owner_sessions
            .iter()
            .any(|owner| owner.file_path == session.file_path)
            || group
                .related_sessions
                .iter()
                .any(|related| related.file_path == session.file_path)
        {
            continue;
        }
        group.related_sessions.push(session.clone());
    }

    let mut projects = groups
        .into_values()
        .map(|mut group| {
            group.checkouts.sort();
            group.checkouts.dedup();
            group.owner_sessions.sort_by(session_order);
            group.related_sessions.sort_by(session_order);
            let owner_session_count = group.owner_sessions.len();
            let mut sessions = group.owner_sessions;
            sessions.extend(group.related_sessions);
            let display_path = group
                .checkouts
                .first()
                .cloned()
                .unwrap_or_else(|| group.logical_name.clone());
            ProjectRecord {
                stable_id: group.stable_id,
                logical_name: group.logical_name,
                display_path,
                checkouts: group.checkouts,
                sessions,
                owner_session_count,
                confidence: group.confidence,
                source_flags: group.source_flags,
                missing: group.missing,
            }
        })
        .collect::<Vec<_>>();

    for (index, project) in strict.projects.iter().enumerate() {
        if consumed_strict.contains(&index) {
            continue;
        }
        let include = match mode {
            ProjectViewMode::Strict => true,
            ProjectViewMode::Deep | ProjectViewMode::Full => true,
            ProjectViewMode::Custom => {
                let checkout_key = normalize_project_key(&project.display_path);
                let effective_id = discovery
                    .checkouts
                    .iter()
                    .find(|checkout| checkout.checkout_key == checkout_key)
                    .map(|checkout| checkout.stable_id.as_str())
                    .unwrap_or(&project.stable_id);
                selected_projects.contains(effective_id)
            }
        };
        if include {
            projects.push(project.clone());
        }
    }
    projects.sort_by(|left, right| {
        left.logical_name
            .to_ascii_lowercase()
            .cmp(&right.logical_name.to_ascii_lowercase())
            .then_with(|| left.display_path.cmp(&right.display_path))
    });
    Catalog {
        sessions_dir: strict.sessions_dir.clone(),
        projects,
        files_scanned: strict.files_scanned,
        files_skipped: strict.files_skipped,
    }
}

fn session_order(left: &SessionSummary, right: &SessionSummary) -> std::cmp::Ordering {
    right
        .started_at_sort_key_ms
        .cmp(&left.started_at_sort_key_ms)
        .then_with(|| left.file_path.cmp(&right.file_path))
}

pub(crate) fn handle_event(state: &mut BrowserState, event: Event) -> Result<bool> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }
            Ok(handle_key_event(state, key.code))
        }
        Event::Mouse(mouse) => Ok(handle_mouse_event(state, mouse)),
        Event::Resize(_, _) => Ok(true),
        _ => Ok(false),
    }
}

fn handle_key_event(state: &mut BrowserState, code: KeyCode) -> bool {
    match code {
        KeyCode::Char('p') | KeyCode::Char('P') if state.view == ViewMode::Projects => {
            return state.cycle_project_mode();
        }
        KeyCode::Char(' ') if state.view == ViewMode::Projects => {
            return state
                .selected_project_index()
                .is_some_and(|index| state.toggle_selected_project(index));
        }
        KeyCode::Char('+') | KeyCode::Char('=') if state.view == ViewMode::Projects => {
            return state.change_deep_depth(1);
        }
        KeyCode::Char('-') if state.view == ViewMode::Projects => {
            return state.change_deep_depth(-1);
        }
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Left if state.view == ViewMode::Sessions => {
            close_selected_project(state);
            return true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_selection(state, -1);
            return true;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_selection(state, 1);
            return true;
        }
        KeyCode::PageUp => {
            move_selection(state, -10);
            return true;
        }
        KeyCode::PageDown => {
            move_selection(state, 10);
            return true;
        }
        KeyCode::Home => {
            jump_to_edge(state, true);
            return true;
        }
        KeyCode::End => {
            jump_to_edge(state, false);
            return true;
        }
        KeyCode::Enter | KeyCode::Right => {
            if state.view == ViewMode::Projects && state.selected_project().is_some() {
                open_selected_project(state);
                return true;
            }
            if state.view == ViewMode::Sessions {
                if state.session_state.selected() == Some(0) {
                    close_selected_project(state);
                } else {
                    try_load_selected_session_detail(state);
                }
                return true;
            }
        }
        _ => {}
    }

    false
}

fn handle_mouse_event(state: &mut BrowserState, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            move_selection(state, -1);
            true
        }
        MouseEventKind::ScrollDown => {
            move_selection(state, 1);
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();
            if rect_contains(state.layout.project_list_area, mouse.column, mouse.row) {
                if let Some(index) = click_project_row(state, mouse.row) {
                    let content = inner_rect(state.layout.project_list_area);
                    if state.project_mode == ProjectViewMode::Full
                        && mouse.column >= content.x.saturating_add(3)
                        && mouse.column < content.x.saturating_add(7)
                    {
                        state.last_click = None;
                        return state.toggle_selected_project(index);
                    }
                    if state.register_click(BrowserClickTarget::Project(index), now) {
                        open_selected_project(state);
                    }
                    return true;
                }
            }
            if rect_contains(state.layout.session_list_area, mouse.column, mouse.row) {
                if let Some(target) = click_session_row(state, mouse.row) {
                    let double_click = state.register_click(target, now);
                    if target == BrowserClickTarget::Parent && double_click {
                        close_selected_project(state);
                    } else {
                        try_load_selected_session_detail(state);
                    }
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn move_selection(state: &mut BrowserState, delta: isize) {
    match state.view {
        ViewMode::Projects => {
            let len = state.catalog.projects.len();
            if len == 0 {
                return;
            }
            let next = advance_index(state.project_state.selected(), len, delta);
            state.project_state.select(Some(next));
            sync_session_selection(state);
        }
        ViewMode::Sessions => {
            let len = state
                .selected_project()
                .map(|project| project.sessions.len().saturating_add(1))
                .unwrap_or(1);
            let next = advance_index(state.session_state.selected(), len, delta);
            state.session_state.select(Some(next));
            try_load_selected_session_detail(state);
        }
    }
}

fn jump_to_edge(state: &mut BrowserState, first: bool) {
    match state.view {
        ViewMode::Projects => {
            let len = state.catalog.projects.len();
            if len == 0 {
                return;
            }
            let index = if first { 0 } else { len.saturating_sub(1) };
            state.project_state.select(Some(index));
            sync_session_selection(state);
        }
        ViewMode::Sessions => {
            let len = state
                .selected_project()
                .map(|project| project.sessions.len().saturating_add(1))
                .unwrap_or(1);
            let index = if first { 0 } else { len.saturating_sub(1) };
            state.session_state.select(Some(index));
            try_load_selected_session_detail(state);
        }
    }
}

fn sync_session_selection(state: &mut BrowserState) {
    let len = state
        .selected_project()
        .map(|project| project.sessions.len().saturating_add(1))
        .unwrap_or(1);
    let selected = state.session_state.selected().unwrap_or(1);
    let clamped = selected.min(len.saturating_sub(1));
    state.session_state.select(Some(clamped));
}

fn open_selected_project(state: &mut BrowserState) {
    if state.selected_project().is_none() {
        return;
    }
    state.view = ViewMode::Sessions;
    state.session_state.select(Some(
        if state
            .selected_project()
            .is_some_and(|project| project.sessions.is_empty())
        {
            0
        } else {
            1
        },
    ));
    state.last_click = None;
    try_load_selected_session_detail(state);
}

fn close_selected_project(state: &mut BrowserState) {
    state.view = ViewMode::Projects;
    state.error = None;
    state.last_click = None;
}

fn try_load_selected_session_detail(state: &mut BrowserState) {
    if state.selected_session().is_none() {
        state.error = None;
        return;
    }
    match state.ensure_selected_session_detail() {
        Ok(()) => state.error = None,
        Err(err) => state.error = Some(err.to_string()),
    }
}

fn advance_index(current: Option<usize>, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let current = current.unwrap_or(0);
    let current = isize::try_from(current).unwrap_or(0);
    let len = isize::try_from(len).unwrap_or(0);
    let next = (current + delta).clamp(0, len.saturating_sub(1));
    usize::try_from(next).unwrap_or(0)
}

fn click_project_row(state: &mut BrowserState, row: u16) -> Option<usize> {
    let content = inner_rect(state.layout.project_list_area);
    if row < content.y || row >= content.y.saturating_add(content.height) {
        return None;
    }
    let current_offset = state.project_state.offset();
    let row_offset = usize::from(row.saturating_sub(content.y));
    let index = current_offset.saturating_add(row_offset);
    if index >= state.catalog.projects.len() {
        return None;
    }
    state.project_state.select(Some(index));
    sync_session_selection(state);
    Some(index)
}

fn click_session_row(state: &mut BrowserState, row: u16) -> Option<BrowserClickTarget> {
    let content = inner_rect(state.layout.session_list_area);
    if row < content.y || row >= content.y.saturating_add(content.height) {
        return None;
    }
    let current_offset = state.session_state.offset();
    let row_offset = usize::from(row.saturating_sub(content.y));
    let index = current_offset.saturating_add(row_offset);
    let len = state
        .selected_project()
        .map(|project| project.sessions.len().saturating_add(1))
        .unwrap_or(1);
    if index >= len {
        return None;
    }
    state.session_state.select(Some(index));
    if index == 0 {
        Some(BrowserClickTarget::Parent)
    } else {
        Some(BrowserClickTarget::Session(index - 1))
    }
}

pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut BrowserState,
    usage: Option<&LocalUsageSnapshot>,
    usage_error: Option<&str>,
    formatter: DisplayFormatter<'_>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(frame, chunks[0], state, formatter);
    state.layout = match state.view {
        ViewMode::Projects => {
            render_projects_view(frame, chunks[1], state, usage, usage_error, formatter)
        }
        ViewMode::Sessions => render_sessions_view(frame, chunks[1], state, formatter),
    };
    render_footer(frame, chunks[2], state, formatter);
}

fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &BrowserState,
    formatter: DisplayFormatter<'_>,
) {
    let line_area = header_line_area(area);
    let controls = history_style_controls_area(area);
    let controls_gap = if controls.width > 0 { 2 } else { 0 };
    let content_area = Rect {
        width: line_area
            .width
            .saturating_sub(controls.width)
            .saturating_sub(controls_gap),
        ..line_area
    };
    let summary = format!(
        "{} projects  {} files  {} skipped",
        formatter.format_usize(state.catalog.projects.len()),
        formatter.format_usize(state.catalog.files_scanned),
        formatter.format_usize(state.catalog.files_skipped)
    );
    let summary_width = u16::try_from(UnicodeWidthStr::width(summary.as_str())).unwrap_or(u16::MAX);
    let show_summary = content_area.width >= 32u16.saturating_add(summary_width);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if show_summary {
            [Constraint::Min(32), Constraint::Length(summary_width)]
        } else {
            [Constraint::Min(0), Constraint::Length(0)]
        })
        .split(content_area);

    let scope = format!(
        "SESSION_HISTORY {}",
        truncate_single_line(&state.catalog.sessions_dir.display().to_string(), 64)
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            scope,
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        row[0],
    );

    if show_summary {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                summary,
                Style::default().fg(Color::Gray),
            ))),
            row[1],
        );
    }
}

fn header_line_area(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y.saturating_add(area.height.min(2).saturating_sub(1)),
        width: area.width,
        height: area.height.min(1),
    }
}

pub(crate) fn history_style_controls_area(area: Rect) -> Rect {
    let line = header_line_area(area);
    let width = if line.width >= 68 {
        68
    } else if line.width >= 30 {
        30
    } else {
        0
    };
    if width == 0 {
        return Rect::default();
    }
    Rect {
        x: line.x.saturating_add(line.width - width),
        y: line.y,
        width,
        height: 1,
    }
}

pub(crate) fn history_depth_controls_area(area: Rect) -> Rect {
    let controls = history_style_controls_area(area);
    if controls.width < 68 {
        return Rect::default();
    }
    Rect {
        x: controls.x.saturating_add(18),
        y: controls.y.saturating_add(1),
        width: 16,
        height: 1,
    }
}

fn render_projects_view(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut BrowserState,
    usage: Option<&LocalUsageSnapshot>,
    usage_error: Option<&str>,
    formatter: DisplayFormatter<'_>,
) -> UiLayout {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    let items = state
        .catalog
        .projects
        .iter()
        .map(|project| {
            let checkbox = if state.project_mode == ProjectViewMode::Full {
                if state.selected_projects.contains(&project.stable_id) {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let checkout_count = if project.checkouts.len() > 1 {
                format!("  ({})", formatter.format_usize(project.checkouts.len()))
            } else {
                String::new()
            };
            let marker = if project.missing {
                "  MISSING"
            } else if project.source_flags & SOURCE_NOISY_TREE != 0 && project.confidence < 70 {
                "  LOW"
            } else {
                ""
            };
            ListItem::new(Line::from(format!(
                "{checkbox}{}{checkout_count}{marker}",
                project.logical_name
            )))
        })
        .collect::<Vec<_>>();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Projects ")
                .border_style(active_border(state.view == ViewMode::Projects)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");
    frame.render_stateful_widget(list, columns[0], &mut state.project_state);

    let detail_text =
        render_project_detail(state.selected_project(), usage, usage_error, formatter);
    frame.render_widget(
        Paragraph::new(detail_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Project Detail "),
            )
            .wrap(Wrap { trim: false }),
        columns[1],
    );

    UiLayout {
        project_list_area: columns[0],
        session_list_area: Rect::default(),
    }
}

fn render_sessions_view(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut BrowserState,
    formatter: DisplayFormatter<'_>,
) -> UiLayout {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(area);

    let project = state.selected_project();
    let items = project
        .map(|project| {
            std::iter::once(ListItem::new(Line::from("..")))
                .chain(project.sessions.iter().enumerate().map(|(index, session)| {
                    let relation = if index < project.owner_session_count {
                        "O"
                    } else {
                        "R"
                    };
                    let label = format!(
                        "{relation}  {}  {}",
                        session_started_label(session, formatter),
                        truncate_single_line(&session.title, 64)
                    );
                    ListItem::new(Line::from(label))
                }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Sessions ")
                .border_style(active_border(true)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");
    frame.render_stateful_widget(list, columns[0], &mut state.session_state);

    let detail_text = render_session_detail(
        state.selected_session(),
        state.selected_session_detail(),
        formatter,
    );
    frame.render_widget(
        Paragraph::new(detail_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Session Detail "),
            )
            .wrap(Wrap { trim: false }),
        columns[1],
    );

    UiLayout {
        project_list_area: Rect::default(),
        session_list_area: columns[0],
    }
}

fn render_project_detail(
    project: Option<&ProjectRecord>,
    usage: Option<&LocalUsageSnapshot>,
    usage_error: Option<&str>,
    formatter: DisplayFormatter<'_>,
) -> Text<'static> {
    let Some(project) = project else {
        return Text::from("No projects found.");
    };

    let latest = project
        .sessions
        .first()
        .map(|session| session_started_label(session, formatter))
        .unwrap_or_else(|| "--".to_string());

    let mut usage_totals = (0i64, 0i64, 0i64, 0i64, 0usize);
    let mut usage_paths = BTreeSet::new();
    if let Some(snapshot) = usage {
        for path in &project.checkouts {
            let key = normalize_project_key(path);
            if !usage_paths.insert(key) {
                continue;
            }
            if let Some(summary) = snapshot.project_usage_for_path(path) {
                usage_totals.0 = usage_totals.0.saturating_add(summary.total_tokens);
                usage_totals.1 = usage_totals.1.saturating_add(summary.cached_input_tokens);
                usage_totals.2 = usage_totals.2.saturating_add(summary.agent_time_ms);
                usage_totals.3 = usage_totals.3.saturating_add(summary.agent_runs);
                usage_totals.4 = usage_totals.4.saturating_add(summary.indexed_files);
            }
        }
    }
    let expected_files = project.owner_session_count;
    let indexed_files = usage_totals.4;
    let scan_complete = usage.is_some_and(|snapshot| snapshot.scan_pending_files == 0);
    let usage_ready =
        usage_error.is_none() && expected_files > 0 && indexed_files >= expected_files;
    let project_scan = if expected_files == 0 {
        "NO OWNER DATA".to_string()
    } else if usage_error.is_some() {
        "ERROR".to_string()
    } else if usage_ready {
        "READY".to_string()
    } else if scan_complete {
        "NO DATA".to_string()
    } else {
        format!(
            "SCANNING {}/{}",
            formatter.format_usize(indexed_files),
            formatter.format_usize(expected_files)
        )
    };

    let (consumed_tokens, activity) = if usage_ready {
        let non_cached = usage_totals.0.saturating_sub(usage_totals.1).max(0);
        (
            format!(
                "{} total / {} non-cached",
                format_project_count(usage_totals.0, formatter),
                format_project_count(non_cached, formatter)
            ),
            format!(
                "{} runs / {}",
                formatter.format_count(usage_totals.3),
                format_duration(usage_totals.2)
            ),
        )
    } else {
        ("--".to_string(), "--".to_string())
    };
    let discovery_label = if project.missing {
        "MISSING".to_string()
    } else if project.source_flags & crate::read::catalog::SOURCE_OWNER != 0 {
        format!("OWNER | confidence {}", project.confidence)
    } else if project.confidence >= 70 {
        format!("STRONG | confidence {}", project.confidence)
    } else {
        format!("LOW | confidence {}", project.confidence)
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("PROJECT", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(
                project.logical_name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("OWNER_SESSIONS", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(formatter.format_usize(project.owner_session_count)),
        ]),
        Line::from(vec![
            Span::styled("RELATED_SESSIONS", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(
                formatter.format_usize(
                    project
                        .sessions
                        .len()
                        .saturating_sub(project.owner_session_count),
                ),
            ),
        ]),
        Line::from(vec![
            Span::styled("DISCOVERY", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(discovery_label),
        ]),
        Line::from(vec![
            Span::styled("LATEST", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(latest),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("CURRENT_CONSUMED_TOKENS", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(
                consumed_tokens,
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("PROJECT_SCAN", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(project_scan),
        ]),
        Line::from(vec![
            Span::styled("ACTIVITY", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(activity),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "CHECKOUTS",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    for checkout in &project.checkouts {
        lines.push(Line::from(format!("- {checkout}")));
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "RECENT",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )),
    ]);

    for session in project.sessions.iter().take(10) {
        lines.push(Line::from(format!(
            "{}  {}",
            session_started_label(session, formatter),
            truncate_single_line(&session.title, 72)
        )));
    }

    Text::from(lines)
}

fn format_project_count(value: i64, formatter: DisplayFormatter<'_>) -> String {
    match formatter.style() {
        DisplayStyle::SystemCompact => format_compact_kmb(value.max(0) as u64, 16, formatter),
        DisplayStyle::Classic | DisplayStyle::SystemFull => formatter.format_count(value),
    }
}

fn render_session_detail(
    session: Option<&SessionSummary>,
    detail: Option<&SessionDetail>,
    formatter: DisplayFormatter<'_>,
) -> Text<'static> {
    let Some(session) = session else {
        return Text::from("No sessions in this project.");
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("TITLE", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(
                session.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("STARTED", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(session_started_label(session, formatter)),
        ]),
        Line::from(vec![
            Span::styled("SESSION", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(session.session_id.clone()),
        ]),
        Line::from(vec![
            Span::styled("PATH", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(truncate_single_line(
                &session.file_path.display().to_string(),
                96,
            )),
        ]),
        Line::from(vec![
            Span::styled("MODEL", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(
                session
                    .model
                    .clone()
                    .or_else(|| session.model_provider.clone())
                    .unwrap_or_else(|| "--".to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("BRANCH", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(
                session
                    .git_branch
                    .clone()
                    .unwrap_or_else(|| "--".to_string()),
            ),
        ]),
    ];

    if let Some(commit) = &session.git_commit {
        lines.push(Line::from(vec![
            Span::styled("COMMIT", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(truncate_single_line(commit, 24)),
        ]));
    }

    if let Some(repo_url) = &session.repo_url {
        lines.push(Line::from(vec![
            Span::styled("REMOTE", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(truncate_single_line(repo_url, 72)),
        ]));
    }

    if let Some(raw) = &session.started_at_raw {
        lines.push(Line::from(vec![
            Span::styled("TIMESTAMP", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(raw.clone()),
        ]));
    }

    if let Some(detail) = detail {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "COUNTS",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  users={} assistant={} calls={} outputs={} images={}",
                formatter.format_usize(detail.all_user_turns.len()),
                formatter.format_usize(detail.assistant_messages),
                formatter.format_usize(detail.tool_calls),
                formatter.format_usize(detail.tool_outputs),
                formatter.format_usize(detail.input_images)
            )),
        ]));
        if let Some(total_tokens) = detail.total_tokens {
            lines.push(Line::from(vec![
                Span::styled("TOKENS", Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::raw(formatter.format_count(total_tokens)),
            ]));
        }
        if let Some(input_tokens) = detail.input_tokens {
            lines.push(Line::from(vec![
                Span::styled("INPUT", Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::raw(formatter.format_count(input_tokens)),
            ]));
        }
        if let Some(output_tokens) = detail.output_tokens {
            lines.push(Line::from(vec![
                Span::styled("OUTPUT", Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::raw(formatter.format_count(output_tokens)),
            ]));
        }
        if detail.reasoning_encrypted {
            lines.push(Line::from(vec![
                Span::styled("REASONING", Style::default().fg(Color::Gray)),
                Span::raw("  encrypted"),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "USER TURNS",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )));
        let turns = if detail.meaningful_user_turns.is_empty() {
            &detail.all_user_turns
        } else {
            &detail.meaningful_user_turns
        };
        if turns.is_empty() {
            lines.push(Line::from("--"));
        } else {
            for turn in turns.iter().take(16) {
                lines.push(Line::from(format!("- {}", turn)));
            }
        }
    }

    Text::from(lines)
}

fn session_started_label(session: &SessionSummary, formatter: DisplayFormatter<'_>) -> String {
    if formatter.style() == DisplayStyle::Classic {
        return session.started_at_label.clone();
    }
    let Some(raw) = session.started_at_raw.as_deref() else {
        return session.started_at_label.clone();
    };
    let Some(parsed) = DateTime::parse_from_rfc3339(raw).ok() else {
        return session.started_at_label.clone();
    };
    let local = parsed.with_timezone(&Local);
    formatter.format_session_datetime(local.naive_local())
}

fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &BrowserState,
    formatter: DisplayFormatter<'_>,
) {
    let base = match state.view {
        ViewMode::Projects => {
            "Projects: [p] mode, [+/-] depth, [space] select in FULL, wheel, double-click/enter open, r/F5 rescan"
        }
        ViewMode::Sessions => {
            "Sessions: up/down or wheel, double-click .. / backspace / left / esc back, q quit"
        }
    };
    let error = state
        .error
        .as_deref()
        .map(|text| truncate_single_line(text, 100))
        .unwrap_or_default();
    let style = format!("style [n]: {}", formatter.style_label());
    let catalog_status = if let Some(progress) = &state.scan_progress {
        let phase = match progress.phase {
            CatalogScanPhase::Repositories => "REPO_SCAN",
            CatalogScanPhase::Sessions => "PROJECT_SCAN",
            CatalogScanPhase::Saving => "CATALOG_SAVE",
        };
        if progress.total > 0 {
            format!(
                "{phase} {}/{} ({} projects)",
                progress.completed, progress.total, progress.projects
            )
        } else {
            format!(
                "{phase} {} dirs ({} projects)",
                progress.completed, progress.projects
            )
        }
    } else if let Some(scan_error) = &state.scan_error {
        format!(
            "PROJECT_SCAN ERROR: {}",
            truncate_single_line(scan_error, 80)
        )
    } else if state.discovery.as_ref().is_some_and(|snapshot| {
        snapshot.sessions_total > 0 && snapshot.sessions_scanned < snapshot.sessions_total
    }) {
        let snapshot = state.discovery.as_ref().expect("snapshot checked above");
        format!(
            "PROJECT_INDEX {}/{}",
            snapshot.sessions_scanned, snapshot.sessions_total
        )
    } else if state
        .discovery
        .as_ref()
        .is_some_and(|snapshot| snapshot.truncated)
    {
        "FULL SCAN REACHED DIRECTORY LIMIT".to_string()
    } else if state.project_mode == ProjectViewMode::Deep {
        format!("DEEP depth {}", state.deep_depth)
    } else {
        String::new()
    };
    let line = if error.is_empty() && catalog_status.is_empty() {
        Line::from(vec![
            Span::styled(base, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(style, Style::default().fg(Color::Gray)),
        ])
    } else if !error.is_empty() {
        Line::from(vec![
            Span::styled(base, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(style, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(error, Style::default().fg(Color::Red)),
        ])
    } else {
        Line::from(vec![
            Span::styled(base, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(style, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(catalog_status, Style::default().fg(Color::Cyan)),
        ])
    };
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), area);
}

fn active_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[cfg(test)]
mod tests {
    use super::{
        build_discovery_catalog, header_line_area, history_depth_controls_area,
        history_style_controls_area, BrowserClickTarget, BrowserState, DOUBLE_CLICK_WINDOW,
    };
    use crate::read::catalog::{
        CatalogSnapshot, ProjectCheckout, ProjectViewMode, SessionProjectLink, SOURCE_REPOSITORY,
        SOURCE_WORKDIR,
    };
    use crate::read::scan::{Catalog, ProjectRecord, SessionSummary};
    use ratatui::layout::Rect;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn session(path: &str, cwd: &str) -> SessionSummary {
        SessionSummary {
            file_path: PathBuf::from(path),
            session_id: path.to_string(),
            cwd: cwd.to_string(),
            title: path.to_string(),
            started_at_raw: None,
            started_at_label: "--".to_string(),
            started_at_sort_key_ms: 0,
            git_branch: None,
            git_commit: None,
            repo_url: None,
            model_provider: None,
            model: None,
        }
    }

    fn strict_project(path: &str, session: SessionSummary) -> ProjectRecord {
        ProjectRecord {
            stable_id: format!("path:{path}"),
            logical_name: PathBuf::from(path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(path)
                .to_string(),
            display_path: path.to_string(),
            checkouts: vec![path.to_string()],
            sessions: vec![session],
            owner_session_count: 1,
            confidence: 100,
            source_flags: crate::read::catalog::SOURCE_OWNER,
            missing: false,
        }
    }

    #[test]
    fn header_line_has_one_blank_row_above_and_below() {
        assert_eq!(
            header_line_area(Rect::new(2, 1, 100, 3)),
            Rect::new(2, 2, 100, 1)
        );
    }

    #[test]
    fn history_style_controls_use_right_side_of_header_line() {
        assert_eq!(
            history_style_controls_area(Rect::new(2, 1, 100, 3)),
            Rect::new(34, 2, 68, 1)
        );
        assert_eq!(
            history_depth_controls_area(Rect::new(2, 1, 100, 3)),
            Rect::new(52, 3, 16, 1)
        );
        assert_eq!(
            history_style_controls_area(Rect::new(2, 1, 20, 3)),
            Rect::default()
        );
    }

    #[test]
    fn identical_clicks_within_window_are_double_clicks() {
        let mut state = BrowserState::new(Catalog {
            sessions_dir: PathBuf::from("/tmp/sessions"),
            projects: Vec::new(),
            files_scanned: 0,
            files_skipped: 0,
        });
        let now = Instant::now();
        assert!(!state.register_click(BrowserClickTarget::Project(2), now));
        assert!(state.register_click(BrowserClickTarget::Project(2), now + DOUBLE_CLICK_WINDOW));
        assert!(!state.register_click(BrowserClickTarget::Parent, now));
        assert!(!state.register_click(
            BrowserClickTarget::Parent,
            now + DOUBLE_CLICK_WINDOW + Duration::from_millis(1)
        ));
    }

    #[test]
    fn discovery_modes_preserve_exactly_one_owner_per_strict_session() {
        let strict = Catalog {
            sessions_dir: PathBuf::from("/sessions"),
            projects: vec![
                strict_project("/repo/a", session("/sessions/a.jsonl", "/repo/a")),
                strict_project("/launcher", session("/sessions/b.jsonl", "/launcher")),
            ],
            files_scanned: 2,
            files_skipped: 0,
        };
        let snapshot = CatalogSnapshot {
            checkouts: vec![ProjectCheckout {
                stable_id: "remote:example.com/team/a".to_string(),
                checkout_key: "/repo/a".to_string(),
                display_path: "/repo/a".to_string(),
                remote_key: Some("example.com/team/a".to_string()),
                logical_name: "team/a".to_string(),
                discovery_depth: 1,
                source_flags: SOURCE_REPOSITORY | SOURCE_WORKDIR,
                confidence: 80,
                deep_eligible: true,
                first_seen: 1,
                last_seen: 1,
                missing: false,
            }],
            links: vec![SessionProjectLink {
                session_path: "/sessions/b.jsonl".to_string(),
                checkout_key: "/repo/a".to_string(),
                evidence_mask: SOURCE_WORKDIR,
                evidence_count: 1,
                confidence: 80,
            }],
            ..CatalogSnapshot::default()
        };
        for mode in [ProjectViewMode::Deep, ProjectViewMode::Full] {
            let catalog =
                build_discovery_catalog(&strict, Some(&snapshot), mode, 2, &BTreeSet::new());
            assert_eq!(
                catalog
                    .projects
                    .iter()
                    .map(|project| project.owner_session_count)
                    .sum::<usize>(),
                2
            );
            let logical = catalog
                .projects
                .iter()
                .find(|project| project.stable_id == "remote:example.com/team/a")
                .expect("logical project");
            assert_eq!(logical.owner_session_count, 1);
            assert_eq!(logical.sessions.len(), 2);
        }
    }

    #[test]
    fn custom_selection_respects_explicit_remote_exclusion() {
        let strict = Catalog {
            sessions_dir: PathBuf::from("/sessions"),
            projects: vec![strict_project(
                "/repo/a",
                session("/sessions/a.jsonl", "/repo/a"),
            )],
            files_scanned: 1,
            files_skipped: 0,
        };
        let snapshot = CatalogSnapshot {
            checkouts: vec![ProjectCheckout {
                stable_id: "remote:example.com/team/a".to_string(),
                checkout_key: "/repo/a".to_string(),
                display_path: "/repo/a".to_string(),
                remote_key: Some("example.com/team/a".to_string()),
                logical_name: "team/a".to_string(),
                discovery_depth: 1,
                source_flags: SOURCE_REPOSITORY | SOURCE_WORKDIR,
                confidence: 80,
                deep_eligible: true,
                first_seen: 1,
                last_seen: 1,
                missing: false,
            }],
            ..CatalogSnapshot::default()
        };
        let mut browser = BrowserState::new(strict.clone());
        browser.restore_project_state(
            ProjectViewMode::Custom,
            2,
            BTreeSet::new(),
            BTreeSet::from(["remote:example.com/team/a".to_string()]),
            BTreeSet::new(),
        );
        browser.apply_catalog_snapshot(strict, snapshot);
        assert!(browser.catalog.projects.is_empty());
        assert!(!browser
            .selected_projects
            .contains("remote:example.com/team/a"));
    }
}
