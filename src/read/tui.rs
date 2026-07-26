use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::read::scan::{
    load_session_detail, truncate_single_line, Catalog, ProjectRecord, SessionDetail,
    SessionSummary,
};
use crate::usage::{format_compact_kmb, format_duration, LocalUsageSnapshot};
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
use std::collections::BTreeMap;
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
    catalog: Catalog,
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
            catalog,
            view: ViewMode::Projects,
            project_state,
            session_state,
            session_details: BTreeMap::new(),
            error: None,
            layout: UiLayout::default(),
            last_click: None,
        }
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
    let content_area = Rect {
        width: line_area.width.saturating_sub(controls.width),
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
    const WIDTH: u16 = 30;
    let line = header_line_area(area);
    if line.width < WIDTH {
        return Rect::default();
    }
    Rect {
        x: line.x.saturating_add(line.width - WIDTH),
        y: line.y,
        width: WIDTH,
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
        .map(|project| ListItem::new(Line::from(project.display_path.clone())))
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
                .chain(project.sessions.iter().map(|session| {
                    let label = format!(
                        "{}  {}",
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

    let usage_summary =
        usage.and_then(|snapshot| snapshot.project_usage_for_path(&project.display_path));
    let expected_files = project.sessions.len();
    let indexed_files = usage_summary
        .map(|summary| summary.indexed_files)
        .unwrap_or(0);
    let scan_complete = usage.is_some_and(|snapshot| snapshot.scan_pending_files == 0);
    let usage_ready =
        usage_error.is_none() && usage_summary.is_some() && indexed_files >= expected_files;
    let project_scan = if usage_error.is_some() {
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
        let summary = usage_summary.expect("usage summary checked above");
        let non_cached = summary
            .total_tokens
            .saturating_sub(summary.cached_input_tokens)
            .max(0);
        (
            format!(
                "{} total / {} non-cached",
                format_project_count(summary.total_tokens, formatter),
                format_project_count(non_cached, formatter)
            ),
            format!(
                "{} runs / {}",
                formatter.format_count(summary.agent_runs),
                format_duration(summary.agent_time_ms)
            ),
        )
    } else {
        ("--".to_string(), "--".to_string())
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("PATH", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(
                project.display_path.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("SESSIONS", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(formatter.format_usize(project.sessions.len())),
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
            "RECENT",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )),
    ];

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
            "Projects: up/down or wheel, double-click/enter open, s/F2 switch, r/F5 rescan, q quit"
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
    let line = if error.is_empty() {
        Line::from(vec![
            Span::styled(base, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(style, Style::default().fg(Color::Gray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(base, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(style, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(error, Style::default().fg(Color::Red)),
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
        header_line_area, history_style_controls_area, BrowserClickTarget, BrowserState,
        DOUBLE_CLICK_WINDOW,
    };
    use crate::read::scan::Catalog;
    use ratatui::layout::Rect;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

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
            Rect::new(72, 2, 30, 1)
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
}
