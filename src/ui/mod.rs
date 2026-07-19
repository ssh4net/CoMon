use crate::app::{ActiveScreen, AppState, ChartOrientation};
use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::usage::{
    format_compact_kmb, format_count, format_duration_compact, format_tokens_compact,
    format_tokens_overview, ChartRange, ProjectActivity, UsageMetric, ACTIVITY_TIMELINE_WEEKS,
};
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, TimeZone, Weekday};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

const ACTIVITY_PROJECT_HEIGHT: u16 = 9;
const ACTIVITY_PROJECT_STRIDE: u16 = 10;
const ACTIVITY_WEEKDAY_LABEL_WIDTH: u16 = 4;
const ACTIVITY_COLORS: [Color; 4] = [
    Color::Indexed(23),
    Color::Indexed(30),
    Color::Indexed(37),
    Color::Cyan,
];

pub fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn format_updated_label(updated_at: Instant) -> String {
    let secs = updated_at.elapsed().as_secs();
    if secs < 10 {
        "Updated just now".to_string()
    } else if secs < 60 {
        format!("Updated {secs}s ago")
    } else {
        let mins = secs / 60;
        if mins < 60 {
            format!("Updated {mins}m ago")
        } else {
            let hours = mins / 60;
            format!("Updated {hours}h ago")
        }
    }
}

