use std::collections::BTreeMap;

use chrono::{Datelike, Duration, Local, NaiveDate};
use hsin_core::{StatsChartStyle, UsageCalendarDay, UsageQuotaEstimate, UsageStatsReport};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    i18n::I18n,
    usage_format::{compact_tokens as compact, format_cost},
};

use super::super::{
    mouse::{ENTER, Hit, HitMap, plain},
    state::{
        DayDetail, QuotaFilter, STATS_RANGE_CHIPS, STATS_TIME_CUSTOM, StatsFilter, StatsPage,
        StatsScreen, current_time_preset, quota_plans, visible_quota,
    },
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::{centered_fixed, display_width, draw_input_field},
};
use super::chart::{Tooltip, draw_series, draw_tooltip, legend, split};

/// Heatmap intensities from idle to busiest.
const HEAT: [Color; 5] = [
    Color::Rgb(48, 48, 56),
    Color::Rgb(96, 38, 45),
    Color::Rgb(140, 45, 54),
    Color::Rgb(180, 52, 62),
    RED,
];
const HEAT_LABEL_WIDTH: u16 = 4;
const HEATMAP_HEIGHT: u16 = 9;
const MAX_WEEKS: u16 = 53;

/// Labels of the time popup, in the order of its presets.
const TIME_LABELS: [&str; 6] = [
    "stats_today",
    "stats_last_7",
    "stats_last_30",
    "stats_last_90",
    "stats_all_time",
    "stats_custom",
];

pub(super) fn draw_stats(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    loading: bool,
    style: StatsChartStyle,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(5),
        ])
        .split(area);
    let tabs_width = draw_tabs(frame, rows[0], screen, i18n, hits);
    draw_range_chips(frame, rows[0], tabs_width, screen, i18n, hits);
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
    let range = if screen.all_time {
        i18n.text("stats_all_time").to_owned()
    } else {
        format!("{} — {}", screen.from, screen.to)
    };
    let plan_label = match &screen.quota_filter {
        QuotaFilter::Recent => i18n.text("stats_quota_recent").to_owned(),
        QuotaFilter::All => i18n.text("stats_quota_all").to_owned(),
        QuotaFilter::Plan(key) => screen
            .report
            .as_ref()
            .map(quota_plans)
            .and_then(|plans| {
                plans
                    .into_iter()
                    .find(|(plan, _)| plan == key)
                    .map(|(_, label)| label)
            })
            .unwrap_or_else(|| key.clone()),
    };
    let filters = format!(
        "{range}  ·  {}: {}  ·  {}: {}  ·  {}: {plan_label}{}",
        i18n.text("stats_provider"),
        provider_label,
        i18n.text("stats_model"),
        screen
            .model
            .as_deref()
            .unwrap_or_else(|| i18n.text("stats_all")),
        i18n.text("stats_quota_filter"),
        if loading { "  ·  …" } else { "" }
    );
    frame.render_widget(
        Paragraph::new(filters).style(Style::default().fg(MUTED)),
        rows[1],
    );
    match (&screen.report, screen.page) {
        (Some(report), StatsPage::Overview) => {
            draw_overview(frame, rows[2], screen, report, i18n, hits);
        }
        (Some(report), StatsPage::Models) => {
            // The day popup covers the charts, so the pointer is over it rather than them.
            let pointer = screen
                .pointer
                .filter(|_| screen.day_detail.is_none() && screen.filter.is_none());
            if let Some(tooltip) =
                draw_models(frame, rows[2], report, screen.scroll, style, pointer, i18n)
            {
                draw_tooltip(frame, &tooltip, i18n);
            }
        }
        (None, _) => frame.render_widget(
            Paragraph::new(i18n.text("loading")).style(Style::default().fg(MUTED)),
            rows[2],
        ),
    }
    if let Some(filter) = &screen.filter {
        hits.barrier(area);
        draw_filter(frame, area, screen, filter, i18n, hits);
    }
    if let Some(detail) = &screen.day_detail {
        hits.barrier(area);
        if let Some(tooltip) = draw_day_popup(frame, area, detail, style, screen.pointer, i18n) {
            draw_tooltip(frame, &tooltip, i18n);
        }
    }
}

