use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Sparkline, Wrap},
};

use crate::i18n::I18n;

use super::super::{
    state::{StatsFilter, StatsPage, StatsScreen},
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::{centered_fixed, draw_input_field},
};

pub(super) fn draw_stats(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    loading: bool,
    i18n: &I18n,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(5),
        ])
        .split(area);
    let tabs = Line::from(vec![
        tab(
            &format!("1 {}", i18n.text("stats_overview")),
            screen.page == StatsPage::Overview,
        ),
        Span::raw("  "),
        tab(
            &format!("2 {}", i18n.text("stats_models")),
            screen.page == StatsPage::Models,
        ),
    ]);
    frame.render_widget(Paragraph::new(tabs), rows[0]);
    let provider_label = screen.provider_id.as_ref().map_or_else(
        || i18n.text("stats_all").to_owned(),
        |id| {
            screen
                .report
                .as_ref()
                .and_then(|report| {
                    report
                        .filters
                        .providers
                        .iter()
                        .find(|provider| &provider.id == id)
                })
                .map_or_else(
                    || id.clone(),
                    |provider| {
                        format!(
                            "{}{}",
                            if provider.inferred { "~" } else { "" },
                            provider.name
                        )
                    },
                )
        },
    );
    let filters = format!(
        "{} — {}  ·  {}: {}  ·  {}: {}{}",
        screen.from,
        screen.to,
        i18n.text("stats_provider"),
        provider_label,
        i18n.text("stats_model"),
        screen
            .model
            .as_deref()
            .unwrap_or_else(|| i18n.text("stats_all")),
        if loading { "  ·  …" } else { "" }
    );
    frame.render_widget(
        Paragraph::new(filters).style(Style::default().fg(MUTED)),
        rows[1],
    );
    match (&screen.report, screen.page) {
        (Some(report), StatsPage::Overview) => {
            draw_overview(frame, rows[2], report, screen.scroll, i18n);
        }
        (Some(report), StatsPage::Models) => {
            draw_models(frame, rows[2], report, screen.scroll, i18n);
        }
        (None, _) => frame.render_widget(
            Paragraph::new(i18n.text("loading")).style(Style::default().fg(MUTED)),
            rows[2],
        ),
    }
    if let Some(filter) = &screen.filter {
        draw_filter(frame, area, screen, filter, i18n);
    }
}

fn draw_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &hsin_core::UsageStatsReport,
    scroll: u16,
    i18n: &I18n,
) {
    if area.height < 14 || area.width < 56 {
        draw_summary(frame, area, report, scroll, i18n);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(5)])
        .split(area);
    draw_heatmap(frame, rows[0], report, i18n);
    draw_summary(frame, rows[1], report, scroll, i18n);
}

fn draw_heatmap(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &hsin_core::UsageStatsReport,
    i18n: &I18n,
) {
    let daily = report
        .daily
        .iter()
        .map(|bucket| (bucket.date.as_str(), bucket.tokens.total_tokens()))
        .collect::<BTreeMap<_, _>>();
    let Some(from) = local_date(report.query.from) else {
        return;
    };
    let Some(to) = local_date(report.query.to.saturating_sub(1)) else {
        return;
    };
    let start = from - chrono::Duration::days(i64::from(from.weekday().num_days_from_monday()));
    let max = daily.values().copied().max().unwrap_or(0);
    let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let mut lines = Vec::new();
    for (weekday, label) in weekdays.iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!("{label} "),
            Style::default().fg(MUTED),
        )];
        let mut day = start + chrono::Duration::days(i64::try_from(weekday).unwrap_or(0));
        while day <= to {
            if day < from {
                spans.push(Span::raw("  "));
            } else {
                let key = day.format("%Y-%m-%d").to_string();
                let value = daily.get(key.as_str()).copied().unwrap_or(0);
                let color = heat_color(value, max);
                spans.push(Span::styled("■ ", Style::default().fg(color)));
            }
            day += chrono::Duration::days(7);
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(i18n.text("stats_daily_tokens"))
                .borders(Borders::BOTTOM),
        ),
        area,
    );
}