pub fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let area = frame.area();
    let title = match state.active_screen {
        ActiveScreen::Usage => " comon :: usage ",
        ActiveScreen::Activity => " comon :: activity ",
        ActiveScreen::LimitResets => " comon :: limit resets ",
        ActiveScreen::Read => " comon :: session history ",
    };

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(outer, area);

    let inner = apply_margin(
        area,
        Margin {
            vertical: 1,
            horizontal: 2,
        },
    );

    match state.active_screen {
        ActiveScreen::Usage => {
            let footer_height = footer_height(inner.width, state);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(footer_height),
                ])
                .split(inner);

            render_header(frame, chunks[0], state);
            render_usage(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
            if state.no_sessions_confirm_open {
                render_no_sessions_overlay(frame, area, state);
            }
        }
        ActiveScreen::Activity => {
            let footer_height = footer_height(inner.width, state);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(footer_height),
                ])
                .split(inner);

            render_activity_header(frame, chunks[0], state);
            render_activity(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
        }
        ActiveScreen::LimitResets => {
            let footer_height = footer_height(inner.width, state);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(footer_height),
                ])
                .split(inner);

            render_limit_resets_header(frame, chunks[0], state);
            render_limit_resets(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
        }
        ActiveScreen::Read => {
            let system_locale = state.system_locale.clone();
            let formatter = DisplayFormatter::new(state.display_style, &system_locale);
            crate::read::tui::render(frame, inner, &mut state.read_browser, formatter);
        }
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(28)])
        .split(area);

    let title = "USAGE_SNAPSHOT";
    let left = Paragraph::new(Line::from(Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Left);
    frame.render_widget(left, row[0]);

    let updated = state
        .usage_updated_label()
        .or_else(|| state.limits_updated_label())
        .unwrap_or_else(|| "Updated --".to_string());

    let right = Paragraph::new(Line::from(vec![
        Span::raw(updated),
        Span::raw("  "),
        Span::styled("[r/F5]", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled("[s/F2]", Style::default().fg(Color::Gray)),
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(right, row[1]);
}

fn render_activity_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(36)])
        .split(area);

    let left = Paragraph::new(Line::from(Span::styled(
        "PROJECT_ACTIVITY",
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Left);
    frame.render_widget(left, row[0]);

    let updated = state
        .usage_updated_label()
        .unwrap_or_else(|| "Updated --".to_string());
    let right = Paragraph::new(Line::from(vec![
        Span::raw(updated),
        Span::raw("  "),
        Span::styled("[+/-]", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled("[s/F2]", Style::default().fg(Color::Gray)),
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(right, row[1]);
}

fn render_limit_resets_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(28)])
        .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "LIMIT_RESETS",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        row[0],
    );

    let updated = state
        .limits_updated_label()
        .unwrap_or_else(|| "Updated --".to_string());
    let right = Paragraph::new(Line::from(vec![
        Span::raw(updated),
        Span::raw("  "),
        Span::styled("[r/F5]", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled("[s/F2]", Style::default().fg(Color::Gray)),
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(right, row[1]);
}

fn footer_hint(screen: ActiveScreen) -> &'static str {
    match screen {
        ActiveScreen::Usage => {
            "Usage: Statistic [tab] (tokens/time/runs), Timeframe [w] (week/month), Layout [f] (horizontal/vertical), Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::Activity => {
            "Activity: Statistic [tab] (tokens/time/runs), Projects [+/-], Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::LimitResets => {
            "Limit resets: Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::Read => "",
    }
}

fn footer_error(state: &AppState) -> String {
    let err = state
        .usage_error
        .as_deref()
        .or(state.limits_error.as_deref())
        .unwrap_or("");
    if err.is_empty() {
        String::new()
    } else {
        truncate_middle(err, 80)
    }
}

fn footer_text(state: &AppState) -> String {
    let hint = footer_hint(state.active_screen);
    let style = format!("Style [n]: {}", state.formatter().style_label());
    let err = footer_error(state);
    if err.is_empty() {
        format!("{hint}  {style}")
    } else {
        format!("{hint}  {style}  {err}")
    }
}

fn footer_height(width: u16, state: &AppState) -> u16 {
    wrapped_line_count(&footer_text(state), width).max(1)
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let err = footer_error(state);
    let style = format!("Style [n]: {}", state.formatter().style_label());
    let line = Line::from(vec![
        Span::styled(
            footer_hint(state.active_screen),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(style, Style::default().fg(Color::Gray)),
        Span::raw(if err.is_empty() { "" } else { "  " }),
        Span::styled(err, Style::default().fg(Color::Red)),
    ]);

    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), area);
}

fn render_usage(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let cards_height = usage_cards_height(state, area.width);
    let reset_summary = reset_summary_text(state);
    let controls_height = usage_controls_height(reset_summary.as_deref(), area.width);
    let chunks = usage_layout(area, controls_height, cards_height);

    render_usage_controls(frame, chunks[0], state, reset_summary.as_deref());
    render_usage_cards(frame, chunks[1], state);
    render_usage_chart(frame, chunks[2], state);
    render_top_models(frame, chunks[3], state);
}

fn usage_layout(area: Rect, controls_height: u16, cards_height: u16) -> [Rect; 4] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(controls_height),
            Constraint::Length(cards_height),
            // Cards must keep their measured height. `Min` has higher priority than `Length`
            // in ratatui, so a short terminal would otherwise collapse the cards to borders in
            // order to preserve the chart's minimum height. Let the chart consume the remaining
            // space and degrade first instead.
            Constraint::Fill(1),
            Constraint::Length(3),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2], chunks[3]]
}

fn render_activity(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let cards_height = usage_cards_height(state, area.width);
    let reset_summary = reset_summary_text(state);
    let controls_height = usage_controls_height(reset_summary.as_deref(), area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(controls_height),
            Constraint::Length(cards_height),
            Constraint::Min(1),
        ])
        .split(area);

    render_activity_controls(frame, chunks[0], state, reset_summary.as_deref());
    render_usage_cards(frame, chunks[1], state);
    render_activity_heatmaps(frame, chunks[2], state);
}

fn render_activity_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    reset_summary: Option<&str>,
) {
    let reset_height = reset_summary_height(reset_summary, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(reset_height)])
        .split(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Min(0)])
        .split(chunks[0]);

    let workspace_label = state
        .workspace_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "All workspaces".to_string());
    let left = Paragraph::new(Line::from(vec![
        Span::styled("WORKSPACE", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled(
            truncate_middle(&workspace_label, 48),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(left, row[0]);

    let tokens = pill("TOKENS", state.metric == UsageMetric::Tokens);
    let time = pill("TIME", state.metric == UsageMetric::Time);
    let runs = pill("RUNS", state.metric == UsageMetric::Runs);
    let right = Paragraph::new(Line::from(vec![
        Span::styled("VIEW", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        tokens,
        time,
        runs,
        Span::raw(" "),
        Span::styled("PROJECTS", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        Span::styled(
            state.activity_project_limit.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(right, row[1]);

    if let Some(text) = reset_summary {
        render_reset_summary(frame, chunks[1], text);
    }
}

fn render_activity_heatmaps(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let metric_label = match state.metric {
        UsageMetric::Tokens => "TOKENS",
        UsageMetric::Time => "TIME",
        UsageMetric::Runs => "RUNS",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title_top(
            Line::from(Span::styled(
                format!(" Last {ACTIVITY_TIMELINE_WEEKS} weeks "),
                Style::default().fg(Color::Gray),
            ))
            .left_aligned(),
        )
        .title_top(
            Line::from(Span::styled(
                format!(" {metric_label} "),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    frame.render_widget(block, area);

    let inner = inset_with_border_and_padding(
        area,
        Padding {
            left: 2,
            right: 2,
            top: 1,
            bottom: 0,
        },
    );

    let Some(snapshot) = state.usage.as_ref() else {
        render_activity_message(frame, inner, "Loading activity...");
        return;
    };
    if snapshot.project_activity.is_empty() {
        render_activity_message(frame, inner, "No project activity found.");
        return;
    }
    if inner.width <= ACTIVITY_WEEKDAY_LABEL_WIDTH || inner.height < ACTIVITY_PROJECT_HEIGHT {
        render_activity_message(frame, inner, "Activity view needs more space.");
        return;
    }

    let fit_by_height = ((inner.height.saturating_add(1)) / ACTIVITY_PROJECT_STRIDE).max(1);
    let visible_projects = state
        .activity_project_limit
        .min(snapshot.project_activity.len())
        .min(fit_by_height as usize);

    let mut y = inner.y;
    for project in snapshot.project_activity.iter().take(visible_projects) {
        let slot = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: ACTIVITY_PROJECT_HEIGHT
                .min(inner.y.saturating_add(inner.height).saturating_sub(y)),
        };
        render_project_activity_heatmap(
            frame,
            slot,
            project,
            state.metric,
            snapshot.activity_first_weekday,
            state.formatter(),
        );
        y = y.saturating_add(ACTIVITY_PROJECT_STRIDE);
        if y >= inner.y.saturating_add(inner.height) {
            break;
        }
    }
}

fn render_activity_message(frame: &mut Frame<'_>, area: Rect, message: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(Color::Gray),
        )))
        .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_project_activity_heatmap(
    frame: &mut Frame<'_>,
    area: Rect,
    project: &ProjectActivity,
    metric: UsageMetric,
    first_weekday: Weekday,
    formatter: DisplayFormatter<'_>,
) {
    if area.height < ACTIVITY_PROJECT_HEIGHT || area.width <= ACTIVITY_WEEKDAY_LABEL_WIDTH {
        return;
    }

    let weeks_total = project.days.len() / 7;
    if weeks_total == 0 {
        return;
    }
    let grid_width = area.width.saturating_sub(ACTIVITY_WEEKDAY_LABEL_WIDTH);
    let cell_width = activity_day_cell_width(grid_width, weeks_total);
    let weeks_visible = weeks_total.min((grid_width / cell_width) as usize);
    if weeks_visible == 0 {
        return;
    }
    let week_offset = weeks_total.saturating_sub(weeks_visible);
    let grid_x = area.x.saturating_add(ACTIVITY_WEEKDAY_LABEL_WIDTH);

    let header = format_activity_project_header(project, metric, area.width as usize, formatter);
    let buf = frame.buffer_mut();
    write_text(
        buf,
        area.x,
        area.y,
        area.width,
        &header,
        Style::default().add_modifier(Modifier::BOLD),
    );

    render_activity_month_labels(
        buf,
        project,
        week_offset,
        weeks_visible,
        (grid_x, area.y + 1),
        cell_width,
        formatter,
    );

    let max_value = project
        .days
        .iter()
        .map(|day| activity_day_value(day, metric))
        .max()
        .unwrap_or(0);
    for row in 0..7usize {
        let y = area.y.saturating_add(2 + row as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        write_text(
            buf,
            area.x,
            y,
            ACTIVITY_WEEKDAY_LABEL_WIDTH,
            &activity_weekday_label(first_weekday, row, formatter),
            Style::default().fg(Color::Gray),
        );
        for week in 0..weeks_visible {
            let day_idx = (week_offset + week) * 7 + row;
            let Some(day) = project.days.get(day_idx) else {
                continue;
            };
            let value = activity_day_value(day, metric);
            let level = activity_color_level(value, max_value);
            let x = grid_x.saturating_add((week as u16).saturating_mul(cell_width));
            write_activity_cell(buf, x, y, cell_width, level);
        }
    }
}

fn render_activity_month_labels(
    buf: &mut ratatui::buffer::Buffer,
    project: &ProjectActivity,
    week_offset: usize,
    weeks_visible: usize,
    origin: (u16, u16),
    cell_width: u16,
    formatter: DisplayFormatter<'_>,
) {
    let (grid_x, y) = origin;
    let mut next_free_x = grid_x;
    let grid_end = grid_x.saturating_add((weeks_visible as u16).saturating_mul(cell_width));
    for week in 0..weeks_visible {
        let absolute_week = week_offset + week;
        let Some(label) =
            activity_month_label_for_week(project, absolute_week, week == 0, formatter)
        else {
            continue;
        };
        let x = grid_x.saturating_add((week as u16).saturating_mul(cell_width));
        if x < next_free_x || x >= grid_end {
            continue;
        }
        let available = grid_end.saturating_sub(x);
        write_text(
            buf,
            x,
            y,
            (UnicodeWidthStr::width(label.as_str()) as u16).min(available),
            &label,
            Style::default().fg(Color::Gray),
        );
        next_free_x = x
            .saturating_add(UnicodeWidthStr::width(label.as_str()) as u16)
            .saturating_add(1);
    }
}

fn activity_month_label_for_week(
    project: &ProjectActivity,
    week: usize,
    force: bool,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    let start = week.saturating_mul(7);
    let end = start.saturating_add(7).min(project.days.len());
    let days = project.days.get(start..end)?;
    if force {
        let date = parse_activity_date(&days.first()?.day)?;
        return Some(formatter.abbreviated_month(date.month()));
    }
    for day in days {
        let date = parse_activity_date(&day.day)?;
        if date.day() == 1 {
            return Some(formatter.abbreviated_month(date.month()));
        }
    }
    None
}

fn parse_activity_date(day: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()
}

fn format_activity_project_header(
    project: &ProjectActivity,
    metric: UsageMetric,
    max_width: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    let name = activity_project_name(&project.display_path);
    let active_days = project
        .days
        .iter()
        .filter(|day| activity_day_value(day, metric) > 0)
        .count();
    let total = activity_metric_total_label(project, metric, formatter);
    let last = project
        .last_activity_day
        .as_deref()
        .map(|day| format_activity_date_short(day, formatter))
        .unwrap_or_else(|| "--".to_string());
    truncate_middle(
        &format!(
            "[{name}]  {} days  {total}  last {last}",
            formatter.format_usize(active_days)
        ),
        max_width,
    )
}

fn activity_project_name(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn activity_metric_total_label(
    project: &ProjectActivity,
    metric: UsageMetric,
    formatter: DisplayFormatter<'_>,
) -> String {
    match metric {
        UsageMetric::Tokens => {
            let total = project.total_tokens.max(0) as u64;
            let out_of_cache = (project.total_tokens - project.cached_input_tokens).max(0) as u64;
            let pair = format_horizontal_value(
                total,
                Some(out_of_cache),
                UsageMetric::Tokens,
                20,
                formatter,
            );
            format!("{pair} tokens")
        }
        UsageMetric::Time => format_duration_compact(project.agent_time_ms),
        UsageMetric::Runs => format!("{} runs", format_count(project.agent_runs, formatter)),
    }
}

fn format_activity_date_short(day: &str, formatter: DisplayFormatter<'_>) -> String {
    let Some(date) = parse_activity_date(day) else {
        return day.to_string();
    };
    formatter.format_short_date(date)
}

fn activity_day_value(day: &crate::usage::UsageDay, metric: UsageMetric) -> i64 {
    match metric {
        UsageMetric::Tokens => day.total_tokens.max(0),
        UsageMetric::Time => day.agent_time_ms.max(0),
        UsageMetric::Runs => day.agent_runs.max(0),
    }
}

fn activity_color_level(value: i64, max_value: i64) -> usize {
    if value <= 0 || max_value <= 0 {
        return 0;
    }
    let level = ((value as f64 / max_value as f64) * ACTIVITY_COLORS.len() as f64).ceil() as usize;
    level.clamp(1, ACTIVITY_COLORS.len())
}

fn activity_day_cell_width(grid_width: u16, weeks_total: usize) -> u16 {
    if weeks_total > 0 && usize::from(grid_width) >= weeks_total.saturating_mul(2) {
        2
    } else {
        1
    }
}

fn activity_weekday_label(
    first_weekday: Weekday,
    row: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    let monday_row = activity_weekday_row(first_weekday, Weekday::Mon);
    if row < monday_row || !(row - monday_row).is_multiple_of(2) {
        return String::new();
    }
    match activity_weekday_for_row(first_weekday, row) {
        day @ (Weekday::Mon | Weekday::Wed | Weekday::Fri | Weekday::Sun) => {
            formatter.abbreviated_weekday(day)
        }
        _ => String::new(),
    }
}

fn activity_weekday_for_row(first_weekday: Weekday, row: usize) -> Weekday {
    weekday_from_monday_index((first_weekday.num_days_from_monday() as usize + row) % 7)
}

fn activity_weekday_row(first_weekday: Weekday, weekday: Weekday) -> usize {
    let first = first_weekday.num_days_from_monday() as usize;
    let day = weekday.num_days_from_monday() as usize;
    (7 + day - first) % 7
}

fn weekday_from_monday_index(index: usize) -> Weekday {
    match index % 7 {
        0 => Weekday::Mon,
        1 => Weekday::Tue,
        2 => Weekday::Wed,
        3 => Weekday::Thu,
        4 => Weekday::Fri,
        5 => Weekday::Sat,
        _ => Weekday::Sun,
    }
}

fn write_activity_cell(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    width: u16,
    level: usize,
) {
    let style = if level > 0 {
        Style::default().bg(ACTIVITY_COLORS[level - 1])
    } else {
        Style::default()
    };
    for dx in 0..width {
        if let Some(cell) = buf.cell_mut((x.saturating_add(dx), y)) {
            cell.set_char(' ').set_style(style);
        }
    }
}

fn write_text(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    max_width: u16,
    text: &str,
    style: Style,
) {
    if max_width == 0 {
        return;
    }
    for (idx, ch) in text.chars().take(max_width as usize).enumerate() {
        if let Some(cell) = buf.cell_mut((x.saturating_add(idx as u16), y)) {
            cell.set_char(ch).set_style(style);
        }
    }
}

fn render_usage_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    reset_summary: Option<&str>,
) {
    let reset_height = reset_summary_height(reset_summary, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(reset_height)])
        .split(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Min(0)])
        .split(chunks[0]);

    let workspace_label = state
        .workspace_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "All workspaces".to_string());
    let left = Paragraph::new(Line::from(vec![
        Span::styled("WORKSPACE", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled(
            truncate_middle(&workspace_label, 48),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(left, row[0]);

    let tokens = pill("TOKENS", state.metric == UsageMetric::Tokens);
    let time = pill("TIME", state.metric == UsageMetric::Time);
    let runs = pill("RUNS", state.metric == UsageMetric::Runs);
    let week = pill("WEEK", state.range == ChartRange::Week);
    let month = pill("MONTH", state.range == ChartRange::Month);
    let vert = pill("VERT", state.orientation == ChartOrientation::Vertical);
    let horz = pill("HORZ", state.orientation == ChartOrientation::Horizontal);

    let right = Paragraph::new(Line::from(vec![
        Span::styled("VIEW", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        tokens,
        time,
        runs,
        Span::raw(" "),
        Span::styled("GRAPH", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        week,
        month,
        Span::raw(" "),
        Span::styled("BARS", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        vert,
        horz,
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(right, row[1]);

    if let Some(text) = reset_summary {
        render_reset_summary(frame, chunks[1], text);
    }
}

#[derive(Debug)]
struct CardSpec {
    lines: Vec<String>,
}

impl CardSpec {
    fn new(value: String, captions: Vec<String>) -> Self {
        let mut lines = Vec::with_capacity(1 + captions.len());
        lines.push(value);
        for caption in captions {
            if !caption.trim().is_empty() {
                lines.push(caption);
            }
        }
        Self { lines }
    }

    fn required_height(&self, card_width: u16) -> u16 {
        const CARD_VERTICAL_CHROME: u16 = 3;
        const CARD_BOTTOM_SPACER: u16 = 1;
        const CARD_MIN_HEIGHT: u16 = 6;
        let content_width = card_width.saturating_sub(5).max(1);
        let mut lines = 0_u16;
        for line in &self.lines {
            lines = lines.saturating_add(wrapped_line_count(line, content_width));
        }
        CARD_VERTICAL_CHROME
            .saturating_add(CARD_BOTTOM_SPACER)
            .saturating_add(lines)
            .max(CARD_MIN_HEIGHT)
    }
}

#[derive(Debug, Clone, Copy)]
struct UsageCardLayout {
    columns: usize,
    min_card_width: u16,
}

fn usage_card_layout(width: u16) -> UsageCardLayout {
    if width >= 180 {
        // The final two cards use 16% of the row, so use that width for safe measurement.
        UsageCardLayout {
            columns: 6,
            min_card_width: width.saturating_mul(16) / 100,
        }
    } else {
        // The smaller 3-card rows use 34% / 33% / 33%; measure against the narrowest card.
        UsageCardLayout {
            columns: 3,
            min_card_width: width.saturating_mul(33) / 100,
        }
    }
}

fn uses_compact_limit_lines(card_width: u16) -> bool {
    // Full reset labels need about 33 cells after the card's border and horizontal padding.
    card_width < 38
}

fn limits_card_content(state: &AppState, compact: bool) -> (String, Vec<String>) {
    let formatter = state.formatter();
    let (value, caption1, caption2, caption3) = if !state.limits_enabled {
        let msg = state
            .limits_error
            .as_deref()
            .or(state.limits_notice.as_deref())
            .unwrap_or("Limits unavailable.");
        ("Unavailable".to_string(), Some(msg.to_string()), None, None)
    } else if let Some(limits) = state.limits.as_ref() {
        format_limits_compact_card_lines(limits, compact, formatter)
    } else {
        ("Loading...".to_string(), None, None, None)
    };

    let captions = [caption1, caption2, caption3]
        .into_iter()
        .flatten()
        .collect();
    (value, captions)
}

fn usage_card_specs(state: &AppState, card_width: u16) -> Vec<CardSpec> {
    let formatter = state.formatter();
    let snapshot = state.usage.as_ref();
    let totals = snapshot.map(|snapshot| snapshot.totals_view(state.metric, formatter));
    let today = snapshot.and_then(|snapshot| snapshot.days.last());
    let (limits_value, limits_captions) =
        limits_card_content(state, uses_compact_limit_lines(card_width));

    let today_value = today
        .map(|day| {
            format!(
                "{} tokens",
                format_tokens_overview(day.total_tokens, formatter)
            )
        })
        .unwrap_or_else(|| "--".to_string());
    let today_captions = vec![
        today
            .map(|day| format!("Runs {}", format_count(day.agent_runs, formatter)))
            .unwrap_or_default(),
        today
            .map(|day| format!("Time {}", format_duration_words(day.agent_time_ms)))
            .unwrap_or_default(),
    ];

    let last7_runs = snapshot
        .map(|snapshot| {
            snapshot
                .last7_days()
                .iter()
                .map(|day| day.agent_runs)
                .sum::<i64>()
        })
        .map(|runs| format!("Runs {}", format_count(runs, formatter)));
    let last30_runs = snapshot
        .map(|snapshot| snapshot.days.iter().map(|day| day.agent_runs).sum::<i64>())
        .map(|runs| format!("Runs {}", format_count(runs, formatter)));

    let mut cards = Vec::with_capacity(6);
    cards.push(CardSpec::new(limits_value, limits_captions));
    cards.push(CardSpec::new(today_value, today_captions));

    match state.metric {
        UsageMetric::Tokens => {
            let last7 = totals
                .as_ref()
                .map(|totals| totals.last7_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let avg = totals
                .as_ref()
                .map(|totals| totals.avg_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let last30 = totals
                .as_ref()
                .map(|totals| totals.last30_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let cache = totals
                .as_ref()
                .map(|totals| totals.cache_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak = totals
                .as_ref()
                .map(|totals| totals.peak_day_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak_sub = totals
                .as_ref()
                .map(|totals| totals.peak_sub_label.clone())
                .unwrap_or_default();
            let total = totals
                .as_ref()
                .map(|totals| totals.total_label.clone())
                .unwrap_or_else(|| "--".to_string());

            cards.push(CardSpec::new(
                last7,
                vec![
                    format!("Avg {avg} / day"),
                    last7_runs.clone().unwrap_or_default(),
                ],
            ));
            cards.push(CardSpec::new(
                last30,
                vec![format!("Total {total}"), last30_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(
                cache,
                vec!["Last 7 days".to_string(), last7_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(peak, vec![peak_sub]));
        }
        UsageMetric::Time => {
            let last7 = totals
                .as_ref()
                .map(|totals| totals.last7_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let avg = totals
                .as_ref()
                .map(|totals| totals.avg_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let last30 = totals
                .as_ref()
                .map(|totals| totals.last30_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let runs = totals
                .as_ref()
                .map(|totals| totals.runs_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak = totals
                .as_ref()
                .map(|totals| totals.peak_day_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak_sub = totals
                .as_ref()
                .map(|totals| totals.peak_sub_label.clone())
                .unwrap_or_default();
            let total = totals
                .as_ref()
                .map(|totals| totals.total_label.clone())
                .unwrap_or_else(|| "--".to_string());

            cards.push(CardSpec::new(
                last7,
                vec![format!("Avg {avg} / day"), last7_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(
                last30,
                vec![format!("Total {total}"), last30_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(runs, vec!["Last 7 days".to_string()]));
            cards.push(CardSpec::new(peak, vec![peak_sub]));
        }
        UsageMetric::Runs => {
            let last7 = totals
                .as_ref()
                .map(|totals| totals.last7_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let avg = totals
                .as_ref()
                .map(|totals| totals.avg_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let last30 = totals
                .as_ref()
                .map(|totals| totals.last30_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak = totals
                .as_ref()
                .map(|totals| totals.peak_day_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak_sub = totals
                .as_ref()
                .map(|totals| totals.peak_sub_label.clone())
                .unwrap_or_default();
            let avg30 = snapshot
                .filter(|snapshot| !snapshot.days.is_empty())
                .map(|snapshot| {
                    let total_runs = snapshot.days.iter().map(|day| day.agent_runs).sum::<i64>();
                    format_count(
                        (total_runs as f64 / snapshot.days.len() as f64).round() as i64,
                        formatter,
                    )
                })
                .unwrap_or_else(|| "--".to_string());
            let tokens7 = snapshot
                .map(|snapshot| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(snapshot.totals.last7_days_tokens, formatter)
                    )
                })
                .unwrap_or_else(|| "--".to_string());
            let tokens30 = snapshot
                .map(|snapshot| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(snapshot.totals.last30_days_tokens, formatter)
                    )
                })
                .unwrap_or_else(|| "--".to_string());
            let time7 = snapshot
                .map(|snapshot| {
                    let total_ms = snapshot
                        .last7_days()
                        .iter()
                        .map(|day| day.agent_time_ms)
                        .sum::<i64>();
                    format_duration_compact(total_ms)
                })
                .unwrap_or_else(|| "--".to_string());

            cards.push(CardSpec::new(
                last7,
                vec![format!("Avg {avg} / day"), tokens7],
            ));
            cards.push(CardSpec::new(
                last30,
                vec![format!("Avg {avg30} / day"), tokens30],
            ));
            cards.push(CardSpec::new(time7, vec!["Last 7 days".to_string()]));
            cards.push(CardSpec::new(peak, vec![peak_sub]));
        }
    }

    cards
}

fn usage_card_row_heights(state: &AppState, width: u16) -> Vec<u16> {
    let layout = usage_card_layout(width);
    let card_width = layout.min_card_width.max(1);
    let cards = usage_card_specs(state, card_width);
    let mut row_heights = Vec::with_capacity(cards.len().div_ceil(layout.columns));
    for row in cards.chunks(layout.columns) {
        row_heights.push(card_row_height(row, card_width));
    }
    row_heights
}

fn card_row_height(cards: &[CardSpec], card_width: u16) -> u16 {
    let mut height = 0_u16;
    for card in cards {
        height = height.max(card.required_height(card_width));
    }
    height
}

fn usage_cards_height(state: &AppState, width: u16) -> u16 {
    let mut total = 0_u16;
    for height in usage_card_row_heights(state, width) {
        total = total.saturating_add(height);
    }
    total
}

fn render_usage_cards(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let formatter = state.formatter();
    let card_layout = usage_card_layout(area.width);
    let row_heights = usage_card_row_heights(state, area.width);
    let two_rows = row_heights.len() > 1;
    let (row1, row2) = if two_rows {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(row_heights[0]),
                Constraint::Length(row_heights[1]),
            ])
            .split(area);
        (Some(rows[0]), Some(rows[1]))
    } else {
        (Some(area), None)
    };

    let snapshot = state.usage.as_ref();
    let totals = snapshot.map(|s| s.totals_view(state.metric, formatter));
    let today = snapshot.and_then(|s| s.days.last());

    // LIMITS card (live from Codex app-server).
    let (limits_value, limits_caption1, limits_caption2, limits_caption3): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = if !state.limits_enabled {
        let msg = state
            .limits_error
            .as_deref()
            .or(state.limits_notice.as_deref())
            .unwrap_or("Limits unavailable.");
        ("Unavailable".to_string(), Some(msg.to_string()), None, None)
    } else if let Some(l) = state.limits.as_ref() {
        format_limits_compact_card_lines(
            l,
            uses_compact_limit_lines(card_layout.min_card_width),
            formatter,
        )
    } else {
        ("Loading...".to_string(), None, None, None)
    };

    // TODAY card always shows Tokens / Runs / Time.
    let today_value = today
        .map(|d| {
            format!(
                "{} tokens",
                format_tokens_overview(d.total_tokens, formatter)
            )
        })
        .unwrap_or_else(|| "--".to_string());
    let today_caption1 = today
        .map(|d| format!("Runs {}", format_count(d.agent_runs, formatter)))
        .unwrap_or_default();
    let today_caption2 = today
        .map(|d| format!("Time {}", format_duration_words(d.agent_time_ms)))
        .unwrap_or_default();

    let last7_runs_sum = snapshot
        .map(|s| s.last7_days().iter().map(|d| d.agent_runs).sum::<i64>())
        .unwrap_or(0);
    let last30_runs_sum = snapshot
        .map(|s| s.days.iter().map(|d| d.agent_runs).sum::<i64>())
        .unwrap_or(0);
    let last7_runs_caption = if snapshot.is_some() {
        Some(format!("Runs {}", format_count(last7_runs_sum, formatter)))
    } else {
        None
    };
    let last30_runs_caption = if snapshot.is_some() {
        Some(format!("Runs {}", format_count(last30_runs_sum, formatter)))
    } else {
        None
    };

    match state.metric {
        UsageMetric::Tokens => {
            let last7 = totals
                .as_ref()
                .map(|t| t.last7_primary_label.clone())
                .unwrap_or("--".into());
            let avg = totals
                .as_ref()
                .map(|t| t.avg_primary_label.clone())
                .unwrap_or("--".into());
            let last30 = totals
                .as_ref()
                .map(|t| t.last30_primary_label.clone())
                .unwrap_or("--".into());
            let cache = totals
                .as_ref()
                .map(|t| t.cache_label.clone())
                .unwrap_or("--".into());
            let peak = totals
                .as_ref()
                .map(|t| t.peak_day_label.clone())
                .unwrap_or("--".into());
            let peak_sub = totals
                .as_ref()
                .map(|t| t.peak_sub_label.clone())
                .unwrap_or("".into());
            let total = totals
                .as_ref()
                .map(|t| t.total_label.clone())
                .unwrap_or("--".into());

            if let Some(row1) = row1 {
                if row2.is_none() {
                    let cards = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(16),
                            Constraint::Percentage(16),
                        ])
                        .split(row1);
                    frame.render_widget(
                        card4(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
                            limits_caption3.as_deref(),
                        ),
                        cards[0],
                    );
                    frame.render_widget(
                        card(
                            "TODAY",
                            &today_value,
                            Some(&today_caption1),
                            Some(&today_caption2),
                        ),
                        cards[1],
                    );
                    let runs7 = last7_runs_caption.as_deref();
                    let runs30 = last30_runs_caption.as_deref();
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        cards[2],
                    );
                    frame.render_widget(
                        card(
                            "LAST_30_DAYS",
                            &last30,
                            Some(&format!("Total {total}")),
                            runs30,
                        ),
                        cards[3],
                    );
                    frame.render_widget(
                        card("CACHE_HIT_RATE", &cache, Some("Last 7 days"), runs7),
                        cards[4],
                    );
                    frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), cards[5]);
                } else {
                    let top = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(34),
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                        ])
                        .split(row1);
                    frame.render_widget(
                        card4(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
                            limits_caption3.as_deref(),
                        ),
                        top[0],
                    );
                    let runs7 = last7_runs_caption.as_deref();
                    frame.render_widget(
                        card(
                            "TODAY",
                            &today_value,
                            Some(&today_caption1),
                            Some(&today_caption2),
                        ),
                        top[1],
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        top[2],
                    );
                }
            }
            if let Some(row2) = row2 {
                let bottom = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row2);
                let runs7 = last7_runs_caption.as_deref();
                let runs30 = last30_runs_caption.as_deref();
                frame.render_widget(
                    card(
                        "LAST_30_DAYS",
                        &last30,
                        Some(&format!("Total {total}")),
                        runs30,
                    ),
                    bottom[0],
                );
                frame.render_widget(
                    card("CACHE_HIT_RATE", &cache, Some("Last 7 days"), runs7),
                    bottom[1],
                );
                frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), bottom[2]);
            }
        }
        UsageMetric::Time => {
            let last7 = totals
                .as_ref()
                .map(|t| t.last7_primary_label.clone())
                .unwrap_or("--".into());
            let avg = totals
                .as_ref()
                .map(|t| t.avg_primary_label.clone())
                .unwrap_or("--".into());
            let last30 = totals
                .as_ref()
                .map(|t| t.last30_primary_label.clone())
                .unwrap_or("--".into());
            let runs = totals
                .as_ref()
                .map(|t| t.runs_label.clone())
                .unwrap_or("--".into());
            let peak = totals
                .as_ref()
                .map(|t| t.peak_day_label.clone())
                .unwrap_or("--".into());
            let peak_sub = totals
                .as_ref()
                .map(|t| t.peak_sub_label.clone())
                .unwrap_or("".into());
            let total = totals
                .as_ref()
                .map(|t| t.total_label.clone())
                .unwrap_or("--".into());

            if let Some(row1) = row1 {
                if row2.is_none() {
                    let cards = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(16),
                            Constraint::Percentage(16),
                        ])
                        .split(row1);
                    frame.render_widget(
                        card4(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
                            limits_caption3.as_deref(),
                        ),
                        cards[0],
                    );
                    frame.render_widget(
                        card(
                            "TODAY",
                            &today_value,
                            Some(&today_caption1),
                            Some(&today_caption2),
                        ),
                        cards[1],
                    );
                    let runs7 = last7_runs_caption.as_deref();
                    let runs30 = last30_runs_caption.as_deref();
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        cards[2],
                    );
                    frame.render_widget(
                        card(
                            "LAST_30_DAYS",
                            &last30,
                            Some(&format!("Total {total}")),
                            runs30,
                        ),
                        cards[3],
                    );
                    frame.render_widget(card("RUNS", &runs, Some("Last 7 days"), None), cards[4]);
                    frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), cards[5]);
                } else {
                    let top = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(34),
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                        ])
                        .split(row1);
                    frame.render_widget(
                        card4(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
                            limits_caption3.as_deref(),
                        ),
                        top[0],
                    );
                    let runs7 = last7_runs_caption.as_deref();
                    frame.render_widget(
                        card(
                            "TODAY",
                            &today_value,
                            Some(&today_caption1),
                            Some(&today_caption2),
                        ),
                        top[1],
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        top[2],
                    );
                }
            }
            if let Some(row2) = row2 {
                let bottom = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row2);
                let runs30 = last30_runs_caption.as_deref();
                frame.render_widget(
                    card(
                        "LAST_30_DAYS",
                        &last30,
                        Some(&format!("Total {total}")),
                        runs30,
                    ),
                    bottom[0],
                );
                frame.render_widget(card("RUNS", &runs, Some("Last 7 days"), None), bottom[1]);
                frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), bottom[2]);
            }
        }
        UsageMetric::Runs => {
            let last7 = totals
                .as_ref()
                .map(|t| t.last7_primary_label.clone())
                .unwrap_or("--".into());
            let avg = totals
                .as_ref()
                .map(|t| t.avg_primary_label.clone())
                .unwrap_or("--".into());
            let last30 = totals
                .as_ref()
                .map(|t| t.last30_primary_label.clone())
                .unwrap_or("--".into());
            let peak = totals
                .as_ref()
                .map(|t| t.peak_day_label.clone())
                .unwrap_or("--".into());
            let peak_sub = totals
                .as_ref()
                .map(|t| t.peak_sub_label.clone())
                .unwrap_or("".into());

            let avg30 = snapshot
                .map(|s| {
                    if s.days.is_empty() {
                        0
                    } else {
                        let total_runs = s.days.iter().map(|d| d.agent_runs).sum::<i64>();
                        (total_runs as f64 / s.days.len() as f64).round() as i64
                    }
                })
                .unwrap_or(0);
            let avg30_label = if snapshot.is_some() {
                format_count(avg30, formatter)
            } else {
                "--".into()
            };

            let tokens7 = snapshot
                .map(|s| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(s.totals.last7_days_tokens, formatter)
                    )
                })
                .unwrap_or_else(|| "--".into());
            let tokens30 = snapshot
                .map(|s| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(s.totals.last30_days_tokens, formatter)
                    )
                })
                .unwrap_or_else(|| "--".into());
            let time7 = snapshot
                .map(|s| {
                    let ms = s.last7_days().iter().map(|d| d.agent_time_ms).sum::<i64>();
                    format_duration_compact(ms)
                })
                .unwrap_or_else(|| "--".into());

            if let Some(row1) = row1 {
                if row2.is_none() {
                    let cards = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(16),
                            Constraint::Percentage(16),
                        ])
                        .split(row1);
                    frame.render_widget(
                        card4(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
                            limits_caption3.as_deref(),
                        ),
                        cards[0],
                    );
                    frame.render_widget(
                        card(
                            "TODAY",
                            &today_value,
                            Some(&today_caption1),
                            Some(&today_caption2),
                        ),
                        cards[1],
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            Some(&tokens7),
                        ),
                        cards[2],
                    );
                    frame.render_widget(
                        card(
                            "LAST_30_DAYS",
                            &last30,
                            Some(&format!("Avg {avg30_label} / day")),
                            Some(&tokens30),
                        ),
                        cards[3],
                    );
                    frame.render_widget(card("TIME", &time7, Some("Last 7 days"), None), cards[4]);
                    frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), cards[5]);
                } else {
                    let top = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(34),
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                        ])
                        .split(row1);
                    frame.render_widget(
                        card4(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
                            limits_caption3.as_deref(),
                        ),
                        top[0],
                    );
                    frame.render_widget(
                        card(
                            "TODAY",
                            &today_value,
                            Some(&today_caption1),
                            Some(&today_caption2),
                        ),
                        top[1],
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            Some(&tokens7),
                        ),
                        top[2],
                    );
                }
            }
            if let Some(row2) = row2 {
                let bottom = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row2);
                frame.render_widget(
                    card(
                        "LAST_30_DAYS",
                        &last30,
                        Some(&format!("Avg {avg30_label} / day")),
                        Some(&tokens30),
                    ),
                    bottom[0],
                );
                frame.render_widget(card("TIME", &time7, Some("Last 7 days"), None), bottom[1]);
                frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), bottom[2]);
            }
        }
    }
}

fn render_usage_chart(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    // Note: We draw bars manually to control label placement and padding.
    let formatter = state.formatter();

    let range_label = match state.range {
        ChartRange::Week => "Last 7 days",
        ChartRange::Month => "Last 30 days",
    };
    let metric_label = usage_chart_metric_label(state.metric);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title_top(
            Line::from(Span::styled(
                format!(" {range_label} "),
                Style::default().fg(Color::Gray),
            ))
            .left_aligned(),
        );
    // Only show TOKENS/TIME in horizontal mode, per request.
    if state.orientation == ChartOrientation::Horizontal {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" {metric_label} "),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }
    frame.render_widget(block, area);

    let snapshot = state.usage.as_ref();
    let days = snapshot
        .map(|s| match state.range {
            ChartRange::Week => s.last7_days(),
            ChartRange::Month => s.last_n_days(30),
        })
        .unwrap_or_default();

    // Inner area: account for borders and provide padding (top padding = 1 line).
    let inner = inset_with_border_and_padding(
        area,
        Padding {
            left: 2,
            right: 2,
            top: 1,
            bottom: 0,
        },
    );

    if inner.height < 2 {
        return;
    }

    let mut labels: Vec<String> = Vec::with_capacity(days.len());
    let mut values: Vec<u64> = Vec::with_capacity(days.len());
    let mut token_out_of_cache_values: Vec<u64> = Vec::with_capacity(days.len());
    for day in &days {
        let label = match state.orientation {
            ChartOrientation::Horizontal => format_day_label_weekday_mmdd(&day.day, formatter),
            ChartOrientation::Vertical => match state.range {
                ChartRange::Week => day.short_label(formatter),
                ChartRange::Month => {
                    // Prefer compact day-of-month labels for dense charts.
                    if day.day.len() == 10 {
                        day.day[8..10].to_string()
                    } else {
                        day.short_label(formatter)
                    }
                }
            },
        };
        labels.push(label);
    }
    for day in &days {
        let value = match state.metric {
            UsageMetric::Tokens => day.total_tokens.max(0) as u64,
            UsageMetric::Time => (day.agent_time_ms.max(0) as u64) / 60_000,
            UsageMetric::Runs => day.agent_runs.max(0) as u64,
        };
        values.push(value);
        token_out_of_cache_values.push((day.total_tokens - day.cached_input_tokens).max(0) as u64);
    }

    match state.orientation {
        ChartOrientation::Vertical => {
            if values.is_empty() || inner.width < 3 || inner.height < 3 {
                return;
            }

            // 1 line for labels at the bottom, remaining for bars.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(2), Constraint::Length(1)])
                .split(inner);
            let bars_area = chunks[0];
            let labels_area = chunks[1];

            let (bar_width, bar_gap) = compute_bar_layout(bars_area, values.len() as u16, 0);
            let bw = bar_width.max(1);

            let max_value = values.iter().copied().max().unwrap_or(0).max(1);
            let buf = frame.buffer_mut();

            // Bars are laid left-to-right.
            for (i, (label, value)) in labels.iter().zip(values.iter()).enumerate() {
                let x0 = bars_area
                    .x
                    .saturating_add((i as u16).saturating_mul(bw.saturating_add(bar_gap)));
                if x0 >= bars_area.x.saturating_add(bars_area.width) {
                    break;
                }
                let w = bw.min(
                    bars_area
                        .x
                        .saturating_add(bars_area.width)
                        .saturating_sub(x0),
                );
                if w == 0 {
                    break;
                }

                let fill_bg = if i % 2 == 0 {
                    Color::Cyan
                } else {
                    Color::LightCyan
                };

                let ratio = (*value as f64) / (max_value as f64);
                let filled_h = ((bars_area.height as f64) * ratio.clamp(0.0, 1.0)).round() as u16;

                // Fill bar area with background for filled portion.
                let bottom_y = bars_area
                    .y
                    .saturating_add(bars_area.height)
                    .saturating_sub(1);
                let top_filled_y = bottom_y.saturating_sub(filled_h.saturating_sub(1));
                for yy in bars_area.y..bars_area.y.saturating_add(bars_area.height) {
                    for xx in x0..x0.saturating_add(w) {
                        if let Some(cell) = buf.cell_mut((xx, yy)) {
                            cell.set_char(' ');
                            if filled_h > 0 && yy >= top_filled_y {
                                cell.set_style(Style::default().bg(fill_bg));
                            }
                        }
                    }
                }

                // Value text inside bar, one line above the bottom.
                if bars_area.height >= 3 && filled_h >= 2 {
                    let text_y = bottom_y.saturating_sub(1); // 1-line bottom space
                    let text = match state.metric {
                        UsageMetric::Tokens => format_vertical_token_value(*value, w, formatter),
                        UsageMetric::Time => {
                            let raw = format_minutes_hhmm(*value);
                            if raw.len() <= w as usize {
                                raw
                            } else if w >= 4 {
                                // fallback: compact minutes
                                format_compact_kmb(*value, w, formatter)
                            } else {
                                String::new()
                            }
                        }
                        UsageMetric::Runs => {
                            let raw = value.to_string();
                            if raw.len() <= w as usize {
                                raw
                            } else {
                                format_compact_kmb(*value, w, formatter)
                            }
                        }
                    };
                    let text = truncate_middle(&text, w as usize);
                    let start_x = x0.saturating_add((w.saturating_sub(text.len() as u16)) / 2);
                    for (j, ch) in text.chars().enumerate() {
                        if j as u16 >= w {
                            break;
                        }
                        if let Some(cell) = buf.cell_mut((start_x + j as u16, text_y)) {
                            cell.set_char(ch).set_style(
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(fill_bg)
                                    .add_modifier(Modifier::BOLD),
                            );
                        }
                    }
                }

                // Label under the bar (centered).
                let label_text = truncate_middle(label, w as usize);
                let label_start_x =
                    x0.saturating_add((w.saturating_sub(label_text.len() as u16)) / 2);
                for (j, ch) in label_text.chars().enumerate() {
                    if j as u16 >= w {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((label_start_x + j as u16, labels_area.y)) {
                        cell.set_char(ch)
                            .set_style(Style::default().fg(Color::Gray));
                    }
                }
            }
        }
        ChartOrientation::Horizontal => {
            // Multi-line horizontal bars, values outside to the right.
            //
            // Behavior:
            // - Prefer bar height 5, else 3, else 1, depending on how many bars fit for the current range.
            // - If even height=1 can't fit all bars, show the most recent subset that fits.
            // - Label is on the left, bar is in the middle, and the value is outside the bar with 1 space after bar.

            let count = values.len();
            if count == 0 || inner.width < 10 || inner.height < 1 {
                return;
            }

            // Decide bar height based on available space and selected range.
            let max_per_bar = inner.height / (count as u16).max(1);
            let preferred_h = if max_per_bar >= 5 {
                5
            } else if max_per_bar >= 4 {
                4
            } else if max_per_bar >= 3 {
                3
            } else {
                1
            };
            let bar_h = preferred_h.clamp(1, 5);

            let fit_bars = (inner.height / bar_h).max(1) as usize;
            let start = count.saturating_sub(fit_bars);

            let visible_labels = &labels[start..];
            let visible_values = &values[start..];
            let visible_out_of_cache = &token_out_of_cache_values[start..];

            let max_value = visible_values.iter().copied().max().unwrap_or(0).max(1);

            // Compute label column width based on label lengths (clamped).
            let mut max_label_len = 0usize;
            for l in visible_labels {
                max_label_len = max_label_len.max(l.len());
            }
            let label_w = (max_label_len as u16).saturating_add(1).clamp(4, 14);

            let value_gap: u16 = 1; // spaces between bar and value
            let min_bar_w: u16 = 10;

            let available = inner.width.saturating_sub(label_w);
            if available <= value_gap + 1 {
                return;
            }

            // Value column width: size to the widest full number, then anchor the column to the right.
            let desired_value_w = {
                let mut max_len = 0usize;
                for (idx, v) in visible_values.iter().enumerate() {
                    let out_of_cache = match state.metric {
                        UsageMetric::Tokens => Some(visible_out_of_cache[idx]),
                        _ => None,
                    };
                    let s = format_horizontal_value(
                        *v,
                        out_of_cache,
                        state.metric,
                        u16::MAX,
                        formatter,
                    );
                    max_len = max_len.max(s.len());
                }
                (max_len as u16).clamp(6, 32)
            };

            let mut value_w = desired_value_w.min(available.saturating_sub(value_gap).max(1));
            if available > min_bar_w.saturating_add(value_gap) {
                value_w = value_w.min(
                    available
                        .saturating_sub(value_gap)
                        .saturating_sub(min_bar_w),
                );
            }
            // Keep at least 1 cell for the bar; if we're extremely cramped, shrink values.
            value_w = value_w.clamp(1, available.saturating_sub(value_gap).max(1));
            let bar_w = available
                .saturating_sub(value_gap)
                .saturating_sub(value_w)
                .max(1);

            let buf = frame.buffer_mut();

            for (idx, (label, value)) in
                visible_labels.iter().zip(visible_values.iter()).enumerate()
            {
                let y0 = inner.y.saturating_add((idx as u16).saturating_mul(bar_h));
                if y0 >= inner.y.saturating_add(inner.height) {
                    break;
                }
                let row_area = Rect {
                    x: inner.x,
                    y: y0,
                    width: inner.width,
                    height: bar_h.min(inner.y.saturating_add(inner.height).saturating_sub(y0)),
                };

                let label_area = Rect {
                    x: row_area.x,
                    y: row_area.y,
                    width: label_w.min(row_area.width),
                    height: row_area.height,
                };
                let bar_area = Rect {
                    x: row_area.x.saturating_add(label_area.width),
                    y: row_area.y,
                    width: bar_w.min(
                        row_area
                            .width
                            .saturating_sub(label_area.width)
                            .saturating_sub(value_gap)
                            .saturating_sub(value_w),
                    ),
                    height: row_area.height,
                };
                let value_area = Rect {
                    x: inner.x.saturating_add(inner.width).saturating_sub(value_w),
                    y: row_area.y,
                    width: value_w,
                    height: row_area.height,
                };

                // Center line for label/value.
                let mid_y = row_area.y.saturating_add(row_area.height / 2);

                // Render label on the middle line.
                let label_text = truncate_middle(label, label_area.width as usize);
                for (i, ch) in label_text.chars().enumerate() {
                    if i as u16 >= label_area.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((label_area.x + i as u16, mid_y)) {
                        cell.set_char(ch)
                            .set_style(Style::default().fg(Color::Gray));
                    }
                }

                // Fill bar background according to ratio.
                let ratio = (*value as f64) / (max_value as f64);
                let filled = ((bar_area.width as f64) * ratio.clamp(0.0, 1.0)).round() as u16;
                let fill_bg = if idx % 2 == 0 {
                    Color::Cyan
                } else {
                    Color::LightCyan
                };
                for yy in bar_area.y..bar_area.y.saturating_add(bar_area.height) {
                    for xx in bar_area.x..bar_area.x.saturating_add(bar_area.width) {
                        if let Some(cell) = buf.cell_mut((xx, yy)) {
                            cell.set_char(' ');
                            if xx < bar_area.x.saturating_add(filled) {
                                cell.set_style(Style::default().bg(fill_bg));
                            }
                        }
                    }
                }

                // Value outside the bar, on the middle line, with 1 leading space.
                let value_max_width = value_area.width;
                let out_of_cache = match state.metric {
                    UsageMetric::Tokens => Some(visible_out_of_cache[idx]),
                    _ => None,
                };
                let value_text = format_horizontal_value(
                    *value,
                    out_of_cache,
                    state.metric,
                    value_max_width,
                    formatter,
                );
                let value_text = truncate_middle(&value_text, value_max_width as usize);
                // Right-align numeric values within the dedicated value column.
                let start_x = value_area
                    .x
                    .saturating_add(value_area.width)
                    .saturating_sub(value_text.len() as u16);
                for (i, ch) in value_text.chars().enumerate() {
                    if i as u16 >= value_area.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((start_x + i as u16, mid_y)) {
                        cell.set_char(ch)
                            .set_style(Style::default().fg(Color::Gray));
                    }
                }
            }
        }
    }
}

fn usage_chart_metric_label(metric: UsageMetric) -> &'static str {
    match metric {
        UsageMetric::Tokens => "TOKENS (TOTAL / NON-CACHED)",
        UsageMetric::Time => "TIME",
        UsageMetric::Runs => "RUNS",
    }
}