/// One day's usage: totals, when in the day it happened, and which models and providers it went
/// to.
#[allow(clippy::too_many_lines)]
fn draw_day_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    detail: &DayDetail,
    style: StatsChartStyle,
    pointer: Option<Position>,
    i18n: &I18n,
) -> Option<Tooltip> {
    let popup = centered_fixed(area, 76, 28);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(
            i18n.text("stats_day_title")
                .replace("{date}", &detail.date.format("%Y-%m-%d").to_string()),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED))
        .style(Style::default().bg(INPUT_BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let Some(report) = &detail.report else {
        frame.render_widget(
            Paragraph::new(i18n.text("loading")).style(Style::default().fg(MUTED)),
            inner,
        );
        return None;
    };
    let tokens = &report.summary;
    if tokens.request_count == 0 && tokens.total_tokens() == 0 {
        frame.render_widget(
            Paragraph::new(i18n.text("stats_no_usage")).style(Style::default().fg(MUTED)),
            inner,
        );
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Length(1),
            Constraint::Min(2),
        ])
        .split(inner);
    let bold = Style::default().fg(WHITE).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);
    let mut headline = vec![
        Span::styled(
            format!(
                "{} {}",
                compact(tokens.total_tokens()),
                i18n.text("stats_tokens")
            ),
            bold,
        ),
        Span::styled(
            format!(
                "  ·  {} {}  ·  {} {:.1}%",
                tokens.request_count,
                i18n.text("stats_requests_unit"),
                i18n.text("stats_hit_rate"),
                tokens.cache_hit_rate() * 100.0
            ),
            Style::default().fg(WHITE),
        ),
    ];
    if !report.cost.is_empty() {
        headline.push(Span::styled(
            format!("  ·  ≈{}", format_cost(&report.cost)),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(headline),
            Line::from(Span::styled(
                format!(
                    "{} {}  ·  {} {}  ·  {} {}  ·  {} {}",
                    i18n.text("stats_input"),
                    compact(tokens.input_tokens),
                    i18n.text("stats_cache_write"),
                    compact(tokens.cache_write_tokens),
                    i18n.text("stats_hit"),
                    compact(tokens.cache_read_tokens),
                    i18n.text("stats_output"),
                    compact(tokens.output_tokens)
                ),
                muted,
            )),
        ])
        .wrap(Wrap { trim: true }),
        rows[0],
    );
    let peak = report
        .hourly
        .iter()
        .map(hsin_core::UsageTokenSummary::total_tokens)
        .enumerate()
        .max_by_key(|(hour, tokens)| (*tokens, std::cmp::Reverse(*hour)))
        .filter(|(_, tokens)| *tokens > 0)
        .map(|(hour, _)| format!("  ·  {} {hour:02}:00", i18n.text("stats_peak_hour")))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(i18n.text("stats_by_hour"), bold),
            Span::styled(format!("  00 → 23{peak}"), muted),
        ])),
        rows[1],
    );
    frame.render_widget(Paragraph::new(legend(i18n)), rows[2]);
    let hours = report.hourly.iter().map(split).collect::<Vec<_>>();
    let hour_labels = (0..hours.len())
        .map(|hour| format!("{hour:02}:00"))
        .collect::<Vec<_>>();
    let tooltip = draw_series(frame, rows[3], &hours, &hour_labels, style, pointer);
    let mut lines = vec![Line::from(Span::styled(i18n.text("stats_by_model"), bold))];
    let cost = |costs: &[hsin_core::UsageCost]| {
        if costs.is_empty() {
            String::new()
        } else {
            format!(" · ≈{}", format_cost(costs))
        }
    };
    lines.extend(report.models.iter().take(5).map(|model| {
        Line::from(format!(
            "  {}  {}{}",
            model.model,
            compact(model.tokens.total_tokens()),
            cost(&model.cost)
        ))
    }));
    lines.push(Line::from(Span::styled(
        i18n.text("stats_by_provider"),
        bold,
    )));
    lines.extend(report.providers.iter().take(3).map(|provider| {
        Line::from(format!(
            "  {}{}  {}{}",
            if provider.inferred { "~" } else { "" },
            provider.provider_name,
            compact(provider.tokens.total_tokens()),
            cost(&provider.cost)
        ))
    }));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), rows[5]);
    tooltip
}

