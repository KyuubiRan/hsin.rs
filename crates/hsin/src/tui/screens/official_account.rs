use hsin_core::{ClientKind, OfficialLoginState};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use super::super::{
    state::InputMode,
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::{centered_fixed, draw_input_field},
};
use crate::i18n::I18n;

#[allow(clippy::too_many_lines)]
pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, input: &InputMode, i18n: &I18n) {
    let popup = centered_fixed(
        area,
        78,
        if matches!(input, InputMode::OfficialLogin(_)) {
            18
        } else {
            9
        },
    );
    frame.render_widget(Clear, popup);
    let title = match input {
        InputMode::OfficialLogin(_) => "official_login_title",
        InputMode::OfficialRename { .. } => "official_rename_title",
        _ => "official_switch_title",
    };
    let block = Block::default()
        .title(i18n.text(title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED))
        .style(Style::default().bg(INPUT_BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    match input {
        InputMode::OfficialLogin(dialog) => {
            let rows = Layout::vertical([
                Constraint::Min(4),
                Constraint::Length(if dialog.client == ClientKind::Claude {
                    3
                } else {
                    0
                }),
            ])
            .split(inner);
            let status = if dialog.cancel_requested {
                "official_login_cancelling"
            } else {
                match dialog.state {
                    OfficialLoginState::Starting => "official_login_starting",
                    OfficialLoginState::AwaitingBrowser => "official_login_waiting",
                    OfficialLoginState::Completed => "official_account_added",
                    OfficialLoginState::Cancelled => "official_login_cancelled",
                    OfficialLoginState::Failed => "official_login_failed",
                }
            };
            let mut lines = vec![
                Line::from(format!("{} · {}", dialog.client, i18n.text(status))),
                Line::from(""),
                Line::from(i18n.text("official_login_description")),
            ];
            if let Some(url) = &dialog.browser_url {
                lines.push(Line::from(""));
                lines.push(Line::from(url.clone()));
            }
            if dialog.client == ClientKind::Claude {
                lines.push(Line::from(""));
                lines.push(Line::from(i18n.text("official_login_claude_code")));
            }
            if let Some(error) = &dialog.error {
                lines.push(Line::from(""));
                lines.push(Line::from(
                    error
                        .strip_prefix('@')
                        .map_or(error.as_str(), |key| i18n.text(key)),
                ));
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .style(Style::default().fg(MUTED))
                    .wrap(Wrap { trim: false }),
                rows[0],
            );
            if dialog.client == ClientKind::Claude {
                let hidden = "•".repeat(dialog.code.chars().count());
                draw_input_field(
                    frame,
                    rows[1],
                    i18n.text("official_login_code"),
                    &hidden,
                    None,
                    (!dialog.cancel_requested).then_some(dialog.cursor),
                    !dialog.cancel_requested,
                );
            }
        }
        InputMode::OfficialRename { name, cursor, .. } => {
            let rows =
                Layout::vertical([Constraint::Length(2), Constraint::Length(3)]).split(inner);
            frame.render_widget(
                Paragraph::new(i18n.text("official_rename_description"))
                    .style(Style::default().fg(MUTED)),
                rows[0],
            );
            draw_input_field(
                frame,
                rows[1],
                i18n.text("name"),
                name,
                None,
                Some(*cursor),
                true,
            );
        }
        InputMode::OfficialSwitch { .. } => {
            frame.render_widget(
                Paragraph::new(i18n.text("official_switch_description"))
                    .style(Style::default().fg(WHITE))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
        _ => {}
    }
}