fn format_vertical_token_value(
    value: u64,
    max_width: u16,
    formatter: DisplayFormatter<'_>,
) -> String {
    let preferred = match formatter.style() {
        DisplayStyle::Classic => value.to_string(),
        DisplayStyle::SystemCompact => format_tokens_compact(value as i64, formatter),
        DisplayStyle::SystemFull => formatter.format_u64(value),
    };
    if preferred.len() <= max_width as usize || formatter.style() == DisplayStyle::SystemFull {
        preferred
    } else {
        format_compact_kmb(value, max_width, formatter)
    }
}

fn format_day_label_weekday_mmdd(day: &str, formatter: DisplayFormatter<'_>) -> String {
    // Input: YYYY-MM-DD
    // Output: Mon 02/02
    let parsed = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok();
    let Some(date) = parsed else {
        return day.to_string();
    };
    formatter.format_chart_day(date)
}

fn format_horizontal_value(
    value: u64,
    out_of_cache_tokens: Option<u64>,
    metric: UsageMetric,
    max_width: u16,
    formatter: DisplayFormatter<'_>,
) -> String {
    if max_width == 0 {
        return String::new();
    }

    if metric == UsageMetric::Tokens {
        if let Some(out_of_cache) = out_of_cache_tokens {
            let compact = format!(
                "{} / {}",
                format_tokens_overview(value as i64, formatter),
                format_tokens_overview(out_of_cache as i64, formatter)
            );
            if compact.len() <= max_width as usize {
                return compact;
            }

            if formatter.style() == DisplayStyle::SystemFull {
                return compact;
            }

            let pair_width = max_width.saturating_sub(1);
            if pair_width >= 2 {
                let mut left_w = ((pair_width as usize * 2) / 3) as u16;
                left_w = left_w.clamp(1, pair_width.saturating_sub(1));
                let right_w = pair_width.saturating_sub(left_w);
                return format!(
                    "{} / {}",
                    format_compact_kmb(value, left_w, formatter),
                    format_compact_kmb(out_of_cache, right_w, formatter),
                );
            }
        }
    }

    let full = match metric {
        UsageMetric::Tokens => format_tokens_overview(value as i64, formatter),
        UsageMetric::Time => format_minutes_hhmm(value),
        UsageMetric::Runs => format_count(value as i64, formatter),
    };
    if full.len() <= max_width as usize {
        return full;
    }
    if metric == UsageMetric::Tokens && formatter.style() == DisplayStyle::SystemFull {
        return full;
    }

    // If we can't fit the full value, try compact with the same suffix.
    // (No suffix for horizontal values; the chart header indicates the unit.)

    // Final fallback: compact only.
    format_compact_kmb(value, max_width, formatter)
}