/// The page tabs; returns the width they take.
fn draw_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    i18n: &I18n,
    hits: &mut HitMap,
) -> u16 {
    let pages = [
        ('1', i18n.text("stats_overview"), StatsPage::Overview),
        ('2', i18n.text("stats_models"), StatsPage::Models),
    ];
    let mut spans = Vec::new();
    let mut x = area.x;
    for (key, label, page) in pages {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
            x = x.saturating_add(2);
        }
        let span = tab(&format!("{key} {label}"), screen.page == page);
        let width = u16::try_from(span.width()).unwrap_or(u16::MAX);
        hits.key(
            Rect {
                x,
                y: area.y,
                width: width.min(area.right().saturating_sub(x)),
                height: 1,
            },
            plain(crossterm::event::KeyCode::Char(key)),
        );
        x = x.saturating_add(width);
        spans.push(span);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    x.saturating_sub(area.x)
}

/// All time · 7 days · 30 days, right-aligned beside the tabs when they fit.
fn draw_range_chips(
    frame: &mut Frame<'_>,
    area: Rect,
    tabs_width: u16,
    screen: &StatsScreen,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    let current = current_time_preset(screen);
    let chips = STATS_RANGE_CHIPS
        .iter()
        .map(|preset| {
            (
                *preset,
                tab(i18n.text(TIME_LABELS[*preset]), current == *preset),
            )
        })
        .collect::<Vec<_>>();
    let width = chips
        .iter()
        .map(|(_, span)| u16::try_from(span.width()).unwrap_or(u16::MAX))
        .sum::<u16>()
        .saturating_add(u16::try_from(chips.len().saturating_sub(1)).unwrap_or(0));
    if tabs_width.saturating_add(width).saturating_add(2) > area.width {
        return;
    }
    let start = area.right().saturating_sub(width);
    let mut x = start;
    let mut spans = Vec::new();
    for (preset, span) in chips {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
            x = x.saturating_add(1);
        }
        let chip_width = u16::try_from(span.width()).unwrap_or(u16::MAX);
        hits.push(
            Rect {
                x,
                y: area.y,
                width: chip_width,
                height: 1,
            },
            Hit::StatsRange(preset),
        );
        x = x.saturating_add(chip_width);
        spans.push(span);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect {
            x: start,
            y: area.y,
            width,
            height: 1,
        },
    );
}

fn draw_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    report: &UsageStatsReport,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    if area.height < 16 || area.width < 56 {
        draw_summary(frame, area, screen, report, i18n);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEATMAP_HEIGHT),
            Constraint::Length(2),
            Constraint::Min(4),
        ])
        .split(area);
    draw_heatmap(frame, rows[0], report, screen.day, i18n, hits);
    draw_day_detail(frame, rows[1], report, screen.day, i18n);
    draw_summary(frame, rows[2], screen, report, i18n);
}

