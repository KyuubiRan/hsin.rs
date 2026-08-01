use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::i18n::I18n;

use super::super::{
    state::{MAPPING_TIERS, ModelMappingForm},
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::{centered_fixed, content_width, draw_input_field},
};

const ROW_HEIGHT: u16 = 3;
const TIER_COUNT: u16 = 4;
/// Master toggle plus one row per tier, inside the popup border.
const POPUP_HEIGHT: u16 = ROW_HEIGHT * (TIER_COUNT + 1) + 2;
/// Reserved on a tier row for the trailing 1M checkbox: `[x]` plus its borders.
const CHECKBOX_WIDTH: u16 = 5;

pub(super) fn draw_model_mapping(
    frame: &mut Frame<'_>,
    area: Rect,
    mapping: &ModelMappingForm,
    i18n: &I18n,
) {
    let proportional_width = area.width.saturating_mul(4) / 5;
    let width = content_width(area, 64, 64, 100)
        .max(proportional_width)
        .min(area.width);
    let popup = centered_fixed(area, width, POPUP_HEIGHT.min(area.height));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(i18n.text("model_mapping"))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let row = |index: u16| Rect {
        x: inner.x,
        y: inner.y.saturating_add(index * ROW_HEIGHT),
        width: inner.width,
        height: ROW_HEIGHT,
    };
    let rows = usize::from(inner.height / ROW_HEIGHT);
    if rows == 0 {
        return;
    }

    draw_toggle(
        frame,
        row(0),
        i18n.text("model_mapping_enabled"),
        mapping.enabled,
        mapping.field == 0,
    );

    for (index, tier) in MAPPING_TIERS.iter().enumerate() {
        let Ok(offset) = u16::try_from(index + 1) else {
            break;
        };
        if usize::from(offset) >= rows {
            break;
        }
        let area = row(offset);
        let selected = mapping.enabled && mapping.field == index + 1;
        let value = &mapping.rows[index];
        // Only the tiers with a 1M variant reserve room for a checkbox; Haiku uses the full width.
        let (field_area, checkbox_area) = if tier.context_1m {
            (
                Rect {
                    width: area.width.saturating_sub(CHECKBOX_WIDTH),
                    ..area
                },
                Some(Rect {
                    x: area
                        .x
                        .saturating_add(area.width.saturating_sub(CHECKBOX_WIDTH)),
                    width: CHECKBOX_WIDTH,
                    ..area
                }),
            )
        } else {
            (area, None)
        };
        draw_input_field(
            frame,
            field_area,
            tier.label,
            &value.model,
            Some(tier.default_model),
            selected,
        );
        if let Some(checkbox_area) = checkbox_area {
            draw_checkbox(
                frame,
                checkbox_area,
                value.context_1m,
                selected,
                mapping.enabled,
                i18n,
            );
        }
    }
}

fn draw_toggle(frame: &mut Frame<'_>, area: Rect, label: &str, on: bool, selected: bool) {
    let value = if on { "‹ on ›" } else { "‹ off ›" };
    // The master toggle is always reachable, so it never dims.
    draw_boxed(frame, area, label, value, selected, true);
}

fn draw_checkbox(
    frame: &mut Frame<'_>,
    area: Rect,
    checked: bool,
    selected: bool,
    enabled: bool,
    i18n: &I18n,
) {
    let value = if checked { "[x]" } else { "[ ]" };
    draw_boxed(
        frame,
        area,
        i18n.text("model_mapping_1m"),
        value,
        selected,
        enabled,
    );
}

/// `selected` is the focused control, `enabled` whether the control can be focused at all — a
/// dimmed box means the master toggle is off, not that the value is unset.
fn draw_boxed(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    selected: bool,
    enabled: bool,
) {
    let foreground = if selected {
        RED
    } else if enabled {
        WHITE
    } else {
        MUTED
    };
    frame.render_widget(
        Paragraph::new(value)
            .style(Style::default().fg(foreground).bg(INPUT_BG))
            .block(
                Block::default()
                    .title(label)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(foreground)),
            ),
        area,
    );
}
