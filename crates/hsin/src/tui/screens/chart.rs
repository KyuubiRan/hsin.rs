//! Token series drawn either as bars stacked by kind of token or as one line per kind, with a
//! tooltip for the point under the mouse pointer.

use std::ops::Range;

use hsin_core::{StatsChartStyle, UsageTokenSummary};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Clear, Dataset, GraphType, Padding, Paragraph},
};

use crate::{i18n::I18n, usage_format::compact_tokens};

use super::super::{
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::display_width,
};

/// Cache-hit input, other input, and output, from darkest to brightest: the cheapest tokens are
/// the most numerous, so they get the quietest colour.
pub(super) const SHADES: [Color; 3] = [
    Color::Rgb(92, 36, 44),
    Color::Rgb(160, 50, 60),
    Color::Rgb(242, 128, 138),
];

const LABELS: [&str; 3] = ["stats_chart_hit", "stats_chart_miss", "stats_chart_output"];

/// Marks the bar or point under the pointer.
const HOVER: Color = Color::Rgb(58, 58, 68);

const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// One point of a series: cache-hit input, other input, output.
pub(super) fn split(tokens: &UsageTokenSummary) -> [u64; 3] {
    [
        tokens.cache_read_tokens,
        tokens.non_hit_tokens(),
        tokens.output_tokens,
    ]
}