/// A contribution-style calendar: one column per week ending with the current one, one row per
/// weekday, as many weeks as fit up to a year.
#[allow(clippy::too_many_lines)]
fn draw_heatmap(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &UsageStatsReport,
    selected: Option<NaiveDate>,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    let days = calendar_days(&report.calendar);
    let today = days
        .keys()
        .next_back()
        .copied()
        .unwrap_or_else(|| Local::now().date_naive());
    let weeks = (area.width.saturating_sub(HEAT_LABEL_WIDTH) / 2).min(MAX_WEEKS);
    if weeks == 0 || area.height < HEATMAP_HEIGHT {
        return;
    }
    let this_monday = today - Duration::days(i64::from(today.weekday().num_days_from_monday()));
    let start = this_monday - Duration::weeks(i64::from(weeks) - 1);
    let thresholds = heat_thresholds(
        days.iter()
            .filter(|(date, _)| **date >= start && **date <= today)
            .map(|(_, day)| day.total_tokens),
    );

    let title = i18n.text("stats_activity");
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        ))),
        Rect { height: 1, ..area },
    );
    let mut legend = vec![Span::styled(
        format!("{} ", i18n.text("stats_less")),
        Style::default().fg(MUTED),
    )];
    legend.extend(
        HEAT.iter()
            .map(|color| Span::styled("■ ", Style::default().fg(*color))),
    );
    legend.push(Span::styled(
        i18n.text("stats_more"),
        Style::default().fg(MUTED),
    ));
    let legend = Line::from(legend);
    let legend_width = u16::try_from(legend.width()).unwrap_or(u16::MAX);
    if legend_width.saturating_add(display_width_u16(title) + 2) <= area.width {
        frame.render_widget(
            Paragraph::new(legend),
            Rect {
                x: area.right().saturating_sub(legend_width),
                y: area.y,
                width: legend_width,
                height: 1,
            },
        );
    }

    let grid_x = area.x + HEAT_LABEL_WIDTH;
    let months = i18n.text("stats_months").split(',').collect::<Vec<_>>();
    let mut month_line = String::new();
    for week in 0..weeks {
        let monday = start + Duration::weeks(i64::from(week));
        let first = week == 0 || (monday - Duration::weeks(1)).month() != monday.month();
        let column = usize::from(week) * 2;
        let used = display_width(&month_line);
        if first && (used == 0 || used < column) {
            let label = usize::try_from(monday.month0())
                .ok()
                .and_then(|month| months.get(month))
                .copied()
                .unwrap_or_default();
            month_line.push_str(&" ".repeat(column.saturating_sub(used)));
            month_line.push_str(label);
        }
    }
    frame.render_widget(
        Paragraph::new(month_line).style(Style::default().fg(MUTED)),
        Rect {
            x: grid_x,
            y: area.y + 1,
            width: area.width.saturating_sub(HEAT_LABEL_WIDTH),
            height: 1,
        },
    );

    let weekdays = i18n.text("stats_weekdays").split(',').collect::<Vec<_>>();
    for weekday in 0..7_u16 {
        let y = area.y + 2 + weekday;
        let label = weekdays
            .get(usize::from(weekday))
            .copied()
            .unwrap_or_default();
        let mut spans = vec![Span::styled(
            format!(
                "{label}{}",
                " ".repeat(usize::from(HEAT_LABEL_WIDTH).saturating_sub(display_width(label)))
            ),
            Style::default().fg(MUTED),
        )];
        for week in 0..weeks {
            let date = start + Duration::days(i64::from(week) * 7 + i64::from(weekday));
            if date > today {
                spans.push(Span::raw("  "));
                continue;
            }
            let tokens = days.get(&date).map_or(0, |day| day.total_tokens);
            let mut style = Style::default().fg(HEAT[heat_level(tokens, &thresholds)]);
            if selected == Some(date) {
                style = style.bg(Color::Rgb(78, 78, 88));
            }
            spans.push(Span::styled("■", style));
            spans.push(Span::raw(" "));
            hits.push(
                Rect {
                    x: grid_x + week * 2,
                    y,
                    width: 2,
                    height: 1,
                },
                Hit::HeatDay(date),
            );
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
    }
}

fn draw_day_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &UsageStatsReport,
    selected: Option<NaiveDate>,
    i18n: &I18n,
) {
    let date = selected.map(|date| date.format("%Y-%m-%d").to_string());
    let day = date
        .as_ref()
        .and_then(|date| report.calendar.iter().find(|day| &day.date == date));
    let line = match (date, day) {
        (Some(date), Some(day)) => {
            let mut spans = vec![
                Span::styled(
                    date,
                    Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} {}  ·  {} {}",
                        compact(day.total_tokens),
                        i18n.text("stats_tokens"),
                        day.request_count,
                        i18n.text("stats_requests_unit")
                    ),
                    Style::default().fg(WHITE),
                ),
            ];
            if !day.cost.is_empty() {
                spans.push(Span::styled(
                    format!("  ·  ≈{}", format_cost(&day.cost)),
                    Style::default().fg(RED),
                ));
            }
            Line::from(spans)
        }
        (Some(date), None) => Line::from(Span::styled(date, Style::default().fg(MUTED))),
        (None, _) => Line::from(Span::styled(
            i18n.text("stats_day_hint"),
            Style::default().fg(MUTED),
        )),
    };
    frame.render_widget(Paragraph::new(line), Rect { height: 1, ..area });
}