fn draw_summary(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &hsin_core::UsageStatsReport,
    scroll: u16,
    i18n: &I18n,
) {
    let tokens = &report.summary;
    let mut lines = vec![
        Line::from(vec![
            metric(i18n.text("stats_total_tokens"), tokens.total_tokens()),
            Span::raw("   "),
            Span::styled(
                format!(
                    "{} {:.1}%",
                    i18n.text("stats_hit_rate"),
                    tokens.cache_hit_rate() * 100.0
                ),
                Style::default().fg(WHITE),
            ),
        ]),
        Line::from(format!(
            "{} {}  ·  {} {}  ·  {} {}",
            i18n.text("stats_hit"),
            compact(tokens.cache_read_tokens),
            i18n.text("stats_non_hit"),
            compact(tokens.non_hit_tokens()),
            i18n.text("stats_requests"),
            tokens.request_count
        )),
        Line::from(format!(
            "{} {}  ·  {} {}  ·  {} {}  ·  {} {}",
            i18n.text("stats_input"),
            compact(tokens.input_tokens),
            i18n.text("stats_cache_write"),
            compact(tokens.cache_write_tokens),
            i18n.text("stats_output"),
            compact(tokens.output_tokens),
            i18n.text("stats_reasoning"),
            compact(tokens.reasoning_output_tokens)
        )),
        Line::from(Span::styled(
            format!(
                "{} {}  ·  {} {}  ·  {} {}",
                i18n.text("stats_exact"),
                report.attribution.exact,
                i18n.text("stats_inferred"),
                report.attribution.inferred,
                i18n.text("stats_unattributed"),
                report.attribution.unattributed
            ),
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            i18n.text("stats_source_note"),
            Style::default().fg(MUTED),
        )),
    ];
    lines.extend(report.providers.iter().take(4).map(|provider| {
        Line::from(format!(
            "{}{}  {}",
            if provider.inferred { "~" } else { "" },
            provider.provider_name,
            compact(provider.tokens.total_tokens())
        ))
    }));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_models(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &hsin_core::UsageStatsReport,
    scroll: u16,
    i18n: &I18n,
) {
    if report.models.is_empty() {
        frame.render_widget(
            Paragraph::new(i18n.text("stats_no_usage")).style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    if area.width < 60 || area.height < 14 {
        let lines = report
            .models
            .iter()
            .map(|model| model_line(model, i18n))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .scroll((scroll, 0)),
            area,
        );
        return;
    }
    let chart_count = report.models.len().min(4);
    let chart_height = (area.height / 2).max(6);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(chart_height), Constraint::Min(4)])
        .split(area);
    let charts = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            (0..chart_count).map(|_| Constraint::Ratio(1, u32::try_from(chart_count).unwrap_or(1))),
        )
        .split(rows[0]);
    for (model, chart_area) in report.models.iter().take(4).zip(charts.iter()) {
        let values = model
            .daily
            .iter()
            .map(|bucket| bucket.tokens.total_tokens())
            .collect::<Vec<_>>();
        frame.render_widget(
            Sparkline::default()
                .data(&values)
                .style(Style::default().fg(RED))
                .block(Block::default().title(format!(
                    "{} · {}",
                    model.model,
                    compact(model.tokens.total_tokens())
                ))),
            *chart_area,
        );
    }
    let mut lines = report
        .models
        .iter()
        .take(4)
        .map(|model| model_line(model, i18n))
        .collect::<Vec<_>>();
    if report.models.len() > 4 {
        let other = report.models[4..]
            .iter()
            .map(|model| model.tokens.total_tokens())
            .sum::<u64>();
        lines.push(Line::from(format!(
            "{}  {}",
            i18n.text("stats_other"),
            compact(other)
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0)),
        rows[1],
    );
}

