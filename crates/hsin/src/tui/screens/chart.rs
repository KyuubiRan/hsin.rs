//! Token series drawn either as bars stacked by kind of token or as one line per kind.

use hsin_core::{StatsChartStyle, UsageTokenSummary};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{Axis, Chart, Dataset, GraphType},
};

use crate::{i18n::I18n, usage_format::compact_tokens};

use super::super::theme::{MUTED, RED};

/// Cache-hit input, other input, and output, from darkest to brightest: the cheapest tokens are
/// the most numerous, so they get the quietest colour.
pub(super) const SHADES: [Color; 3] = [
    Color::Rgb(92, 36, 44),
    Color::Rgb(160, 50, 60),
    Color::Rgb(242, 128, 138),
];

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
    let keys = ["stats_chart_hit", "stats_chart_miss", "stats_chart_output"];
    let mut spans = Vec::new();
    for (key, color) in keys.into_iter().zip(SHADES) {
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

pub(super) fn draw_series(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[[u64; 3]],
    style: StatsChartStyle,
    x_labels: Option<(String, String)>,
) {
    if area.width == 0 || area.height == 0 || points.is_empty() {
        return;
    }
    match style {
        StatsChartStyle::Bar => draw_bars(frame, area, points),
        StatsChartStyle::Line => draw_lines(frame, area, points, x_labels),
    }
}

/// Bars stacked bottom to top as cache-hit input, other input, output, at an eighth of a cell.
/// A part that is not zero always gets at least one eighth, so a thin slice of output stays
/// visible above a tall column of cached input; a cell shared by several parts takes the colour
/// of the topmost one.
fn draw_bars(frame: &mut Frame<'_>, area: Rect, points: &[[u64; 3]]) {
    let columns = merge_columns(points, usize::from(area.width));
    let width = (usize::from(area.width) / columns.len()).max(1);
    let max = columns
        .iter()
        .map(|column| column.iter().sum::<u64>())
        .max()
        .unwrap_or(0);
    if max == 0 {
        return;
    }
    let units = u64::from(area.height) * 8;
    let buffer = frame.buffer_mut();
    for (index, column) in columns.iter().enumerate() {
        let mut bounds = [0_u64; 3];
        let mut top = 0;
        for (part, value) in column.iter().enumerate() {
            let mut size = value * units / max;
            if *value > 0 && size == 0 {
                size = 1;
            }
            top = (top + size).min(units);
            bounds[part] = top;
        }
        for row in 0..area.height {
            let low = u64::from(row) * 8;
            let filled = top.saturating_sub(low).min(8);
            if filled == 0 {
                break;
            }
            let cell_top = low + filled;
            let color = (0..3)
                .rev()
                .find(|part| {
                    let start = if *part == 0 { 0 } else { bounds[part - 1] };
                    bounds[*part] > start && start < cell_top && bounds[*part] > low
                })
                .map_or(SHADES[0], |part| SHADES[part]);
            let symbol = EIGHTHS[usize::try_from(filled).unwrap_or(8)];
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
                buffer[(x, y)]
                    .set_symbol(symbol)
                    .set_style(Style::default().fg(color));
            }
        }
    }
}

/// Merges neighbouring points until the series fits `width` columns.
fn merge_columns(points: &[[u64; 3]], width: usize) -> Vec<[u64; 3]> {
    let per_column = points.len().div_ceil(width.max(1)).max(1);
    points
        .chunks(per_column)
        .map(|chunk| {
            chunk.iter().fold([0; 3], |sum, point| {
                [sum[0] + point[0], sum[1] + point[1], sum[2] + point[2]]
            })
        })
        .collect()
}

/// One line per kind of token on a logarithmic scale: cache hits outnumber output a hundredfold
/// or more, and a linear scale would flatten everything but them.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn draw_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[[u64; 3]],
    x_labels: Option<(String, String)>,
) {
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
    let labelled = area.height >= 5;
    let muted = Style::default().fg(MUTED);
    let mut x_axis = Axis::default()
        .bounds([0.0, (points.len().max(2) - 1) as f64])
        .style(muted);
    let mut y_axis = Axis::default().bounds([0.0, y_max]).style(muted);
    if labelled {
        if let Some((first, last)) = x_labels {
            x_axis = x_axis.labels([Span::styled(first, muted), Span::styled(last, muted)]);
        }
        y_axis = y_axis.labels([
            Span::styled("0", muted),
            Span::styled(
                format!("{} (log)", compact_tokens(10_u64.pow(y_max as u32))),
                muted,
            ),
        ]);
    }
    frame.render_widget(
        Chart::new(datasets)
            .x_axis(x_axis)
            .y_axis(y_axis)
            .legend_position(None)
            .style(Style::default().fg(RED)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_series_merge_to_fit_the_width() {
        let points = vec![[1, 2, 3]; 10];
        let merged = merge_columns(&points, 4);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0], [3, 6, 9]);
        assert_eq!(merged.iter().map(|point| point[0]).sum::<u64>(), 10);
        assert_eq!(merge_columns(&points, 20).len(), 10);
    }

    #[test]
    fn a_thin_output_slice_still_shows_on_top_of_a_bar() {
        let backend = ratatui::backend::TestBackend::new(4, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_bars(frame, frame.area(), &[[1_000_000, 0, 1]]);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let top = &buffer[(0, 0)];
        assert_eq!(top.fg, SHADES[2], "the output eighth colours the top cell");
        assert_eq!(buffer[(0, 2)].fg, SHADES[0]);
        assert_eq!(buffer[(0, 2)].symbol(), "█");
    }
}
