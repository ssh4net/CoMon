use crate::app::{ActiveScreen, AppState, ChartOrientation};
use crate::usage::{
    format_compact_kmb, format_count, format_duration_compact, format_tokens_compact, ChartRange,
    UsageMetric,
};
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, TimeZone};
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
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ])
                .split(inner);

            render_header(frame, chunks[0], state);
            render_usage(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area);
            }
            if state.no_sessions_confirm_open {
                render_no_sessions_overlay(frame, area, state);
            }
        }
        ActiveScreen::Read => {
            crate::read::tui::render(frame, inner, &mut state.read_browser);
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

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let err = state
        .usage_error
        .as_deref()
        .or(state.limits_error.as_deref())
        .unwrap_or("");
    let err_span = if err.is_empty() {
        Span::raw("")
    } else {
        Span::styled(truncate_middle(err, 80), Style::default().fg(Color::Red))
    };

    let usage_hint =
        "Usage: Statistic [tab] (tokens/time/runs), Timeframe [w] (week/month), Layout [f] (horizontal/vertical), Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]";
    let line = Line::from(vec![
        Span::styled(usage_hint, Style::default().fg(Color::Gray)),
        Span::raw("  "),
        err_span,
    ]);

    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), area);
}

fn render_usage(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let cards_height = if area.width >= 150 { 7 } else { 15 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(cards_height),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    render_usage_controls(frame, chunks[0], state);
    render_usage_cards(frame, chunks[1], state);
    render_usage_chart(frame, chunks[2], state);
    render_top_models(frame, chunks[3], state);
}

fn render_usage_controls(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Min(0)])
        .split(area);

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
}