pub(super) fn legend(i18n: &I18n) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, color) in LABELS.into_iter().zip(SHADES) {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled("■ ", Style::default().fg(color)));
        spans.push(Span::styled(
            i18n.text(key).to_owned(),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

/// The values under the pointer. `draw_tooltip` draws it once every chart of the screen is down,
/// so a neighbouring chart cannot paint over the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Tooltip {
    anchor: Position,
    label: String,
    values: [u64; 3],
}

/// Draws `points`, named one to one by `labels`, and returns the tooltip for the point under
/// `pointer`, if any.
pub(super) fn draw_series(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[[u64; 3]],
    labels: &[String],
    style: StatsChartStyle,
    pointer: Option<Position>,
) -> Option<Tooltip> {
    if area.width == 0 || area.height == 0 || points.is_empty() {
        return None;
    }
    let pointer = pointer.filter(|pointer| area.contains(*pointer));
    let hovered = match style {
        StatsChartStyle::Bar => draw_bars(frame, area, points, pointer),
        StatsChartStyle::Line => draw_lines(frame, area, points, labels, pointer),
    }?;
    let values = points[hovered.clone()]
        .iter()
        .fold([0; 3], |sum, point| add(sum, *point));
    let first = labels.get(hovered.start).cloned().unwrap_or_default();
    let label = match labels.get(hovered.end - 1) {
        Some(last) if hovered.len() > 1 => format!("{first} – {last}"),
        _ => first,
    };
    Some(Tooltip {
        anchor: pointer?,
        label,
        values,
    })
}

/// A small box beside the pointer with the three values and their total. It sits below and to
/// the right of the pointer, and flips to the other side where the screen edge would cut it off.
pub(super) fn draw_tooltip(frame: &mut Frame<'_>, tooltip: &Tooltip, i18n: &I18n) {
    let total = tooltip.values.iter().sum::<u64>();
    let rows = LABELS
        .iter()
        .map(|key| i18n.text(key))
        .zip(tooltip.values)
        .chain(std::iter::once((i18n.text("stats_total_tokens"), total)))
        .map(|(name, value)| (name, compact_tokens(value)))
        .collect::<Vec<_>>();
    let name_width = rows
        .iter()
        .map(|(name, _)| display_width(name))
        .max()
        .unwrap_or(0);
    let value_width = rows
        .iter()
        .map(|(_, value)| display_width(value))
        .max()
        .unwrap_or(0);
    // Swatch, name, a gap of two, then the value.
    let content = (2 + name_width + 2 + value_width).max(display_width(&tooltip.label));
    let bounds = frame.area();
    let width = u16::try_from(content + 4)
        .unwrap_or(u16::MAX)
        .min(bounds.width);
    let height = u16::try_from(rows.len() + 3)
        .unwrap_or(u16::MAX)
        .min(bounds.height);
    let anchor = tooltip.anchor;
    let x = if anchor.x.saturating_add(2).saturating_add(width) <= bounds.right() {
        anchor.x + 2
    } else {
        anchor.x.saturating_sub(width + 1).max(bounds.x)
    };
    let y = if anchor.y.saturating_add(1).saturating_add(height) <= bounds.bottom() {
        anchor.y + 1
    } else {
        anchor
            .y
            .saturating_sub(height)
            .max(bounds.y)
            .min(bounds.bottom().saturating_sub(height))
    };
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    let muted = Style::default().fg(MUTED);
    let white = Style::default().fg(WHITE);
    let mut lines = vec![Line::from(Span::styled(
        tooltip.label.clone(),
        white.add_modifier(Modifier::BOLD),
    ))];
    for (index, (name, value)) in rows.into_iter().enumerate() {
        let swatch = SHADES.get(index).map_or_else(
            || Span::raw("  "),
            |color| Span::styled("■ ", Style::default().fg(*color)),
        );
        let padding = name_width - display_width(name) + 2 + value_width - display_width(&value);
        // The last row is the total.
        let value_style = if index == LABELS.len() {
            white.add_modifier(Modifier::BOLD)
        } else {
            white
        };
        lines.push(Line::from(vec![
            swatch,
            Span::styled(name.to_owned(), muted),
            Span::raw(" ".repeat(padding)),
            Span::styled(value, value_style),
        ]));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(muted)
                .style(Style::default().bg(INPUT_BG))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// Bars stacked bottom to top as cache-hit input, other input, output, at an eighth of a cell.
/// A part that is not zero always gets at least one eighth, so a thin slice of output stays
/// visible above a tall column of cached input; a cell shared by several parts takes the colour
/// of the topmost one. Returns the points behind the bar under the pointer.
fn draw_bars(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[[u64; 3]],
    pointer: Option<Position>,
) -> Option<Range<usize>> {
    let per_column = points_per_column(points.len(), usize::from(area.width));
    let columns = merge_columns(points, per_column);
    let width = (usize::from(area.width) / columns.len()).max(1);
    let hovered = pointer
        .map(|pointer| usize::from(pointer.x - area.x) / width)
        .filter(|column| *column < columns.len());
    let max = columns
        .iter()
        .map(|column| column.iter().sum::<u64>())
        .max()
        .unwrap_or(0);
    let units = u64::from(area.height) * 8;
    let buffer = frame.buffer_mut();
    for (index, column) in columns.iter().enumerate() {
        let bounds = stack(*column, max, units);
        let top = bounds[2];
        for row in 0..area.height {
            let low = u64::from(row) * 8;
            let filled = top.saturating_sub(low).min(8);
            let cell = (filled > 0).then(|| {
                let cell_top = low + filled;
                let color = (0..3)
                    .rev()
                    .find(|part| {
                        let start = if *part == 0 { 0 } else { bounds[part - 1] };
                        bounds[*part] > start && start < cell_top && bounds[*part] > low
                    })
                    .map_or(SHADES[0], |part| SHADES[part]);
                (EIGHTHS[usize::try_from(filled).unwrap_or(8)], color)
            });
            // The hovered bar is marked up to the top of the chart.
            if cell.is_none() && hovered != Some(index) {
                break;
            }
            let y = area.bottom() - 1 - row;
            for offset in 0..width {
                let x = area.x + u16::try_from(index * width + offset).unwrap_or(u16::MAX);
                if x >= area.right() {
                    break;
                }
                // Leave a gap between wide columns so neighbours stay apart.
                if width > 2 && offset == width - 1 {
                    continue;
                }
                let target = &mut buffer[(x, y)];
                if let Some((symbol, color)) = cell {
                    target.set_symbol(symbol).set_fg(color);
                }
                if hovered == Some(index) {
                    target.set_bg(HOVER);
                }
            }
        }
    }
    hovered.map(|column| column * per_column..((column + 1) * per_column).min(points.len()))
}

/// The top of each part of a stacked bar, in eighths of a cell out of `units`.
fn stack(column: [u64; 3], max: u64, units: u64) -> [u64; 3] {
    let mut bounds = [0; 3];
    if max == 0 {
        return bounds;
    }
    let mut top = 0;
    for (part, value) in column.into_iter().enumerate() {
        let mut size = value * units / max;
        if value > 0 && size == 0 {
            size = 1;
        }
        top = (top + size).min(units);
        bounds[part] = top;
    }
    bounds
}

fn points_per_column(points: usize, width: usize) -> usize {
    points.div_ceil(width.max(1)).max(1)
}

/// Merges runs of `per_column` neighbouring points into one column.
fn merge_columns(points: &[[u64; 3]], per_column: usize) -> Vec<[u64; 3]> {
    points
        .chunks(per_column.max(1))
        .map(|chunk| chunk.iter().fold([0; 3], |sum, point| add(sum, *point)))
        .collect()
}

fn add(sum: [u64; 3], point: [u64; 3]) -> [u64; 3] {
    [sum[0] + point[0], sum[1] + point[1], sum[2] + point[2]]
}

/// One line per kind of token on a logarithmic scale: cache hits outnumber output a hundredfold
/// or more, and a linear scale would flatten everything but them. The axes are drawn here rather
/// than by `Chart`, so the plot's cells are known exactly and the pointer maps onto a point.
/// Returns the point under the pointer.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn draw_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[[u64; 3]],
    labels: &[String],
    pointer: Option<Position>,
) -> Option<Range<usize>> {
    let scale = |value: u64| ((value as f64) + 1.0).log10();
    let series = (0..3)
        .map(|part| {
            points
                .iter()
                .enumerate()
                .map(|(index, point)| (index as f64, scale(point[part])))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let top = points
        .iter()
        .flat_map(|point| point.iter().copied())
        .max()
        .unwrap_or(0);
    let y_max = scale(top).ceil().max(1.0);
    let plot = if area.height >= 5 {
        let top_label = format!("{} (log)", compact_tokens(10_u64.pow(y_max as u32)));
        draw_axes(
            frame.buffer_mut(),
            area,
            &top_label,
            labels,
            Style::default().fg(MUTED),
        )
    } else {
        area
    };
    if plot.width < 2 || plot.height == 0 {
        return None;
    }
    let last = points.len() - 1;
    // Braille gives each cell two dots across, and `Chart` spreads the points over all of them.
    let dots = f64::from(plot.width) * 2.0 - 1.0;
    let hovered = pointer
        .filter(|pointer| plot.contains(*pointer))
        .map(|pointer| {
            let dot = f64::from(pointer.x - plot.x) * 2.0 + 0.5;
            ((dot * last as f64 / dots).round() as usize).min(last)
        });
    let datasets = series
        .iter()
        .zip(SHADES)
        .map(|(data, color)| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(data)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, last.max(1) as f64]))
            .y_axis(Axis::default().bounds([0.0, y_max]))
            .legend_position(None)
            .style(Style::default().fg(RED)),
        plot,
    );
    // A guide through the hovered point, behind the lines: only blank cells take it.
    if let Some(index) = hovered {
        let column = if last == 0 {
            0
        } else {
            (index as f64 * dots / last as f64 / 2.0) as u16
        };
        let x = plot.x + column.min(plot.width - 1);
        let buffer = frame.buffer_mut();
        for y in plot.top()..plot.bottom() {
            let cell = &mut buffer[(x, y)];
            if cell.symbol() == " " {
                cell.set_symbol("│").set_fg(HOVER);
            }
        }
    }
    hovered.map(|index| index..index + 1)
}

/// Draws the y labels, the two axis lines, and the first and last x labels, and returns the plot
/// area they enclose.
fn draw_axes(
    buffer: &mut Buffer,
    area: Rect,
    top_label: &str,
    labels: &[String],
    style: Style,
) -> Rect {
    let gutter = u16::try_from(display_width(top_label).max(1)).unwrap_or(u16::MAX);
    if gutter + 3 > area.width {
        return area;
    }
    let axis_x = area.x + gutter;
    let axis_y = area.bottom() - 2;
    buffer.set_string(area.x, area.y, top_label, style);
    buffer.set_string(axis_x - 1, axis_y - 1, "0", style);
    for y in area.y..axis_y {
        buffer[(axis_x, y)].set_symbol("│").set_style(style);
    }
    buffer[(axis_x, axis_y)].set_symbol("└").set_style(style);
    for x in axis_x + 1..area.right() {
        buffer[(x, axis_y)].set_symbol("─").set_style(style);
    }
    if let (Some(first), Some(last)) = (labels.first(), labels.last()) {
        let label_y = area.bottom() - 1;
        buffer.set_string(axis_x + 1, label_y, first, style);
        let width = u16::try_from(display_width(last)).unwrap_or(u16::MAX);
        let first_end = axis_x + 1 + u16::try_from(display_width(first)).unwrap_or(u16::MAX);
        if labels.len() > 1 && first_end < area.right().saturating_sub(width) {
            buffer.set_string(area.right() - width, label_y, last, style);
        }
    }
    Rect {
        x: axis_x + 1,
        y: area.y,
        width: area.right() - axis_x - 1,
        height: axis_y - area.y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(
        width: u16,
        height: u16,
        points: &[[u64; 3]],
        style: StatsChartStyle,
        pointer: Option<Position>,
    ) -> (Buffer, Option<Tooltip>) {
        let labels = (0..points.len())
            .map(|index| format!("{index:02}"))
            .collect::<Vec<_>>();
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut tooltip = None;
        terminal
            .draw(|frame| {
                tooltip = draw_series(frame, frame.area(), points, &labels, style, pointer);
            })
            .unwrap();
        (terminal.backend().buffer().clone(), tooltip)
    }

    #[test]
    fn long_series_merge_to_fit_the_width() {
        let points = vec![[1, 2, 3]; 10];
        let merged = merge_columns(&points, points_per_column(points.len(), 4));
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0], [3, 6, 9]);
        assert_eq!(merged.iter().map(|point| point[0]).sum::<u64>(), 10);
        assert_eq!(merge_columns(&points, points_per_column(10, 20)).len(), 10);
    }

    #[test]
    fn a_thin_output_slice_still_shows_on_top_of_a_bar() {
        let (buffer, _) = draw(4, 3, &[[1_000_000, 0, 1]], StatsChartStyle::Bar, None);
        assert_eq!(
            buffer[(0, 0)].fg,
            SHADES[2],
            "the output eighth colours the top cell"
        );
        assert_eq!(buffer[(0, 2)].fg, SHADES[0]);
        assert_eq!(buffer[(0, 2)].symbol(), "█");
    }

    #[test]
    fn hovering_a_bar_reports_its_three_values() {
        let points = [[10, 20, 30], [40, 50, 60], [70, 80, 90]];
        // Three points over six cells: each bar is two cells wide.
        let pointer = Position::new(3, 0);
        let (buffer, tooltip) = draw(6, 4, &points, StatsChartStyle::Bar, Some(pointer));
        let tooltip = tooltip.expect("tooltip");
        assert_eq!(tooltip.label, "01");
        assert_eq!(tooltip.values, [40, 50, 60]);
        assert_eq!(tooltip.anchor, pointer);
        assert_eq!(buffer[(2, 0)].bg, HOVER, "the hovered bar is marked");
        assert_ne!(buffer[(0, 0)].bg, HOVER);
    }

    #[test]
    fn hovering_a_merged_bar_sums_its_points_and_names_the_run() {
        let points = vec![[1, 2, 3]; 10];
        let (_, tooltip) = draw(
            4,
            3,
            &points,
            StatsChartStyle::Bar,
            Some(Position::new(0, 1)),
        );
        let tooltip = tooltip.expect("tooltip");
        assert_eq!(tooltip.label, "00 – 02");
        assert_eq!(tooltip.values, [3, 6, 9]);
    }

    #[test]
    fn hovering_a_line_picks_the_nearest_point() {
        let points = (0..24)
            .map(|hour| [hour, hour * 2, hour * 3])
            .collect::<Vec<_>>();
        let (_, end) = draw(
            40,
            8,
            &points,
            StatsChartStyle::Line,
            Some(Position::new(39, 1)),
        );
        assert_eq!(end.expect("tooltip").values, [23, 46, 69]);
        let (_, labels) = draw(
            40,
            8,
            &points,
            StatsChartStyle::Line,
            Some(Position::new(0, 7)),
        );
        assert_eq!(labels, None, "the axis labels are not part of the plot");
        // The gutter holds "100 (log)", so the plot starts after it and its axis line.
        let (buffer, start) = draw(
            40,
            8,
            &points,
            StatsChartStyle::Line,
            Some(Position::new(10, 2)),
        );
        assert_eq!(start.expect("tooltip").label, "00");
        assert_eq!(buffer[(10, 0)].fg, HOVER, "a guide marks the hovered point");
    }

    #[test]
    fn no_pointer_shows_nothing() {
        let (_, tooltip) = draw(6, 4, &[[1, 1, 1]], StatsChartStyle::Bar, None);
        assert_eq!(tooltip, None);
    }

    #[test]
    fn the_tooltip_flips_left_at_the_right_edge() {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let i18n = I18n::new(Some(hsin_core::LANGUAGE_EN_US));
        let tooltip = Tooltip {
            anchor: Position::new(58, 2),
            label: "09-27".to_owned(),
            values: [1_000, 2_000, 3_000],
        };
        terminal
            .draw(|frame| draw_tooltip(frame, &tooltip, &i18n))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..12)
            .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let output = text
            .iter()
            .find(|line| line.contains("Output"))
            .expect("output row");
        assert!(output.contains("3.0k"), "{output}");
        assert!(text.iter().any(|line| line.contains("6.0k")), "the total");
        assert!(text.iter().any(|line| line.contains("09-27")));
        let right = text[4]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| *c == '│')
            .expect("border");
        assert!(right < 58, "the box sits left of the pointer: {text:#?}");
    }
}