fn format_duration_words(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;

    if hours > 0 {
        let hour_label = if hours == 1 { "hour" } else { "hours" };
        if mins > 0 {
            format!("{hours} {hour_label} {mins} min")
        } else {
            format!("{hours} {hour_label}")
        }
    } else {
        format!("{mins} min")
    }
}

fn format_minutes_hhmm(total_minutes: u64) -> String {
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 99 {
        return "99:59".to_string();
    }
    format!("{:02}:{:02}", hours, minutes)
}

fn render_top_models(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let formatter = state.formatter();
    let snapshot = state.usage.as_ref();
    let models = snapshot.map(|s| s.top_models.clone()).unwrap_or_default();

    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "TOP_MODELS  ",
        Style::default().fg(Color::Gray),
    )];
    if models.is_empty() {
        spans.push(Span::raw("--"));
    } else {
        for (idx, m) in models.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!(
                    "{} {}%",
                    truncate_middle(&m.model, 18),
                    formatter.format_one_decimal(m.share_percent)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect, screen: ActiveScreen) {
    let w = area.width.min(60);
    let h = area.height.min(13);
    let popup = centered_rect(w, h, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            " Help ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let text = match screen {
        ActiveScreen::Usage => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  Tab  - toggle statistic (Tokens/Time/Runs)"),
            Line::from("  w    - toggle timeframe (Week/Month)"),
            Line::from("  f    - toggle layout (Horz/Vert)"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  r/F5 - refresh usage + limits"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q/Esc - quit"),
        ]),
        ActiveScreen::Activity => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  Tab  - toggle statistic (Tokens/Time/Runs)"),
            Line::from("  +/=  - show more projects"),
            Line::from("  -    - show fewer projects"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  r/F5 - refresh usage + limits"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q/Esc - quit"),
        ]),
        ActiveScreen::LimitResets => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  r/F5 - refresh reset credits"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q/Esc - quit"),
        ]),
        ActiveScreen::Read => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  q/Esc - quit"),
        ]),
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_no_sessions_overlay(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let w = area.width.min(74);
    let h = area.height.min(11);
    let popup = centered_rect(w, h, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            " Warning! ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));

    let project = state
        .workspace_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "--".to_string());

    let text = Text::from(vec![
        Line::from("No Codex session files were found for this project."),
        Line::from(Span::styled(
            truncate_middle(&project, 64),
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from("Usage will stay empty until you run Codex in this repo."),
        Line::from("Continue anyway?"),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter/Y to continue. ESC/Q to quit",
            Style::default().fg(Color::Gray),
        )),
    ]);

    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn pill(label: &str, active: bool) -> Span<'static> {
    let style = if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Span::styled(format!(" {label} "), style)
}