fn render_usage_cards(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    // With an extra LIMITS card, we prefer 2 rows unless the layout is wide enough.
    let two_rows = area.width < 180 && area.height >= 12;
    let (row1, row2) = if two_rows {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        (Some(rows[0]), Some(rows[1]))
    } else {
        (Some(area), None)
    };

    let snapshot = state.usage.as_ref();
    let totals = snapshot.map(|s| s.totals_view(state.metric));
    let today = snapshot.and_then(|s| s.days.last());

    // LIMITS card (live from Codex app-server).
    let (limits_value, limits_caption1, limits_caption2): (String, Option<String>, Option<String>) =
        if !state.limits_enabled {
            let msg = state
                .limits_error
                .as_deref()
                .unwrap_or("Limits unavailable.");
            ("Unavailable".to_string(), Some(msg.to_string()), None)
        } else if let Some(l) = state.limits.as_ref() {
            let (l1, l2) = format_limits_compact_lines(l);
            let l3 = format_credits_compact_line(l).unwrap_or_else(|| "Credits:  --".to_string());
            (l1, Some(l2), Some(l3))
        } else {
            ("Loading...".to_string(), None, None)
        };

    // TODAY card always shows Tokens / Runs / Time.
    let today_value = today
        .map(|d| format!("{} tokens", format_count(d.total_tokens)))
        .unwrap_or_else(|| "--".to_string());
    let today_caption1 = today
        .map(|d| format!("Runs {}", format_count(d.agent_runs)))
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
        Some(format!("Runs {}", format_count(last7_runs_sum)))
    } else {
        None
    };
    let last30_runs_caption = if snapshot.is_some() {
        Some(format!("Runs {}", format_count(last30_runs_sum)))
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
                        card(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
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
                        card(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
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
                        card(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
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
                        card(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
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
                format_count(avg30)
            } else {
                "--".into()
            };

            let tokens7 = snapshot
                .map(|s| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(s.totals.last7_days_tokens)
                    )
                })
                .unwrap_or_else(|| "--".into());
            let tokens30 = snapshot
                .map(|s| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(s.totals.last30_days_tokens)
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
                        card(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
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
                        card(
                            "LIMITS",
                            &limits_value,
                            limits_caption1.as_deref(),
                            limits_caption2.as_deref(),
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

    let range_label = match state.range {
        ChartRange::Week => "Last 7 days",
        ChartRange::Month => "Last 30 days",
    };
    let metric_label = match state.metric {
        UsageMetric::Tokens => "TOKENS",
        UsageMetric::Time => "TIME",
        UsageMetric::Runs => "RUNS",
    };
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
            ChartOrientation::Horizontal => format_day_label_weekday_mmdd(&day.day),
            ChartOrientation::Vertical => match state.range {
                ChartRange::Week => day.short_label(),
                ChartRange::Month => {
                    // Prefer compact day-of-month labels for dense charts.
                    if day.day.len() == 10 {
                        day.day[8..10].to_string()
                    } else {
                        day.short_label()
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
                        UsageMetric::Tokens => {
                            let raw = value.to_string();
                            if raw.len() <= w as usize {
                                raw
                            } else {
                                format_compact_kmb(*value, w)
                            }
                        }
                        UsageMetric::Time => {
                            let raw = format_minutes_hhmm(*value);
                            if raw.len() <= w as usize {
                                raw
                            } else if w >= 4 {
                                // fallback: compact minutes
                                format_compact_kmb(*value, w)
                            } else {
                                String::new()
                            }
                        }
                        UsageMetric::Runs => {
                            let raw = value.to_string();
                            if raw.len() <= w as usize {
                                raw
                            } else {
                                format_compact_kmb(*value, w)
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
            let bar_h = preferred_h.min(5).max(1);

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
                    let s = format_horizontal_value(*v, out_of_cache, state.metric, u16::MAX);
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
                let value_text =
                    format_horizontal_value(*value, out_of_cache, state.metric, value_max_width);
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

fn format_day_label_weekday_mmdd(day: &str) -> String {
    // Input: YYYY-MM-DD
    // Output: Mon 02/02
    let parsed = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok();
    let Some(date) = parsed else {
        return day.to_string();
    };
    let weekday = match date.weekday().number_from_monday() {
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "Sun",
    };
    format!("{weekday} {:02}/{:02}", date.month(), date.day())
}

fn format_horizontal_value(
    value: u64,
    out_of_cache_tokens: Option<u64>,
    metric: UsageMetric,
    max_width: u16,
) -> String {
    if max_width == 0 {
        return String::new();
    }

    if metric == UsageMetric::Tokens {
        if let Some(out_of_cache) = out_of_cache_tokens {
            let full = format!(
                "{} / {}",
                format_count(value as i64),
                format_count(out_of_cache as i64)
            );
            if full.len() <= max_width as usize {
                return full;
            }

            let compact = format!(
                "{} / {}",
                format_tokens_compact(value as i64),
                format_tokens_compact(out_of_cache as i64)
            );
            if compact.len() <= max_width as usize {
                return compact;
            }

            let pair_width = max_width.saturating_sub(1);
            if pair_width >= 2 {
                let mut left_w = ((pair_width as usize * 2) / 3) as u16;
                left_w = left_w.clamp(1, pair_width.saturating_sub(1));
                let right_w = pair_width.saturating_sub(left_w);
                return format!(
                    "{} / {}",
                    format_compact_kmb(value, left_w),
                    format_compact_kmb(out_of_cache, right_w),
                );
            }
        }
    }

    let full = match metric {
        UsageMetric::Tokens => format_count(value as i64),
        UsageMetric::Time => format_minutes_hhmm(value),
        UsageMetric::Runs => format_count(value as i64),
    };
    if full.len() <= max_width as usize {
        return full;
    }

    // If we can't fit the full value, try compact with the same suffix.
    // (No suffix for horizontal values; the chart header indicates the unit.)

    // Final fallback: compact only.
    format_compact_kmb(value, max_width)
}

fn format_duration_words(ms: i64) -> String {
    let mut secs = (ms.max(0) / 1000) as i64;
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
                format!("{} {:.1}%", truncate_middle(&m.model, 18), m.share_percent),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let w = area.width.min(60);
    let h = area.height.min(12);
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

    let text = Text::from(vec![
        Line::from("Keys:"),
        Line::from("  Tab  - toggle statistic (Tokens/Time/Runs)"),
        Line::from("  w    - toggle timeframe (Week/Month)"),
        Line::from("  f    - toggle layout (Horz/Vert)"),
        Line::from("  r/F5 - refresh usage + limits"),
        Line::from("  ?    - toggle help"),
        Line::from("  q/Esc - quit"),
    ]);
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
    if let Some(caption) = caption1 {
        if !caption.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                caption.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }
    if let Some(caption) = caption2 {
        if !caption.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                caption.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }

    Paragraph::new(Text::from(lines))
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true })
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

fn resets_label(resets_at: Option<i64>) -> Option<String> {
    let raw = resets_at?;
    let ms = crate::codex_rpc::normalize_epoch_millis(raw);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    let today = Local::now().date_naive();
    let day = dt.date_naive();
    let time = dt.format("%H:%M").to_string();
    if day == today {
        Some(format!("resets {time}"))
    } else {
        let month = dt.format("%b").to_string();
        Some(format!("resets {time}, {} {month}", day.day()))
    }
}

fn format_limit_compact_line(
    label_with_colon: &str,
    window: Option<&crate::codex_rpc::RateLimitWindow>,
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
    let resets = resets_label(w.resets_at)
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    format!("{label}{pct}{resets}")
}

fn format_limits_compact_lines(l: &crate::codex_rpc::AccountRateLimits) -> (String, String) {
    let primary_label = l
        .primary
        .as_ref()
        .and_then(|w| w.window_duration_mins)
        .and_then(format_window_label)
        .map(|w| format!("{w} limit:"))
        .unwrap_or_else(|| "5h limit:".to_string());
    let l1 = format_limit_compact_line(&primary_label, l.primary.as_ref());
    let l2 = format_limit_compact_line("Weekly:", l.secondary.as_ref());
    (l1, l2)
}

fn format_credits_compact_line(l: &crate::codex_rpc::AccountRateLimits) -> Option<String> {
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
        .map(|v| format!("{} credits", v.round() as i64))
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
    fn horizontal_tokens_show_total_and_out_of_cache() {
        let out =
            format_horizontal_value(45_456_785, Some(1_756_241), UsageMetric::Tokens, u16::MAX);
        assert_eq!(out, "45,456,785 / 1,756,241");
    }

    #[test]
    fn horizontal_tokens_pair_compacts_when_tight() {
        let out = format_horizontal_value(45_456_785, Some(1_756_241), UsageMetric::Tokens, 10);
        assert!(out.contains(" / "));
        assert!(!out.is_empty());
    }
}