#[allow(clippy::too_many_lines)]
fn draw_summary(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    report: &UsageStatsReport,
    i18n: &I18n,
) {
    let scroll = screen.scroll;
    let tokens = &report.summary;
    let overview = &report.overview;
    let none = || "—".to_owned();
    let days = |count: u64| format!("{count} {}", i18n.text("stats_days_unit"));
    let cells = [
        (
            i18n.text("stats_favorite_model"),
            overview.favorite_model.clone().unwrap_or_else(none),
        ),
        (
            i18n.text("stats_total_tokens"),
            compact(tokens.total_tokens()),
        ),
        (i18n.text("stats_active_days"), days(overview.active_days)),
        (
            i18n.text("stats_most_active_day"),
            overview.most_active_day.as_ref().map_or_else(none, |day| {
                format!("{} · {}", day.date, compact(day.tokens.total_tokens()))
            }),
        ),
        (
            i18n.text("stats_current_streak"),
            days(overview.current_streak),
        ),
        (
            i18n.text("stats_longest_streak"),
            days(overview.longest_streak),
        ),
        (
            i18n.text("stats_peak_hour"),
            overview
                .peak_hour
                .map_or_else(none, |hour| format!("{hour:02}:00")),
        ),
        (
            i18n.text("stats_hit_rate"),
            format!("{:.1}%", tokens.cache_hit_rate() * 100.0),
        ),
        (
            i18n.text("stats_requests"),
            tokens.request_count.to_string(),
        ),
        (
            i18n.text("stats_cost"),
            if report.cost.is_empty() {
                none()
            } else {
                format!("≈{}", format_cost(&report.cost))
            },
        ),
    ];
    let cell = |label: &str, value: &str| {
        vec![
            Span::styled(format!("{label}  "), Style::default().fg(MUTED)),
            Span::styled(
                value.to_owned(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]
    };
    let mut lines = Vec::new();
    if area.width >= 60 {
        let column = usize::from(area.width / 2);
        for pair in cells.chunks(2) {
            let mut spans = cell(pair[0].0, &pair[0].1);
            let used = display_width(pair[0].0) + 2 + display_width(&pair[0].1);
            spans.push(Span::raw(" ".repeat(column.saturating_sub(used).max(2))));
            if let Some((label, value)) = pair.get(1) {
                spans.extend(cell(label, value));
            }
            lines.push(Line::from(spans));
        }
    } else {
        lines.extend(
            cells
                .iter()
                .map(|(label, value)| Line::from(cell(label, value))),
        );
    }
    if report.unpriced_tokens > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} {}",
                i18n.text("stats_unpriced"),
                compact(report.unpriced_tokens)
            ),
            Style::default().fg(MUTED),
        )));
    }
    // An account label only helps once there is more than one account to tell apart.
    let accounts = report
        .quota
        .iter()
        .filter_map(|estimate| estimate.account.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    let (quota, hidden) =
        visible_quota(report, &screen.quota_filter, chrono::Utc::now().timestamp());
    for estimate in quota {
        lines.push(Line::from(""));
        lines.extend(quota_lines(estimate, accounts.len() > 1, i18n));
    }
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            i18n.text("stats_quota_hidden")
                .replace("{count}", &hidden.to_string()),
            Style::default().fg(MUTED),
        )));
    }
    let mut details = vec![
        Line::from(""),
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
    details.extend(report.providers.iter().take(4).map(|provider| {
        let cost = if provider.cost.is_empty() {
            String::new()
        } else {
            format!("  ≈{}", format_cost(&provider.cost))
        };
        Line::from(format!(
            "{}{}  {}{cost}",
            if provider.inferred { "~" } else { "" },
            provider.provider_name,
            compact(provider.tokens.total_tokens())
        ))
    }));
    // A narrow terminal leads with the token breakdown, which the grid only summarizes.
    if area.width < 60 {
        details.remove(0);
        details.insert(
            0,
            Line::from(vec![
                metric(i18n.text("stats_total_tokens"), tokens.total_tokens()),
                Span::styled(
                    format!(
                        "   {} {:.1}%",
                        i18n.text("stats_hit_rate"),
                        tokens.cache_hit_rate() * 100.0
                    ),
                    Style::default().fg(WHITE),
                ),
            ]),
        );
        details.push(Line::from(""));
        details.append(&mut lines);
        lines = details;
    } else {
        lines.append(&mut details);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0)),
        area,
    );
}

