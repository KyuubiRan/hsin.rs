use hsin_core::ClientKind;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::i18n::I18n;

use super::super::{
    state::{InputMode, State},
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::display_width,
};

const FULL_HEADER_HEIGHT: u16 = 7;
const TITLE_WIDTH: u16 = 36;
const BANNER_HEIGHT: u16 = 6;
pub(in crate::tui) const TITLE: &str = "Hsin";
pub(in crate::tui) const VERSION_LABEL: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Rows of the "Hsin" wordmark, split into (H, S, rest) so the "S" can be tinted red.
const BANNER: [(&str, &str, &str); 6] = [
    ("██╗  ██╗", "███████╗", "██╗███╗   ██╗"),
    ("██║  ██║", "██╔════╝", "██║████╗  ██║"),
    ("███████║", "███████╗", "██║██╔██╗ ██║"),
    ("██╔══██║", "╚════██║", "██║██║╚██╗██║"),
    ("██║  ██║", "███████║", "██║██║ ╚████║"),
    ("╚═╝  ╚═╝", "╚══════╝", "╚═╝╚═╝  ╚═══╝"),
];

pub(super) fn draw_header(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(RED));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if area.height < FULL_HEADER_HEIGHT {
        let title_width = compact_title_width().min(inner.width.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(compact_title_line()),
            Rect {
                x: inner.x.saturating_add(1),
                width: title_width,
                ..inner
            },
        );
        if !matches!(&state.input, InputMode::Settings(_)) {
            draw_compact_client_switcher(frame, inner, state, i18n, title_width);
        }
        return;
    }

    let white = Style::default().fg(WHITE).add_modifier(Modifier::BOLD);
    let red = Style::default().fg(RED).add_modifier(Modifier::BOLD);
    let lines = BANNER
        .iter()
        .map(|(head, s, tail)| {
            Line::from(vec![
                Span::styled(*head, white),
                Span::styled(*s, red),
                Span::styled(*tail, white),
            ])
        })
        .collect::<Vec<_>>();

    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            x: inner.x.saturating_add(1),
            y: inner.y,
            width: TITLE_WIDTH.min(inner.width.saturating_sub(1)),
            height: BANNER_HEIGHT.min(inner.height),
        },
    );

    let controls_x = inner.x.saturating_add(TITLE_WIDTH).saturating_add(2);
    if controls_x < inner.right() {
        frame.render_widget(
            Paragraph::new(VERSION_LABEL).style(Style::default().fg(MUTED)),
            Rect {
                x: controls_x,
                y: inner.y.saturating_add(BANNER_HEIGHT.saturating_sub(1)),
                width: inner.right().saturating_sub(controls_x),
                height: 1,
            },
        );
    }

    let controls = Rect {
        x: controls_x.min(inner.right()),
        y: inner.y,
        width: inner.right().saturating_sub(controls_x),
        height: 1.min(inner.height),
    };
    if controls.width == 0 || controls.height == 0 {
        return;
    }
    if matches!(&state.input, InputMode::Settings(_)) {
        return;
    }
    frame.render_widget(
        Paragraph::new(client_switcher_line(state, i18n)).alignment(Alignment::Right),
        controls,
    );
}

fn compact_title_line() -> Line<'static> {
    let title = Style::default().fg(WHITE).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("H", title),
        Span::styled("s", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
        Span::styled("in", title),
        Span::raw(" "),
        Span::styled(VERSION_LABEL, Style::default().fg(MUTED)),
    ])
}

fn compact_title_width() -> u16 {
    u16::try_from(
        display_width(TITLE)
            .saturating_add(1)
            .saturating_add(display_width(VERSION_LABEL)),
    )
    .unwrap_or(u16::MAX)
}

fn draw_compact_client_switcher(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &State,
    i18n: &I18n,
    title_width: u16,
) {
    let full_width = client_switcher_width(i18n);
    let title_right = area.x.saturating_add(1).saturating_add(title_width);
    let available = area.right().saturating_sub(title_right.saturating_add(1));
    let line = if available >= full_width {
        client_switcher_line(state, i18n)
    } else {
        selected_client_line(state, i18n)
    };
    let width = u16::try_from(line.width())
        .unwrap_or(u16::MAX)
        .min(available);
    if width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(line).alignment(Alignment::Right),
        Rect {
            x: area.right().saturating_sub(width),
            y: area.y,
            width,
            height: 1,
        },
    );
}

fn client_switcher_line<'a>(state: &State, i18n: &'a I18n) -> Line<'a> {
    Line::from(vec![
        client_span(i18n.text("codex"), state.client == ClientKind::Codex),
        Span::raw(" "),
        client_span(i18n.text("claude"), state.client == ClientKind::Claude),
    ])
}

fn selected_client_line<'a>(state: &State, i18n: &'a I18n) -> Line<'a> {
    let label = match state.client {
        ClientKind::Codex => i18n.text("codex"),
        ClientKind::Claude => i18n.text("claude"),
    };
    Line::from(client_span(label, true))
}

fn client_span(label: &str, selected: bool) -> Span<'_> {
    let style = if selected {
        Style::default()
            .fg(WHITE)
            .bg(RED)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(WHITE).bg(INPUT_BG)
    };
    Span::styled(format!(" {label} "), style)
}

fn client_switcher_width(i18n: &I18n) -> u16 {
    let width = display_width(i18n.text("codex"))
        .saturating_add(display_width(i18n.text("claude")))
        .saturating_add(5);
    u16::try_from(width).unwrap_or(u16::MAX)
}
