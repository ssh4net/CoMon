use crate::read::scan::{
    load_session_detail, truncate_single_line, Catalog, ProjectRecord, SessionDetail,
    SessionSummary,
};
use anyhow::Result;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Projects,
    Sessions,
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
        session_state.select(
            if catalog
                .projects
                .first()
                .map(|project| project.sessions.is_empty())
                .unwrap_or(true)
            {
                None
            } else {
                Some(0)
            },
        );

        Self {
            catalog,
            view: ViewMode::Projects,
            project_state,
            session_state,
            session_details: BTreeMap::new(),
            error: None,
            layout: UiLayout::default(),
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
        self.session_state.selected()
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
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
            if state.view == ViewMode::Sessions {
                state.view = ViewMode::Projects;
                state.error = None;
                return true;
            }
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
                state.view = ViewMode::Sessions;
                sync_session_selection(state);
                try_load_selected_session_detail(state);
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
            if rect_contains(state.layout.project_list_area, mouse.column, mouse.row)
                && click_project_row(state, mouse.row)
            {
                return true;
            }
            if rect_contains(state.layout.session_list_area, mouse.column, mouse.row)
                && click_session_row(state, mouse.row)
            {
                state.view = ViewMode::Sessions;
                try_load_selected_session_detail(state);
                return true;
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
                .map(|project| project.sessions.len())
                .unwrap_or(0);
            if len == 0 {
                return;
            }
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
                .map(|project| project.sessions.len())
                .unwrap_or(0);
            if len == 0 {
                return;
            }
            let index = if first { 0 } else { len.saturating_sub(1) };
            state.session_state.select(Some(index));
            try_load_selected_session_detail(state);
        }
    }
}

fn sync_session_selection(state: &mut BrowserState) {
    let len = state
        .selected_project()
        .map(|project| project.sessions.len())
        .unwrap_or(0);
    if len == 0 {
        state.session_state.select(None);
        return;
    }
    let selected = state.session_state.selected().unwrap_or(0);
    let clamped = selected.min(len.saturating_sub(1));
    state.session_state.select(Some(clamped));
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
    let current = current.unwrap_or(0);
    let current = isize::try_from(current).unwrap_or(0);
    let len = isize::try_from(len).unwrap_or(0);
    let next = (current + delta).clamp(0, len.saturating_sub(1));
    usize::try_from(next).unwrap_or(0)
}

fn click_project_row(state: &mut BrowserState, row: u16) -> bool {
    let content = inner_rect(state.layout.project_list_area);
    if row < content.y || row >= content.y.saturating_add(content.height) {
        return false;
    }
    let current_offset = state.project_state.offset();
    let row_offset = usize::from(row.saturating_sub(content.y));
    let index = current_offset.saturating_add(row_offset);
    if index >= state.catalog.projects.len() {
        return false;
    }
    state.project_state.select(Some(index));
    sync_session_selection(state);
    true
}

fn click_session_row(state: &mut BrowserState, row: u16) -> bool {
    let content = inner_rect(state.layout.session_list_area);
    if row < content.y || row >= content.y.saturating_add(content.height) {
        return false;
    }
    let current_offset = state.session_state.offset();
    let row_offset = usize::from(row.saturating_sub(content.y));
    let index = current_offset.saturating_add(row_offset);
    let len = state
        .selected_project()
        .map(|project| project.sessions.len())
        .unwrap_or(0);
    if index >= len {
        return false;
    }
    state.session_state.select(Some(index));
    true
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut BrowserState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(frame, chunks[0], state);
    state.layout = match state.view {
        ViewMode::Projects => render_projects_view(frame, chunks[1], state),
        ViewMode::Sessions => render_sessions_view(frame, chunks[1], state),
    };
    render_footer(frame, chunks[2], state);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &BrowserState) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(32), Constraint::Length(30)])
        .split(area);

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

    let summary = format!(
        "{} projects  {} files  {} skipped",
        state.catalog.projects.len(),
        state.catalog.files_scanned,
        state.catalog.files_skipped
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            summary,
            Style::default().fg(Color::Gray),
        ))),
        row[1],
    );
}

fn render_projects_view(frame: &mut Frame<'_>, area: Rect, state: &mut BrowserState) -> UiLayout {
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

    let detail_text = render_project_detail(state.selected_project());
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

fn render_sessions_view(frame: &mut Frame<'_>, area: Rect, state: &mut BrowserState) -> UiLayout {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(area);

    let project = state.selected_project();
    let items = project
        .map(|project| {
            project
                .sessions
                .iter()
                .map(|session| {
                    let label = format!(
                        "{}  {}",
                        session.started_at_label,
                        truncate_single_line(&session.title, 64)
                    );
                    ListItem::new(Line::from(label))
                })
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

    let detail_text =
        render_session_detail(state.selected_session(), state.selected_session_detail());
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

fn render_project_detail(project: Option<&ProjectRecord>) -> Text<'static> {
    let Some(project) = project else {
        return Text::from("No projects found.");
    };

    let latest = project
        .sessions
        .first()
        .map(|session| session.started_at_label.clone())
        .unwrap_or_else(|| "--".to_string());

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
            Span::raw(project.sessions.len().to_string()),
        ]),
        Line::from(vec![
            Span::styled("LATEST", Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::raw(latest),
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
            session.started_at_label,
            truncate_single_line(&session.title, 72)
        )));
    }

    Text::from(lines)
}

fn render_session_detail(
    session: Option<&SessionSummary>,
    detail: Option<&SessionDetail>,
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
            Span::raw(session.started_at_label.clone()),
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
                detail.all_user_turns.len(),
                detail.assistant_messages,
                detail.tool_calls,
                detail.tool_outputs,
                detail.input_images
            )),
        ]));
        if let Some(total_tokens) = detail.total_tokens {
            lines.push(Line::from(vec![
                Span::styled("TOKENS", Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::raw(total_tokens.to_string()),
            ]));
        }
        if let Some(input_tokens) = detail.input_tokens {
            lines.push(Line::from(vec![
                Span::styled("INPUT", Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::raw(input_tokens.to_string()),
            ]));
        }
        if let Some(output_tokens) = detail.output_tokens {
            lines.push(Line::from(vec![
                Span::styled("OUTPUT", Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::raw(output_tokens.to_string()),
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

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &BrowserState) {
    let base = match state.view {
        ViewMode::Projects => {
            "Projects: up/down, mouse wheel, enter/right open, s/F2 switch, r/F5 rescan, q quit"
        }
        ViewMode::Sessions => {
            "Sessions: up/down, mouse wheel, backspace/left/esc back, s/F2 switch, r/F5 rescan, q quit"
        }
    };
    let error = state
        .error
        .as_deref()
        .map(|text| truncate_single_line(text, 100))
        .unwrap_or_default();
    let line = if error.is_empty() {
        Line::from(vec![Span::styled(base, Style::default().fg(Color::Gray))])
    } else {
        Line::from(vec![
            Span::styled(base, Style::default().fg(Color::Gray)),
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
