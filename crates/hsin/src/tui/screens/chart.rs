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
        StatsChartStyle::Bar => draw_bars(frame, area, points, labels, pointer),
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
/// of the topmost one. The y axis runs linearly from zero to a round value at or above the
/// tallest bar. Returns the points behind the bar under the pointer.
fn draw_bars(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[[u64; 3]],
    labels: &[String],
    pointer: Option<Position>,
) -> Option<Range<usize>> {
    // Tick labels decide the gutter, the gutter decides how many points share a column, and that
    // decides the tallest bar and so the ticks; a second pass settles it.
    let mut axes = Axes::new(area, 0);
    let mut scale = BarScale::new(points, axes.map_or(area, |axes| axes.plot), axes.is_some());
    for _ in 0..2 {
        let gutter = scale.gutter();
        if axes.is_none_or(|axes| axes.gutter == gutter) {
            break;
        }
        axes = Axes::new(area, gutter);
        scale = BarScale::new(points, axes.map_or(area, |axes| axes.plot), axes.is_some());
    }
    let plot = axes.map_or(area, |axes| axes.plot);
    let BarScale {
        per_column,
        columns,
        top,
        ..
    } = &scale;
    // Columns spread over the whole plot, so some are a cell wider than others; every bar is
    // drawn equally wide, leaving a gap between wide columns so neighbours stay apart.
    let count = columns.len();
    let start = |column: usize| column * usize::from(plot.width) / count;
    let width = (usize::from(plot.width) / count).max(1);
    let drawn = if width > 2 { width - 1 } else { width };
    if let Some(axes) = axes {
        let x_ticks = spaced_ticks(
            (0..columns.len())
                .map(|column| {
                    (
                        u16::try_from(start(column) + (drawn - 1) / 2).unwrap_or(u16::MAX),
                        labels.get(column * per_column).cloned().unwrap_or_default(),
                    )
                })
                .collect(),
            plot.width,
        );
        let y_ticks = scale
            .ticks
            .iter()
            .map(|(value, label)| (bar_row(*value, *top, plot.height), label.clone()))
            .collect::<Vec<_>>();
        axes.draw(frame.buffer_mut(), &y_ticks, &x_ticks, None);
    }
    let hovered = pointer
        .filter(|pointer| plot.contains(*pointer))
        .and_then(|pointer| {
            let x = usize::from(pointer.x - plot.x);
            (0..count).find(|column| start(column + 1) > x)
        });
    let units = u64::from(plot.height) * 8;
    let buffer = frame.buffer_mut();
    for (index, column) in columns.iter().enumerate() {
        let bounds = stack(*column, *top, units);
        let top = bounds[2];
        for row in 0..plot.height {
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
            let y = plot.bottom() - 1 - row;
            for offset in 0..drawn {
                let x = plot.x + u16::try_from(start(index) + offset).unwrap_or(u16::MAX);
                if x >= plot.right() {
                    break;
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

/// How a bar chart fits a plot: the merged columns, the value at the top of the y axis, and the
/// ticks up to it. Without rulers the tallest bar fills the plot; with them the top is rounded up
/// to the last tick.
struct BarScale {
    per_column: usize,
    columns: Vec<[u64; 3]>,
    top: u64,
    ticks: Vec<(u64, String)>,
}

impl BarScale {
    fn new(points: &[[u64; 3]], plot: Rect, ruled: bool) -> Self {
        let per_column = points_per_column(points.len(), usize::from(plot.width));
        let columns = merge_columns(points, per_column);
        let max = columns
            .iter()
            .map(|column| column.iter().sum::<u64>())
            .max()
            .unwrap_or(0);
        if !ruled {
            return Self {
                per_column,
                columns,
                top: max.max(1),
                ticks: Vec::new(),
            };
        }
        // A tick every other row at most and no more than five steps up, choosing the count of
        // steps that puts the top closest to the tallest bar.
        let (step, top) = (1..=u64::from(plot.height / 2).clamp(1, 5))
            .map(|intervals| {
                let step = nice_step(max, intervals);
                (step, max.div_ceil(step).max(1) * step)
            })
            .min_by_key(|(step, top)| (*top, std::cmp::Reverse(top / step)))
            .unwrap_or((1, 1));
        let ticks = (0..=top / step)
            .map(|index| (index * step, tick_label(index * step)))
            .collect();
        Self {
            per_column,
            columns,
            top,
            ticks,
        }
    }

    fn gutter(&self) -> u16 {
        label_gutter(self.ticks.iter().map(|(_, label)| label.as_str()))
    }
}

/// The row, from the top of a plot `height` cells tall, where a bar of `value` out of `top` ends.
fn bar_row(value: u64, top: u64, height: u16) -> u16 {
    let eighths = value * u64::from(height) * 8 / top.max(1);
    let from_bottom = u16::try_from(eighths.saturating_sub(1) / 8).unwrap_or(u16::MAX);
    height - 1 - from_bottom.min(height - 1)
}

/// The smallest step of a round multiple of a power of ten that covers `max` in `intervals`
/// steps.
fn nice_step(max: u64, intervals: u64) -> u64 {
    let raw = max.div_ceil(intervals.max(1)).max(1);
    let mut magnitude = 1_u64;
    while magnitude.saturating_mul(10) <= raw {
        magnitude *= 10;
    }
    [1, 2, 3, 4, 5, 6, 8, 10]
        .into_iter()
        .map(|factor| factor * magnitude)
        .find(|step| *step >= raw)
        .unwrap_or(10 * magnitude)
}

/// A compact tick label without a redundant `.0`: `500k` rather than `500.0k`.
fn tick_label(value: u64) -> String {
    let label = compact_tokens(value);
    match label.find(".0") {
        Some(index) if index + 3 == label.len() => {
            format!("{}{}", &label[..index], &label[index + 2..])
        }
        _ => label,
    }
}

fn label_gutter<'a>(labels: impl Iterator<Item = &'a str>) -> u16 {
    u16::try_from(labels.map(display_width).max().unwrap_or(1)).unwrap_or(u16::MAX)
}

fn points_per_column(points: usize, width: usize) -> usize {
    points.div_ceil(width.max(1)).max(1)
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
/// or more, and a linear scale would flatten everything but them. The y axis ticks powers of
/// ten. The axes are drawn here rather than by `Chart`, so the plot's cells are known exactly
/// and the pointer maps onto a point. Returns the point under the pointer.
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
    let highest = points
        .iter()
        .flat_map(|point| point.iter().copied())
        .max()
        .unwrap_or(0);
    let decades = (scale(highest).ceil() as u32).max(1);
    let decade_label = |decade: u32| {
        if decade == 0 {
            "0".to_owned()
        } else {
            tick_label(10_u64.saturating_pow(decade))
        }
    };
    // The gutter also carries the `log` marker under the y labels.
    let decade_labels = (0..=decades).map(decade_label).collect::<Vec<_>>();
    let gutter = label_gutter(decade_labels.iter().map(String::as_str).chain([LOG]));
    let axes = Axes::new(area, gutter);
    let plot = axes.map_or(area, |axes| axes.plot);
    if plot.width < 2 || plot.height == 0 {
        return None;
    }
    let last = points.len() - 1;
    // Braille gives each cell two dots across, and `Chart` spreads the points over all of them.
    let dots = f64::from(plot.width) * 2.0 - 1.0;
    let column_of = |index: usize| {
        if last == 0 {
            0
        } else {
            ((index as f64 * dots / last as f64 / 2.0) as u16).min(plot.width - 1)
        }
    };
    if let Some(axes) = axes {
        let y_ticks = log_ticks(decades, plot.height, &decade_labels);
        let x_ticks = spaced_ticks(
            (0..points.len())
                .map(|index| {
                    (
                        column_of(index),
                        labels.get(index).cloned().unwrap_or_default(),
                    )
                })
                .collect(),
            plot.width,
        );
        axes.draw(frame.buffer_mut(), &y_ticks, &x_ticks, Some(LOG));
    }
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
            .y_axis(Axis::default().bounds([0.0, f64::from(decades)]))
            .legend_position(None)
            .style(Style::default().fg(RED)),
        plot,
    );
    // A guide through the hovered point, behind the lines: only blank cells take it.
    if let Some(index) = hovered {
        let x = plot.x + column_of(index);
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

/// Powers of ten for a log y axis `decades` high on a plot `height` cells tall, as rows from its
/// top. Every `stride`-th decade down from the top keeps ticks at least two rows apart; zero
/// always closes the axis, taking the place of a tick that would crowd it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn log_ticks(decades: u32, height: u16, labels: &[String]) -> Vec<(u16, String)> {
    // Braille gives each cell four dots down, and `Chart` spreads the y range over all of them.
    let rows = f64::from(height) * 4.0 - 1.0;
    let row_of =
        |decade: u32| ((f64::from(decades - decade) * rows / f64::from(decades)) / 4.0) as u16;
    let stride = (2 * decades).div_ceil(u32::from(height)).max(1);
    let mut chosen = (0..=decades)
        .rev()
        .step_by(stride as usize)
        .collect::<Vec<_>>();
    if chosen.last() != Some(&0) {
        if chosen
            .last()
            .is_some_and(|decade| row_of(0) < row_of(*decade) + 2)
        {
            chosen.pop();
        }
        chosen.push(0);
    }
    let mut ticks: Vec<(u16, String)> = Vec::new();
    for decade in chosen {
        let row = row_of(decade);
        if ticks.last().is_none_or(|(previous, _)| *previous != row) {
            let label = labels.get(decade as usize).cloned().unwrap_or_default();
            ticks.push((row, label));
        }
    }
    ticks
}

/// Marks a logarithmic y axis.
const LOG: &str = "log";

/// The narrowest plot worth giving up room for axes.
const MIN_PLOT_WIDTH: u16 = 8;

/// Room around a plot for rulers: y labels in a gutter on the left and a y axis line, then below
/// the plot an x axis line with tick marks and a row of x labels. As the area shrinks the x axis
/// line goes first, then the rulers altogether.
#[derive(Debug, Clone, Copy)]
struct Axes {
    area: Rect,
    plot: Rect,
    gutter: u16,
    axis_line: bool,
}

impl Axes {
    fn new(area: Rect, gutter: u16) -> Option<Self> {
        if area.height < 3 || area.width < gutter + 1 + MIN_PLOT_WIDTH {
            return None;
        }
        let axis_line = area.height >= 6;
        Some(Self {
            area,
            plot: Rect {
                x: area.x + gutter + 1,
                y: area.y,
                width: area.width - gutter - 1,
                height: area.height - 1 - u16::from(axis_line),
            },
            gutter,
            axis_line,
        })
    }

    /// Draws the rulers. `y_ticks` hold a row from the top of the plot and `x_ticks` a column
    /// from its left edge; `corner` goes under the y labels.
    fn draw(
        &self,
        buffer: &mut Buffer,
        y_ticks: &[(u16, String)],
        x_ticks: &[(u16, String)],
        corner: Option<&str>,
    ) {
        let style = Style::default().fg(MUTED);
        let plot = self.plot;
        let axis_x = plot.x - 1;
        for y in plot.top()..plot.bottom() {
            buffer[(axis_x, y)].set_symbol("│").set_style(style);
        }
        for (row, label) in y_ticks {
            let y = plot.y + (*row).min(plot.height - 1);
            buffer[(axis_x, y)].set_symbol("┤").set_style(style);
            let width = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
            if width <= self.gutter {
                buffer.set_string(axis_x - width, y, label, style);
            }
        }
        if self.axis_line {
            let y = plot.bottom();
            buffer[(axis_x, y)].set_symbol("└").set_style(style);
            for x in plot.left()..plot.right() {
                buffer[(x, y)].set_symbol("─").set_style(style);
            }
            for (column, _) in x_ticks {
                buffer[(plot.x + column, y)]
                    .set_symbol("┬")
                    .set_style(style);
            }
        }
        let label_y = self.area.bottom() - 1;
        for (column, label) in x_ticks {
            buffer.set_string(plot.x + column, label_y, label, style);
        }
        if let Some(corner) = corner
            && display_width(corner) <= usize::from(self.gutter)
        {
            buffer.set_string(self.area.x, label_y, corner, style);
        }
    }
}

/// Keeps every `stride`-th candidate tick so the labels, starting on their ticks, stay at least
/// two cells apart and inside a plot `width` wide.
fn spaced_ticks(candidates: Vec<(u16, String)>, width: u16) -> Vec<(u16, String)> {
    let label_width = |label: &str| u16::try_from(display_width(label)).unwrap_or(u16::MAX);
    let widest = candidates
        .iter()
        .map(|(_, label)| label_width(label))
        .max()
        .unwrap_or(0);
    let span = match (candidates.first(), candidates.last()) {
        (Some((first, _)), Some((last, _))) => usize::from(last - first),
        _ => return Vec::new(),
    };
    let needed = usize::from(widest + 2) * (candidates.len() - 1);
    let stride = if span == 0 {
        candidates.len()
    } else {
        needed.div_ceil(span).max(1)
    };
    let mut ticks: Vec<(u16, String)> = Vec::new();
    for (column, label) in candidates.into_iter().step_by(stride) {
        let fits = column.saturating_add(label_width(&label)) <= width;
        let clear = ticks.last().is_none_or(|(previous, previous_label)| {
            column >= previous + label_width(previous_label) + 2
        });
        if fits && clear && !label.is_empty() {
            ticks.push((column, label));
        }
    }
    ticks
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
        // The gutter holds "100" and "log", so the plot starts after it and its axis line.
        let (buffer, start) = draw(
            40,
            8,
            &points,
            StatsChartStyle::Line,
            Some(Position::new(4, 2)),
        );
        assert_eq!(start.expect("tooltip").label, "00");
        assert_eq!(buffer[(4, 0)].fg, HOVER, "a guide marks the hovered point");
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn ticks_use_round_steps_and_short_labels() {
        assert_eq!(nice_step(0, 3), 1);
        assert_eq!(nice_step(9, 3), 3);
        assert_eq!(nice_step(1_234, 4), 400);
        assert_eq!(nice_step(3_900_000, 2), 2_000_000);
        assert_eq!(tick_label(500_000), "500k");
        assert_eq!(tick_label(2_000_000), "2m");
        assert_eq!(tick_label(1_500), "1.5k");
        assert_eq!(tick_label(10_000), "10k");
        assert_eq!(tick_label(0), "0");
    }

    #[test]
    fn x_labels_keep_apart_and_inside_the_plot() {
        let candidates = (0..24)
            .map(|hour| (hour * 3, format!("{hour:02}:00")))
            .collect::<Vec<_>>();
        let ticks = spaced_ticks(candidates, 72);
        assert_eq!(ticks.first().map(|(column, _)| *column), Some(0));
        for pair in ticks.windows(2) {
            assert!(pair[1].0 >= pair[0].0 + 7, "{ticks:?}");
        }
        assert!(ticks.iter().all(|(column, _)| column + 5 <= 72));
        assert!(ticks.len() >= 6, "{ticks:?}");
    }

    #[test]
    fn bars_get_rulers_with_round_ticks_and_dates() {
        let points = (0..30_u64)
            .map(|day| [day * 1_000_000, day * 10_000, day * 5_000])
            .collect::<Vec<_>>();
        let (buffer, _) = draw(80, 12, &points, StatsChartStyle::Bar, None);
        let text = rows(&buffer);
        // The tallest bar, a little over 29m, rounds up to a 30m top.
        assert!(text[0].trim_start().starts_with("30m┤"), "{text:#?}");
        assert!(text[9].trim_start().starts_with("0┤"), "{text:#?}");
        assert!(
            text[10].contains('└') && text[10].contains('┬'),
            "{text:#?}"
        );
        assert!(
            text[11].contains("00") && text[11].contains("04"),
            "{text:#?}"
        );
        assert!(!text[11].contains("log"));
    }

    #[test]
    fn lines_get_rulers_with_powers_of_ten() {
        let points = (0..30_u64)
            .map(|day| [day * 1_000_000, day * 10_000, day * 5])
            .collect::<Vec<_>>();
        let (buffer, _) = draw(80, 12, &points, StatsChartStyle::Line, None);
        let text = rows(&buffer);
        assert!(text[0].trim_start().starts_with("100m┤"), "{text:#?}");
        assert!(text[9].trim_start().starts_with("0┤"), "{text:#?}");
        assert!(text[11].starts_with("log"), "{text:#?}");
        assert!(text.iter().any(|row| row.contains("10k┤")), "{text:#?}");
    }

    #[test]
    fn a_log_axis_always_labels_its_top_and_zero() {
        let labels = (0..=8).map(|decade| decade.to_string()).collect::<Vec<_>>();
        let ticks = log_ticks(8, 6, &labels);
        assert_eq!(ticks.first(), Some(&(0, "8".to_owned())), "{ticks:?}");
        assert_eq!(ticks.last(), Some(&(5, "0".to_owned())), "{ticks:?}");
        for pair in ticks.windows(2) {
            assert!(pair[1].0 >= pair[0].0 + 2, "{ticks:?}");
        }
    }

    #[test]
    fn small_charts_drop_the_axis_line_then_the_rulers() {
        let points = vec![[5, 5, 5]; 10];
        let (buffer, _) = draw(40, 4, &points, StatsChartStyle::Bar, None);
        let text = rows(&buffer);
        assert!(!text.iter().any(|row| row.contains('└')), "{text:#?}");
        assert!(text[3].contains("00"), "labels stay: {text:#?}");
        let (buffer, _) = draw(40, 2, &points, StatsChartStyle::Bar, None);
        assert!(!rows(&buffer).iter().any(|row| row.contains('┤')));
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