fn card(
    title: &str,
    value: &str,
    caption1: Option<&str>,
    caption2: Option<&str>,
) -> Paragraph<'static> {
    card_with_captions(title, value, &[caption1, caption2])
}

fn card4(
    title: &str,
    value: &str,
    caption1: Option<&str>,
    caption2: Option<&str>,
    caption3: Option<&str>,
) -> Paragraph<'static> {
    card_with_captions(title, value, &[caption1, caption2, caption3])
}

fn card_with_captions(title: &str, value: &str, captions: &[Option<&str>]) -> Paragraph<'static> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Gray),
        ));

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        value.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for caption in captions.iter().flatten() {
        if !caption.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                caption.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }
    lines.push(Line::from(""));

    Paragraph::new(Text::from(lines))
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true })
}

fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let mut total = 0_u16;

    for logical_line in text.split('\n') {
        let mut line_width = 0_usize;
        let mut line_count = 0_u16;
        for word in logical_line.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            if line_width == 0 {
                if word_width > width {
                    line_count = line_count.saturating_add(word_width.div_ceil(width) as u16);
                } else {
                    line_width = word_width;
                }
                continue;
            }

            if word_width > width || line_width.saturating_add(1 + word_width) > width {
                line_count = line_count.saturating_add(1);
                if word_width > width {
                    line_count = line_count.saturating_add(word_width.div_ceil(width) as u16);
                    line_width = 0;
                } else {
                    line_width = word_width;
                }
            } else {
                line_width = line_width.saturating_add(1 + word_width);
            }
        }

        if line_width > 0 || line_count == 0 {
            line_count = line_count.saturating_add(1);
        }
        total = total.saturating_add(line_count);
    }

    total.max(1)
}

