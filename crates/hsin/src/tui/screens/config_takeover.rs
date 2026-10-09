use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::i18n::I18n;

use super::super::{
    mouse::{Hit, HitMap, plain},
    state::ConfigTakeoverDialog,
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::centered_fixed,
};

pub(super) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &ConfigTakeoverDialog,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    // Unclaimed legacy state belongs to no other instance; it needs a manual
    // native restore, so its explanation and recovery steps replace takeover.
    let legacy = !dialog.details.targets.is_empty()
        && dialog.details.targets.iter().all(I18n::ownership_is_legacy);
    let rows_per_target = if legacy { 11 } else { 6 };
    let popup = centered_fixed(
        area,
        76,
        12 + u16::try_from(dialog.details.targets.len()).unwrap_or(2) * rows_per_target,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(i18n.text(if legacy {
            "config_takeover_legacy_title"
        } else {
            "config_takeover_title"
        }))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED))
        .style(Style::default().bg(INPUT_BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut lines = vec![
        Line::from(i18n.text(if legacy {
            "config_takeover_legacy_description"
        } else if dialog.startup {
            "config_takeover_startup_description"
        } else {
            "config_takeover_description"
        })),
        Line::from(""),
    ];
    for target in &dialog.details.targets {
        lines.push(Line::from(Span::styled(
            format!("{}: {}", target.client, i18n.owner_label(target)),
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(target.config_path.clone()));
        lines.extend(
            i18n.ownership_reason_lines(target)
                .into_iter()
                .map(Line::from),
        );
        lines.push(Line::from(""));
    }
    if dialog.operation.is_none() {
        lines.push(Line::from(i18n.text("config_takeover_working")));
    } else {
        lines.push(Line::from(i18n.text(if legacy {
            "config_takeover_legacy_unavailable"
        } else if dialog.available() {
            if dialog.startup {
                "config_takeover_startup_restore_notice"
            } else {
                "config_takeover_restore_notice"
            }
        } else {
            "config_takeover_unavailable"
        })));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: false }),
        Rect {
            height: inner.height.saturating_sub(3),
            ..inner
        },
    );
    draw_buttons(frame, inner, dialog, i18n, hits);
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    inner: Rect,
    dialog: &ConfigTakeoverDialog,
    i18n: &I18n,
    hits: &mut HitMap,
) {
    let buttons = Rect {
        y: inner.bottom().saturating_sub(2),
        height: 1.min(inner.height),
        ..inner
    };
    let width = buttons.width / 2;
    let confirm = if dialog.startup {
        "config_takeover_startup_confirm"
    } else {
        "config_takeover_confirm"
    };
    for (index, label) in ["config_takeover_cancel", confirm].into_iter().enumerate() {
        let index_u16 = u16::try_from(index).unwrap_or(0);
        let button = Rect {
            x: buttons.x + index_u16 * width,
            width,
            ..buttons
        };
        let enabled = dialog.operation.is_some() && (index == 0 || dialog.available());
        let style = if enabled && index == dialog.selected {
            Style::default()
                .fg(WHITE)
                .bg(RED)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        frame.render_widget(
            Paragraph::new(i18n.text(label))
                .alignment(ratatui::layout::Alignment::Center)
                .style(style),
            button,
        );
        if enabled {
            hits.push(
                button,
                Hit::Row {
                    index,
                    selected: dialog.selected,
                    activate: Some(plain(KeyCode::Enter)),
                },
            );
        }
    }
}