fn metric(label: &str, value: u64) -> Span<'static> {
    Span::styled(
        format!("{label} {}", compact(value)),
        Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
    )
}

/// A subscription window and the allowance back-calculated for it.
#[allow(clippy::too_many_lines)]
fn quota_lines(
    estimate: &UsageQuotaEstimate,
    show_account: bool,
    i18n: &I18n,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let bold = Style::default().fg(WHITE).add_modifier(Modifier::BOLD);
    let window = quota_window_label(estimate.window_minutes, i18n);
    let resets =
        chrono::DateTime::from_timestamp(estimate.resets_at, 0).map_or_else(String::new, |value| {
            value
                .with_timezone(&Local)
                .format("%m-%d %H:%M")
                .to_string()
        });
    let mut header = vec![Span::styled(i18n.text("stats_quota").to_owned(), bold)];
    if let Some(plan) = &estimate.plan_type {
        header.push(Span::styled(format!(" · {plan}"), bold));
    }
    if let Some(source) = &estimate.source {
        header.push(Span::styled(format!(" · {source}"), muted));
    }
    if show_account && let Some(account) = &estimate.account {
        header.push(Span::styled(
            format!(" · {} {account}", i18n.text("stats_quota_account")),
            muted,
        ));
    }
    header.push(Span::styled(
        if estimate.current {
            format!(
                "  {window} · {:.0}% {} · {} {resets}",
                estimate.used_percent,
                i18n.text("stats_quota_used"),
                i18n.text("stats_quota_resets"),
            )
        } else {
            format!("  {window} · {}", i18n.text("stats_quota_past"))
        },
        muted,
    ));
    let mut lines = vec![Line::from(header)];
    let Some(capacity) = &estimate.capacity else {
        lines.push(Line::from(Span::styled(
            i18n.text("stats_quota_pending").to_owned(),
            muted,
        )));
        return lines;
    };
    let amount = |tokens: u64, cost: &[hsin_core::UsageCost]| {
        if cost.is_empty() {
            format!("≈{}", compact(tokens))
        } else {
            format!("≈{} · ≈{}", compact(tokens), format_cost(cost))
        }
    };
    let range = format!(
        "  ({}–{})",
        compact(capacity.tokens_low),
        capacity.tokens_high.map_or_else(|| "?".to_owned(), compact)
    );
    let label = |key: &str| Span::styled(format!("{}  ", i18n.text(key)), muted);
    lines.push(Line::from(vec![
        label("stats_quota_capacity"),
        Span::styled(amount(capacity.tokens, &capacity.cost), bold),
        Span::styled(range, muted),
    ]));
    if let Some(remaining) = &estimate.remaining {
        lines.push(Line::from(vec![
            label("stats_quota_remaining"),
            Span::styled(amount(remaining.tokens, &remaining.cost), bold),
        ]));
    }
    if let Some(monthly) = &estimate.monthly {
        lines.push(Line::from(vec![
            label("stats_quota_monthly"),
            Span::styled(
                amount(monthly.tokens, &monthly.cost),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    let shown = if estimate.current { 4 } else { 2 };
    for cycle in estimate.cycles.iter().take(shown) {
        let time = |at: i64| {
            chrono::DateTime::from_timestamp(at, 0).map_or_else(String::new, |value| {
                value
                    .with_timezone(&Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
        };
        let cost = if cycle.cost.is_empty() {
            String::new()
        } else {
            format!(" · ≈{}", format_cost(&cycle.cost))
        };
        let text = format!(
            "  {} → {}  {:.0}→{:.0}%  ≈{}{cost}",
            time(cycle.first_at),
            time(cycle.last_at),
            cycle.from_percent,
            cycle.to_percent,
            compact(cycle.tokens)
        );
        lines.push(Line::from(Span::styled(text, muted)));
    }
    lines.push(Line::from(Span::styled(
        i18n.text("stats_quota_note")
            .replace("{percent}", &format!("{:.0}", estimate.basis_percent))
            .replace("{cycles}", &estimate.basis_cycles.to_string()),
        muted,
    )));
    lines
}

fn quota_window_label(minutes: u32, i18n: &I18n) -> String {
    if minutes == 7 * 24 * 60 {
        i18n.text("stats_quota_weekly").to_owned()
    } else if minutes.is_multiple_of(60) {
        i18n.text("stats_quota_hours")
            .replace("{hours}", &(minutes / 60).to_string())
    } else {
        format!("{minutes} min")
    }
}

#[allow(clippy::too_many_lines)]
fn draw_models(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &UsageStatsReport,
    scroll: u16,
    style: StatsChartStyle,
    pointer: Option<Position>,
    i18n: &I18n,
) -> Option<Tooltip> {
    if report.models.is_empty() {
        frame.render_widget(
            Paragraph::new(i18n.text("stats_no_usage")).style(Style::default().fg(MUTED)),
            area,
        );
        return None;
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
        return None;
    }
    let chart_count = report.models.len().min(4);
    // The charts take what the model list below them leaves: a line per charted model, one for
    // the rest, and a blank line.
    let list_rows = u16::try_from(chart_count).unwrap_or(4) + u16::from(report.models.len() > 4);
    let chart_height = area.height.saturating_sub(list_rows + 1).max(6);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(chart_height), Constraint::Min(4)])
        .split(area);
    frame.render_widget(
        Paragraph::new(legend(i18n)),
        Rect {
            height: 1,
            ..rows[0]
        },
    );
    let charts = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            (0..chart_count).map(|_| Constraint::Ratio(1, u32::try_from(chart_count).unwrap_or(1))),
        )
        .split(Rect {
            y: rows[0].y + 1,
            height: rows[0].height.saturating_sub(1),
            ..rows[0]
        });
    let mut tooltip = None;
    for (model, chart_area) in report.models.iter().take(4).zip(charts.iter()) {
        let cost = if model.cost.is_empty() {
            String::new()
        } else {
            format!(" · ≈{}", format_cost(&model.cost))
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    model.model.clone(),
                    Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · {}{cost}", compact(model.tokens.total_tokens())),
                    Style::default().fg(MUTED),
                ),
            ])),
            Rect {
                height: 1,
                ..*chart_area
            },
        );
        let points = model
            .daily
            .iter()
            .map(|bucket| split(&bucket.tokens))
            .collect::<Vec<_>>();
        let dates = model
            .daily
            .iter()
            .map(|bucket| bucket.date.get(5..).unwrap_or(&bucket.date).to_owned())
            .collect::<Vec<_>>();
        tooltip = draw_series(
            frame,
            Rect {
                y: chart_area.y + 1,
                height: chart_area.height.saturating_sub(1),
                ..*chart_area
            },
            &points,
            &dates,
            style,
            pointer,
        )
        .or(tooltip);
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
    tooltip
}

#[allow(clippy::too_many_lines)]
fn draw_filter(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &StatsScreen,
    filter: &StatsFilter,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    let custom =
        matches!(filter, StatsFilter::Time { selected, .. } if *selected == STATS_TIME_CUSTOM);
    let height = match filter {
        StatsFilter::Time { .. } => {
            u16::try_from(TIME_LABELS.len()).unwrap_or(6) + 2 + if custom { 4 } else { 0 }
        }
        _ => 10,
    };
    let popup = centered_fixed(area, 58, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .title(match filter {
                StatsFilter::Time { .. } => i18n.text("stats_time_range"),
                StatsFilter::Provider { .. } => i18n.text("stats_provider"),
                StatsFilter::Model { .. } => i18n.text("stats_model"),
                StatsFilter::Quota { .. } => i18n.text("stats_quota_filter"),
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
    let mut items = Vec::new();
    match filter {
        StatsFilter::Time { .. } => {
            items.extend(TIME_LABELS.iter().map(|key| ListItem::new(i18n.text(key))));
        }
        StatsFilter::Provider { .. } => {
            items.push(ListItem::new(i18n.text("stats_all")));
            if let Some(report) = &screen.report {
                items.extend(report.filters.providers.iter().map(|provider| {
                    ListItem::new(format!(
                        "{}{}",
                        if provider.inferred { "~" } else { "" },
                        provider.name
                    ))
                }));
            }
        }
        StatsFilter::Model { .. } => {
            items.push(ListItem::new(i18n.text("stats_all")));
            if let Some(report) = &screen.report {
                items.extend(report.filters.models.iter().cloned().map(ListItem::new));
            }
        }
        StatsFilter::Quota { .. } => {
            items.push(ListItem::new(i18n.text("stats_quota_recent")));
            items.push(ListItem::new(i18n.text("stats_quota_all")));
            if let Some(report) = &screen.report {
                items.extend(
                    quota_plans(report)
                        .into_iter()
                        .map(|(_, label)| ListItem::new(label)),
                );
            }
        }
    }
    let selected = match filter {
        StatsFilter::Time { selected, .. }
        | StatsFilter::Provider { selected }
        | StatsFilter::Model { selected }
        | StatsFilter::Quota { selected } => *selected,
    };
    let item_count = items.len();
    let list_area = Rect {
        height: if custom {
            u16::try_from(item_count)
                .unwrap_or(u16::MAX)
                .min(inner.height)
        } else {
            inner.height
        },
        ..inner
    };
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::default().fg(WHITE).bg(RED)),
        list_area,
        &mut state,
    );
    hits.list(list_area, &state, (0..item_count).map(|_| 1), ENTER);
    if let StatsFilter::Time {
        custom_field,
        cursor,
        ..
    } = filter
        && custom
    {
        let fields = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(Rect {
                y: list_area.bottom() + 1,
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

fn calendar_days(calendar: &[UsageCalendarDay]) -> BTreeMap<NaiveDate, &UsageCalendarDay> {
    calendar
        .iter()
        .filter_map(|day| {
            NaiveDate::parse_from_str(&day.date, "%Y-%m-%d")
                .ok()
                .map(|date| (date, day))
        })
        .collect()
}

/// Quartiles of the active days, so the colours follow the user's own spread rather than one
/// outlier day.
fn heat_thresholds(values: impl Iterator<Item = u64>) -> [u64; 3] {
    let mut active = values.filter(|value| *value > 0).collect::<Vec<_>>();
    if active.is_empty() {
        return [0; 3];
    }
    active.sort_unstable();
    let last = active.len() - 1;
    [1, 2, 3].map(|quarter| active[last * quarter / 4])
}

fn heat_level(tokens: u64, thresholds: &[u64; 3]) -> usize {
    if tokens == 0 {
        0
    } else {
        1 + thresholds
            .iter()
            .filter(|threshold| tokens > **threshold)
            .count()
    }
}

fn display_width_u16(value: &str) -> u16 {
    u16::try_from(display_width(value)).unwrap_or(u16::MAX)
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

fn model_line(model: &hsin_core::UsageModelBreakdown, i18n: &I18n) -> Line<'static> {
    let cost = if model.cost.is_empty() {
        String::new()
    } else {
        format!(" · ≈{}", format_cost(&model.cost))
    };
    Line::from(format!(
        "{}  {} · {} {} · {} {} · {} {}{cost}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_levels_follow_quartiles_of_active_days() {
        let thresholds = heat_thresholds([0, 10, 20, 30, 40, 1_000].into_iter());
        assert_eq!(thresholds, [20, 30, 40]);
        assert_eq!(heat_level(0, &thresholds), 0);
        assert_eq!(heat_level(10, &thresholds), 1);
        assert_eq!(heat_level(25, &thresholds), 2);
        assert_eq!(heat_level(1_000, &thresholds), 4);
        assert_eq!(heat_thresholds([0, 0].into_iter()), [0; 3]);
    }
}