fn reset_credit_expiration_label(
    expires_at: i64,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    let ms = crate::codex_rpc::normalize_epoch_millis(expires_at);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    Some(formatter.format_reset_datetime(dt.naive_local()))
}

fn reset_summary_text(state: &AppState) -> Option<String> {
    reset_summary_for_limits(state.limits.as_ref()?, state.formatter())
}

fn reset_summary_for_limits(
    limits: &crate::codex_rpc::AccountRateLimits,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    let available = limits.reset_credits_available?;
    let mut text = format!("Resets: {} available", formatter.format_count(available));
    let earliest_expiry = limits
        .reset_credits
        .as_deref()
        .and_then(|credits| credits.iter().filter_map(|credit| credit.expires_at).min());
    if let Some(expiry) =
        earliest_expiry.and_then(|expires_at| reset_credit_expiration_label(expires_at, formatter))
    {
        text.push_str(" | earliest expires ");
        text.push_str(&expiry);
    }
    Some(text)
}

fn reset_summary_display_text(text: &str) -> String {
    format!("LIMIT RESETS  {text}")
}

fn reset_summary_height(summary: Option<&str>, width: u16) -> u16 {
    summary
        .map(reset_summary_display_text)
        .map(|text| wrapped_line_count(&text, width).saturating_add(1))
        .unwrap_or(0)
}

fn usage_controls_height(summary: Option<&str>, width: u16) -> u16 {
    1_u16.saturating_add(reset_summary_height(summary, width))
}

fn render_reset_summary(frame: &mut Frame<'_>, area: Rect, text: &str) {
    let line = Line::from(vec![
        Span::styled(" LIMIT RESETS ", Style::default().fg(Color::Gray)),
        Span::styled(text.to_string(), Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(vec![line, Line::from("")])).wrap(Wrap { trim: true }),
        area,
    );
}

fn render_limit_resets(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let summary = reset_summary_text(state);
    let summary_height = reset_summary_height(summary.as_deref(), area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(summary_height), Constraint::Min(0)])
        .split(area);

    if let Some(text) = summary.as_deref() {
        render_reset_summary(frame, chunks[0], text);
    }
    render_limit_reset_details(frame, chunks[1], state);
}

fn reset_credit_date_time_label(timestamp: i64, formatter: DisplayFormatter<'_>) -> Option<String> {
    let ms = crate::codex_rpc::normalize_epoch_millis(timestamp);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    if dt.date_naive() == Local::now().date_naive() {
        Some(format!("today {}", formatter.format_time(dt.naive_local())))
    } else {
        Some(formatter.format_reset_datetime(dt.naive_local()))
    }
}