#[allow(clippy::too_many_lines)]
fn draw_filter(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    filter: &StatsFilter,
    i18n: &I18n,
) {
    let popup = centered_fixed(
        area,
        58,
        if matches!(filter, StatsFilter::Time { selected: 4, .. }) {
            11
        } else {
            10
        },
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .title(match filter {
                StatsFilter::Time { .. } => i18n.text("stats_time_range"),
                StatsFilter::Provider { .. } => i18n.text("stats_provider"),
                StatsFilter::Model { .. } => i18n.text("stats_model"),
            })
            .borders(Borders::ALL)
            .border_style(Style::default().fg(RED))
            .style(Style::default().bg(INPUT_BG)),
        popup,
    );
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    match filter {
        StatsFilter::Time {
            selected,
            custom_field,
            cursor,
        } => {
            let labels = [
                i18n.text("stats_today"),
                i18n.text("stats_last_7"),
                i18n.text("stats_last_30"),
                i18n.text("stats_last_90"),
                i18n.text("stats_custom"),
            ];
            let line = Line::from(
                labels
                    .iter()
                    .enumerate()
                    .flat_map(|(index, label)| {
                        [
                            Span::styled(
                                format!(" {label} "),
                                if index == *selected {
                                    Style::default()
                                        .fg(WHITE)
                                        .bg(RED)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(MUTED)
                                },
                            ),
                            Span::raw(" "),
                        ]
                    })
                    .collect::<Vec<_>>(),
            );
            frame.render_widget(Paragraph::new(line), Rect { height: 1, ..inner });
            if *selected == 4 {
                let fields = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(Rect {
                        y: inner.y + 2,
                        height: 3,
                        ..inner
                    });
                draw_input_field(
                    frame,
                    fields[0],
                    i18n.text("stats_from"),
                    &screen.from,
                    Some("YYYY-MM-DD"),
                    (*custom_field == 0).then_some(*cursor),
                    true,
                );
                draw_input_field(
                    frame,
                    fields[1],
                    i18n.text("stats_to"),
                    &screen.to,
                    Some("YYYY-MM-DD"),
                    (*custom_field == 1).then_some(*cursor),
                    true,
                );
            }
        }
        StatsFilter::Provider { selected } => {
            let mut items = vec![ListItem::new(i18n.text("stats_all"))];
            if let Some(report) = &screen.report {
                items.extend(report.filters.providers.iter().map(|provider| {
                    ListItem::new(format!(
                        "{}{}",
                        if provider.inferred { "~" } else { "" },
                        provider.name
                    ))
                }));
            }
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(
                List::new(items).highlight_style(Style::default().fg(WHITE).bg(RED)),
                inner,
                &mut state,
            );
        }
        StatsFilter::Model { selected } => {
            let mut items = vec![ListItem::new(i18n.text("stats_all"))];
            if let Some(report) = &screen.report {
                items.extend(report.filters.models.iter().cloned().map(ListItem::new));
            }
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(
                List::new(items).highlight_style(Style::default().fg(WHITE).bg(RED)),
                inner,
                &mut state,
            );
        }
    }
}

fn tab(label: &str, selected: bool) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        if selected {
            Style::default()
                .fg(WHITE)
                .bg(RED)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        },
    )
}

fn metric(label: &str, value: u64) -> Span<'static> {
    Span::styled(
        format!("{label} {}", compact(value)),
        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
    )
}

fn model_line(model: &hsin_core::UsageModelBreakdown, i18n: &I18n) -> Line<'static> {
    Line::from(format!(
        "{}  {} · {} {} · {} {} · {} {}",
        model.model,
        compact(model.tokens.total_tokens()),
        i18n.text("stats_input"),
        compact(model.tokens.input_tokens),
        i18n.text("stats_hit"),
        compact(model.tokens.cache_read_tokens),
        i18n.text("stats_output"),
        compact(model.tokens.output_tokens)
    ))
}

fn compact(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!(
            "{}.{}b",
            value / 1_000_000_000,
            value % 1_000_000_000 / 100_000_000
        )
    } else if value >= 1_000_000 {
        format!("{}.{}m", value / 1_000_000, value % 1_000_000 / 100_000)
    } else if value >= 1_000 {
        format!("{}.{}k", value / 1_000, value % 1_000 / 100)
    } else {
        value.to_string()
    }
}

fn local_date(timestamp: i64) -> Option<NaiveDate> {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|value| value.with_timezone(&Local).date_naive())
}

fn heat_color(value: u64, max: u64) -> ratatui::style::Color {
    if value == 0 || max == 0 {
        MUTED
    } else {
        let intensity = (80 + 175 * value / max).min(255) as u8;
        ratatui::style::Color::Rgb(intensity, 50, 55)
    }
}