fn render_limit_reset_details(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            " Available reset credits ",
            Style::default().fg(Color::Gray),
        ));

    let text = if !state.limits_enabled {
        let message = state
            .limits_error
            .as_deref()
            .or(state.limits_notice.as_deref())
            .unwrap_or("Limits unavailable.");
        Text::from(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(Color::Gray),
        )))
    } else {
        match state.limits.as_ref() {
            None => Text::from(Line::from(Span::styled(
                "Loading reset credits...",
                Style::default().fg(Color::Gray),
            ))),
            Some(limits) => match limits.reset_credits.as_deref() {
                None => Text::from(vec![
                    Line::from("Codex returned the reset-credit count but no individual details."),
                    Line::from(Span::styled(
                        "Refresh later to check whether expiration dates become available.",
                        Style::default().fg(Color::Gray),
                    )),
                ]),
                Some(credits) if credits.is_empty() => Text::from(Line::from(Span::styled(
                    "No reset credits are currently available.",
                    Style::default().fg(Color::Gray),
                ))),
                Some(credits) => reset_credit_details_text(credits, state.formatter()),
            },
        }
    };

    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn reset_credit_details_text(
    credits: &[crate::codex_rpc::RateLimitResetCredit],
    formatter: DisplayFormatter<'_>,
) -> Text<'static> {
    let mut lines = Vec::with_capacity(credits.len().saturating_mul(4));
    for (index, credit) in credits.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        let title = credit
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("Reset credit {}", index + 1));
        lines.push(Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )));

        let status = credit.status.as_deref().unwrap_or("Unknown");
        let expires = credit
            .expires_at
            .and_then(|timestamp| reset_credit_date_time_label(timestamp, formatter))
            .map(|value| format!("expires {value}"))
            .unwrap_or_else(|| "expiration unavailable".to_string());
        let granted = credit
            .granted_at
            .and_then(|timestamp| reset_credit_date_time_label(timestamp, formatter))
            .map(|value| format!("granted {value}"));
        let reset_type = credit.reset_type.as_deref();
        let mut metadata = format!("{status} | {expires}");
        if let Some(granted) = granted {
            metadata.push_str(" | ");
            metadata.push_str(&granted);
        }
        if let Some(reset_type) = reset_type {
            metadata.push_str(" | ");
            metadata.push_str(reset_type);
        }
        lines.push(Line::from(Span::styled(
            metadata,
            Style::default().fg(Color::Cyan),
        )));

        if let Some(description) = credit
            .description
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(Line::from(Span::styled(
                description.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }
    Text::from(lines)
}

fn format_window_label(window_duration_mins: f64) -> Option<String> {
    if !window_duration_mins.is_finite() {
        return None;
    }
    let mins = window_duration_mins.max(0.0);
    if mins < 1.0 {
        return None;
    }
    if mins >= 60.0 {
        Some(format!("{}h", (mins / 60.0).round() as i64))
    } else {
        Some(format!("{}m", mins.round() as i64))
    }
}

fn percent_left_value(used_percent: Option<f64>) -> String {
    let Some(used) = used_percent else {
        return "--%".to_string();
    };
    if !used.is_finite() {
        return "--%".to_string();
    }
    let left = (100.0 - used).clamp(0.0, 100.0);
    format!("{}%", left.round() as i64)
}

fn resets_label(resets_at: Option<i64>, formatter: DisplayFormatter<'_>) -> Option<String> {
    let raw = resets_at?;
    let ms = crate::codex_rpc::normalize_epoch_millis(raw);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    let today = Local::now().date_naive();
    let day = dt.date_naive();
    let time = formatter.format_time(dt.naive_local());
    if day == today {
        Some(format!("resets {time}"))
    } else {
        Some(format!(
            "resets {time}, {}",
            formatter.format_reset_date(day)
        ))
    }
}

fn reset_compact_label(resets_at: Option<i64>, formatter: DisplayFormatter<'_>) -> Option<String> {
    let raw = resets_at?;
    let ms = crate::codex_rpc::normalize_epoch_millis(raw);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    let today = Local::now().date_naive();
    if dt.date_naive() == today {
        Some(formatter.format_time(dt.naive_local()))
    } else {
        Some(formatter.format_reset_date(dt.date_naive()))
    }
}

fn format_limit_compact_line(
    label_with_colon: &str,
    window: Option<&crate::codex_rpc::RateLimitWindow>,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> String {
    // Match requested alignment:
    // 5h limit: 100% (resets 20:43)
    // Weekly:   99% (resets 09:47, 10 Feb)
    const LABEL_W: usize = 10;
    let label = format!("{label_with_colon:<LABEL_W$}");
    let Some(w) = window else {
        return format!("{label}--");
    };
    let pct = percent_left_value(w.used_percent);
    if compact {
        let label = label_with_colon
            .trim_end_matches(':')
            .strip_suffix(" limit")
            .unwrap_or(label_with_colon.trim_end_matches(':'));
        let reset = reset_compact_label(w.resets_at, formatter)
            .map(|value| format!(" | {value}"))
            .unwrap_or_default();
        return format!("{label}: {pct}{reset}");
    }
    let resets = resets_label(w.resets_at, formatter)
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    format!("{label}{pct}{resets}")
}

fn format_rolling_limit_lines(
    short_window: Option<&crate::codex_rpc::RateLimitWindow>,
    weekly_window: Option<&crate::codex_rpc::RateLimitWindow>,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> (String, String) {
    let short_label = short_window
        .and_then(|w| w.window_duration_mins)
        .and_then(format_window_label)
        .map(|w| format!("{w} limit:"))
        .unwrap_or_else(|| "5h limit:".to_string());
    let weekly_label = if compact { "7d:" } else { "Weekly:" };

    // The newer response can expose the seven-day window as `primary` with no
    // short window. Keep the populated weekly limit in the card's value slot.
    if short_window.is_none() && weekly_window.is_some() {
        let weekly = format_limit_compact_line(weekly_label, weekly_window, compact, formatter);
        let short = format_limit_compact_line(&short_label, short_window, compact, formatter);
        return (weekly, short);
    }

    let short = format_limit_compact_line(&short_label, short_window, compact, formatter);
    let weekly = format_limit_compact_line(weekly_label, weekly_window, compact, formatter);
    (short, weekly)
}

fn format_limits_compact_card_lines(
    l: &crate::codex_rpc::AccountRateLimits,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let (short_window, weekly_window) = rolling_windows_for_limits(l);
    let (rolling_first, rolling_second) =
        format_rolling_limit_lines(short_window, weekly_window, compact, formatter);
    if let Some((monthly, used)) = format_individual_limit_compact_lines(l, compact, formatter) {
        return (
            monthly,
            Some(used),
            Some(rolling_first),
            Some(rolling_second),
        );
    }

    if let Some(extra) = format_extra_bucket_compact_line(l, compact, formatter) {
        let credits =
            format_credits_compact_line(l, formatter).unwrap_or_else(|| "Credits:  --".to_string());
        (
            rolling_first,
            Some(rolling_second),
            Some(extra),
            Some(credits),
        )
    } else {
        let credits =
            format_credits_compact_line(l, formatter).unwrap_or_else(|| "Credits:  --".to_string());
        (rolling_first, Some(rolling_second), Some(credits), None)
    }
}

const WEEKLY_WINDOW_MINUTES: f64 = 6.0 * 24.0 * 60.0;

fn is_weekly_rolling_window(window: &crate::codex_rpc::RateLimitWindow) -> bool {
    window
        .window_duration_mins
        .is_some_and(|minutes| minutes.is_finite() && minutes >= WEEKLY_WINDOW_MINUTES)
}

fn classify_rolling_windows<'a>(
    primary: Option<&'a crate::codex_rpc::RateLimitWindow>,
    secondary: Option<&'a crate::codex_rpc::RateLimitWindow>,
) -> (
    Option<&'a crate::codex_rpc::RateLimitWindow>,
    Option<&'a crate::codex_rpc::RateLimitWindow>,
) {
    let primary_is_weekly = primary.is_some_and(is_weekly_rolling_window);
    let secondary_is_weekly = secondary.is_some_and(is_weekly_rolling_window);

    if primary_is_weekly {
        return (secondary.filter(|_| !secondary_is_weekly), primary);
    }
    if secondary_is_weekly {
        return (primary.filter(|_| !primary_is_weekly), secondary);
    }

    // Older responses use primary/secondary position to mean short/weekly and
    // do not always include a usable duration. Preserve that shape unchanged.
    (primary, secondary)
}

fn rolling_windows_for_limits(
    l: &crate::codex_rpc::AccountRateLimits,
) -> (
    Option<&crate::codex_rpc::RateLimitWindow>,
    Option<&crate::codex_rpc::RateLimitWindow>,
) {
    if l.primary.is_some() || l.secondary.is_some() {
        return classify_rolling_windows(l.primary.as_ref(), l.secondary.as_ref());
    }
    l.buckets
        .iter()
        .find(|bucket| bucket.primary.is_some() || bucket.secondary.is_some())
        .map(|bucket| classify_rolling_windows(bucket.primary.as_ref(), bucket.secondary.as_ref()))
        .unwrap_or((None, None))
}

fn format_individual_limit_compact_lines(
    l: &crate::codex_rpc::AccountRateLimits,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> Option<(String, String)> {
    let individual_limit = l.individual_limit.as_ref().or_else(|| {
        l.buckets
            .iter()
            .find_map(|bucket| bucket.individual_limit.as_ref())
    })?;

    const LABEL_W: usize = 10;
    let remaining = individual_limit
        .remaining_percent
        .filter(|v| v.is_finite())
        .map(|v| format!("{}%", v.round() as i64))
        .unwrap_or_else(|| "--%".to_string());
    let resets = if compact {
        reset_compact_label(individual_limit.resets_at, formatter)
            .map(|value| format!(" | {value}"))
            .unwrap_or_default()
    } else {
        resets_label(individual_limit.resets_at, formatter)
            .map(|value| format!(" ({value})"))
            .unwrap_or_default()
    };
    let monthly = format!("{:<LABEL_W$}{remaining}{resets}", "Monthly:");

    let used = individual_limit
        .used
        .as_deref()
        .map(|raw| format_credit_amount(raw, formatter))
        .unwrap_or_else(|| "--".to_string());
    let limit = individual_limit
        .limit
        .as_deref()
        .map(|raw| format_credit_amount(raw, formatter))
        .unwrap_or_else(|| "--".to_string());
    let used_line = format!("{:<LABEL_W$}{used}/{limit} used", "Credits:");
    Some((monthly, used_line))
}

fn format_extra_bucket_compact_line(
    l: &crate::codex_rpc::AccountRateLimits,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    let active_id = l.limit_id.as_deref();
    let active_name = l.limit_name.as_deref();
    let bucket = l.buckets.iter().find(|bucket| {
        let same_id = active_id.is_some() && bucket.limit_id.as_deref() == active_id;
        let same_name = active_id.is_none()
            && active_name.is_some()
            && bucket.limit_name.as_deref() == active_name;
        !same_id && !same_name
    })?;
    let window = bucket.primary.as_ref().or(bucket.secondary.as_ref())?;
    let label = compact_bucket_label(bucket);
    Some(format_limit_compact_line(
        &label,
        Some(window),
        compact,
        formatter,
    ))
}

fn compact_bucket_label(bucket: &crate::codex_rpc::RateLimitSnapshot) -> String {
    let raw = bucket
        .limit_name
        .as_deref()
        .or(bucket.limit_id.as_deref())
        .unwrap_or("Other")
        .trim();
    let raw = raw.strip_prefix("GPT-").unwrap_or(raw);
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "Other:".to_string();
    }
    format!("{}:", truncate_middle(cleaned, 9))
}

fn format_credit_amount(raw: &str, formatter: DisplayFormatter<'_>) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    let number = cleaned.parse::<f64>().ok().filter(|v| v.is_finite());
    number
        .map(|v| format_count(v.round() as i64, formatter))
        .unwrap_or_else(|| truncate_middle(cleaned, 12))
}

fn format_credits_compact_line(
    l: &crate::codex_rpc::AccountRateLimits,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    const LABEL_W: usize = 10;
    let credits = l.credits.as_ref()?;
    if !credits.has_credits {
        return None;
    }
    if credits.unlimited {
        return Some(format!("{:<LABEL_W$}Unlimited", "Credits:"));
    }
    let raw = credits.balance.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    let n = cleaned.parse::<f64>().ok().filter(|v| v.is_finite());
    let amount = n
        .map(|v| format!("{} credits", formatter.format_count(v.round() as i64)))
        .unwrap_or_else(|| format!("{cleaned} credits"));
    Some(format!("{:<LABEL_W$}{amount}", "Credits:"))
}

fn truncate_middle(value: &str, max: usize) -> String {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    if max <= 3 {
        return "...".to_string();
    }
    let keep = (max - 3) / 2;
    let mut start = String::with_capacity(keep);
    let mut end = String::with_capacity(keep);

    for ch in cleaned.chars().take(keep) {
        start.push(ch);
    }
    for ch in cleaned
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        end.push(ch);
    }
    format!("{start}...{end}")
}

fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(width)) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn apply_margin(area: Rect, margin: Margin) -> Rect {
    let x = area.x.saturating_add(margin.horizontal);
    let y = area.y.saturating_add(margin.vertical);
    let width = area
        .width
        .saturating_sub(margin.horizontal.saturating_mul(2));
    let height = area
        .height
        .saturating_sub(margin.vertical.saturating_mul(2));
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn compute_bar_layout(area: Rect, bars: u16, padding_x: u16) -> (u16, u16) {
    if bars == 0 {
        return (1, 1);
    }
    // Roughly account for the chart border + padding.
    let inner_width = area
        .width
        .saturating_sub(2)
        .saturating_sub(padding_x.saturating_mul(2));

    // Prefer a gap of 1, but fall back to 0 if bars would be too cramped.
    let mut gap = 1u16;
    let mut width = (inner_width.saturating_sub(gap.saturating_mul(bars.saturating_sub(1)))) / bars;
    if width == 0 {
        gap = 0;
        width = inner_width / bars;
    }
    if width == 0 {
        width = 1;
    }
    (width, gap)
}

fn inset_with_border_and_padding(area: Rect, padding: Padding) -> Rect {
    // Account for a 1-cell border on all sides.
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    Rect {
        x: inner.x.saturating_add(padding.left),
        y: inner.y.saturating_add(padding.top),
        width: inner
            .width
            .saturating_sub(padding.left.saturating_add(padding.right)),
        height: inner
            .height
            .saturating_sub(padding.top.saturating_add(padding.bottom)),
    }
}

// counts-line helper removed (values are rendered inside bars now)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_chart_header_explains_the_slash_pair() {
        assert_eq!(
            usage_chart_metric_label(UsageMetric::Tokens),
            "TOKENS (TOTAL / NON-CACHED)"
        );
    }

    #[test]
    fn truncate_middle_is_unicode_safe_and_strips_control_chars() {
        let input = "ab\x1b[31mcd\x1b[0m-zu\u{0308}rich";
        let out = truncate_middle(input, 10);
        assert!(!out.chars().any(|c| c.is_control()));
        assert!(out.chars().count() <= 10);
    }

    #[test]
    fn truncate_middle_handles_small_limits() {
        assert_eq!(truncate_middle("hello", 0), "...");
        assert_eq!(truncate_middle("hello", 1), "...");
        assert_eq!(truncate_middle("hello", 2), "...");
        assert_eq!(truncate_middle("hello", 3), "...");
    }

    #[test]
    fn card_row_uses_the_tallest_measured_card() {
        let short = CardSpec::new("42".to_string(), vec!["One line".to_string()]);
        let tall = CardSpec::new(
            "Weekly limit".to_string(),
            vec!["Resets: 3 available | earliest expires 21 Jul".to_string()],
        );
        let width = 20;
        let expected = tall.required_height(width);

        assert_eq!(card_row_height(&[short, tall], width), expected);
    }

    #[test]
    fn short_usage_layout_preserves_measured_card_height() {
        let area = Rect::new(0, 0, 140, 18);
        let chunks = usage_layout(area, 2, 12);

        assert_eq!(chunks[0].height, 2);
        assert_eq!(chunks[1].height, 12);
        assert_eq!(chunks[2].height, 1);
        assert_eq!(chunks[3].height, 3);
    }

    #[test]
    fn card_format_density_uses_the_actual_card_width() {
        let six_columns = usage_card_layout(180);
        assert_eq!(six_columns.columns, 6);
        assert_eq!(six_columns.min_card_width, 28);
        assert!(uses_compact_limit_lines(six_columns.min_card_width));

        let three_columns = usage_card_layout(179);
        assert_eq!(three_columns.columns, 3);
        assert_eq!(three_columns.min_card_width, 59);
        assert!(!uses_compact_limit_lines(three_columns.min_card_width));
    }

    #[test]
    fn wrapped_line_count_matches_word_wrapping_and_long_words() {
        assert_eq!(wrapped_line_count("one two three", 7), 2);
        assert_eq!(wrapped_line_count("abcdefghijk", 10), 2);
        assert!(wrapped_line_count(footer_hint(ActiveScreen::Usage), 48) > 1);
    }

    #[test]
    fn reset_summary_uses_earliest_returned_expiration() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let limits = crate::codex_rpc::AccountRateLimits {
            limit_id: None,
            limit_name: None,
            individual_limit: None,
            primary: None,
            secondary: None,
            credits: None,
            buckets: Vec::new(),
            reset_credits_available: Some(3),
            reset_credits: Some(vec![
                crate::codex_rpc::RateLimitResetCredit {
                    id: Some("later".to_string()),
                    reset_type: Some("codexRateLimits".to_string()),
                    status: Some("available".to_string()),
                    granted_at: None,
                    expires_at: Some(1_784_434_782),
                    title: None,
                    description: None,
                },
                crate::codex_rpc::RateLimitResetCredit {
                    id: Some("earlier".to_string()),
                    reset_type: Some("codexRateLimits".to_string()),
                    status: Some("available".to_string()),
                    granted_at: None,
                    expires_at: Some(1_784_334_782),
                    title: None,
                    description: None,
                },
            ]),
        };

        let summary = reset_summary_for_limits(&limits, formatter).expect("reset summary");
        assert!(summary.starts_with("Resets: 3 available | earliest expires "));
        assert!(summary.contains(", "));
    }

    #[test]
    fn reset_summary_height_accounts_for_its_label() {
        let summary = "Resets: 3 available | earliest expires 18 Jul";
        assert!(reset_summary_height(Some(summary), 24) > wrapped_line_count(summary, 24));
        assert_eq!(usage_controls_height(None, 80), 1);
    }

    #[test]
    fn limits_card_uses_monthly_and_named_rolling_bucket() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let limits = crate::codex_rpc::AccountRateLimits {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            individual_limit: Some(crate::codex_rpc::SpendControlLimitSnapshot {
                limit: Some("60000".to_string()),
                remaining_percent: Some(99.0),
                resets_at: None,
                used: Some("564".to_string()),
            }),
            primary: None,
            secondary: None,
            credits: None,
            buckets: vec![crate::codex_rpc::RateLimitSnapshot {
                limit_id: Some("codex_bengalfox".to_string()),
                limit_name: Some("GPT-5.3-Codex-Spark-Preview".to_string()),
                individual_limit: None,
                primary: Some(crate::codex_rpc::RateLimitWindow {
                    used_percent: Some(0.0),
                    window_duration_mins: Some(300.0),
                    resets_at: None,
                }),
                secondary: Some(crate::codex_rpc::RateLimitWindow {
                    used_percent: Some(0.0),
                    window_duration_mins: Some(10080.0),
                    resets_at: None,
                }),
                credits: None,
            }],
            reset_credits_available: None,
            reset_credits: None,
        };

        let (value, caption1, caption2, caption3) =
            format_limits_compact_card_lines(&limits, false, formatter);
        assert_eq!(value, "Monthly:  99%");
        assert_eq!(caption1.as_deref(), Some("Credits:  564/60,000 used"));
        assert_eq!(caption2.as_deref(), Some("5h limit: 100%"));
        assert_eq!(caption3.as_deref(), Some("Weekly:   100%"));

        let (_, _, compact_primary, compact_secondary) =
            format_limits_compact_card_lines(&limits, true, formatter);
        assert_eq!(compact_primary.as_deref(), Some("5h: 100%"));
        assert_eq!(compact_secondary.as_deref(), Some("7d: 100%"));
    }

    #[test]
    fn limits_card_treats_weekly_primary_as_weekly() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let limits = crate::codex_rpc::AccountRateLimits {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            individual_limit: None,
            primary: Some(crate::codex_rpc::RateLimitWindow {
                used_percent: Some(4.0),
                window_duration_mins: Some(10080.0),
                resets_at: None,
            }),
            secondary: None,
            credits: None,
            buckets: Vec::new(),
            reset_credits_available: None,
            reset_credits: None,
        };

        let (short_window, weekly_window) = rolling_windows_for_limits(&limits);
        assert!(short_window.is_none());
        assert_eq!(
            weekly_window.and_then(|window| window.window_duration_mins),
            Some(10080.0)
        );

        let (value, caption1, _, _) = format_limits_compact_card_lines(&limits, false, formatter);
        assert_eq!(value, "Weekly:   96%");
        assert_eq!(caption1.as_deref(), Some("5h limit: --"));

        let (compact_value, compact_caption1, _, _) =
            format_limits_compact_card_lines(&limits, true, formatter);
        assert_eq!(compact_value, "7d: 96%");
        assert_eq!(compact_caption1.as_deref(), Some("5h limit: --"));
    }

    #[test]
    fn classic_horizontal_tokens_keep_full_total_and_non_cached() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let out = format_horizontal_value(
            45_456_785,
            Some(1_756_241),
            UsageMetric::Tokens,
            u16::MAX,
            formatter,
        );
        assert_eq!(out, "45,456,785 / 1,756,241");
    }

    #[test]
    fn system_horizontal_tokens_use_compact_total_and_non_cached() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter =
            DisplayFormatter::new(crate::locale::DisplayStyle::SystemCompact, &system_locale);
        let out = format_horizontal_value(
            45_456_785,
            Some(1_756_241),
            UsageMetric::Tokens,
            u16::MAX,
            formatter,
        );
        assert_eq!(out, "45.46M / 1.76M");
    }

    #[test]
    fn vertical_token_values_compact_only_in_system_mode() {
        let system_locale = crate::locale::SystemLocale::default();
        let classic = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let system =
            DisplayFormatter::new(crate::locale::DisplayStyle::SystemCompact, &system_locale);

        assert_eq!(
            format_vertical_token_value(45_456_785, 20, classic),
            "45456785"
        );
        assert_eq!(
            format_vertical_token_value(45_456_785, 20, system),
            "45.46M"
        );
    }

    #[test]
    fn system_full_keeps_chart_token_values_expanded() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter =
            DisplayFormatter::new(crate::locale::DisplayStyle::SystemFull, &system_locale);

        assert_eq!(
            format_horizontal_value(
                45_456_785,
                Some(1_756_241),
                UsageMetric::Tokens,
                u16::MAX,
                formatter,
            ),
            "45456785 / 1756241"
        );
        assert_eq!(
            format_vertical_token_value(45_456_785, 20, formatter),
            "45456785"
        );
        assert_eq!(
            format_horizontal_value(
                45_456_785,
                Some(1_756_241),
                UsageMetric::Tokens,
                8,
                formatter,
            ),
            "45456785 / 1756241"
        );
        assert_eq!(
            format_vertical_token_value(45_456_785, 4, formatter),
            "45456785"
        );
    }

    #[test]
    fn horizontal_tokens_pair_compacts_when_tight() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let out = format_horizontal_value(
            45_456_785,
            Some(1_756_241),
            UsageMetric::Tokens,
            10,
            formatter,
        );
        assert!(out.contains(" / "));
        assert!(!out.is_empty());
    }

    #[test]
    fn project_activity_tokens_show_total_and_out_of_cache() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let project = ProjectActivity {
            display_path: "/tmp/SFM".to_string(),
            days: Vec::new(),
            last_activity_day: Some("2026-06-05".to_string()),
            total_tokens: 250,
            cached_input_tokens: 150,
            agent_time_ms: 0,
            agent_runs: 0,
        };

        assert_eq!(
            activity_metric_total_label(&project, UsageMetric::Tokens, formatter),
            "250 / 100 tokens"
        );
    }

    #[test]
    fn activity_color_levels_scale_to_project_max() {
        assert_eq!(activity_color_level(0, 100), 0);
        assert_eq!(activity_color_level(1, 100), 1);
        assert_eq!(activity_color_level(50, 100), 2);
        assert_eq!(activity_color_level(100, 100), 4);
    }

    #[test]
    fn activity_project_name_uses_path_leaf() {
        assert_eq!(activity_project_name("/tmp/Photonia"), "Photonia");
        assert_eq!(activity_project_name(r"C:\src\SFM"), "SFM");
    }

    #[test]
    fn activity_day_cell_width_expands_when_all_weeks_fit() {
        assert_eq!(activity_day_cell_width(108, 54), 2);
        assert_eq!(activity_day_cell_width(107, 54), 1);
    }

    #[test]
    fn activity_weekday_labels_follow_first_weekday() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        assert_eq!(activity_weekday_label(Weekday::Mon, 0, formatter), "Mon");
        assert_eq!(activity_weekday_label(Weekday::Mon, 6, formatter), "Sun");
        assert_eq!(activity_weekday_label(Weekday::Sun, 0, formatter), "");
        assert_eq!(activity_weekday_label(Weekday::Sun, 1, formatter), "Mon");
        assert_eq!(activity_weekday_label(Weekday::Sun, 6, formatter), "");
        assert_eq!(activity_weekday_label(Weekday::Sat, 2, formatter), "Mon");
    }
}
