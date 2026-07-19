use hsin_core::{AuthScheme, ClientKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::i18n::I18n;

use super::super::{
    state::ProviderForm,
    theme::{INPUT_BG, MUTED, RED, WHITE},
    widgets::{bottom_centered_fixed, content_width, display_width, draw_input_field},
};

const FORM_FIELD_COUNT: usize = 5;
const FORM_FIELD_HEIGHT: u16 = 3;
const FORM_POPUP_HEIGHT: u16 = FORM_FIELD_HEIGHT * 5 + 2;

pub(super) fn draw_form(frame: &mut Frame<'_>, area: Rect, form: &ProviderForm, i18n: &I18n) {
    let hidden = "•".repeat(form.secret.chars().count());
    let secret = if form.secret_visible {
        form.secret.as_str()
    } else {
        hidden.as_str()
    };
    let secret_placeholder = if form.id.is_some() || form.copied_secret.is_some() {
        i18n.text("api_key_preserve_hint")
    } else {
        "sk-****"
    };
    let auth = match form.auth_scheme {
        AuthScheme::Bearer => "‹ Bearer ›",
        AuthScheme::XApiKey => "‹ X-API-Key ›",
        AuthScheme::OAuth => "‹ OAuth ›",
    };
    let longest = [
        display_width(&form.name),
        display_width(&form.description),
        display_width(&form.base_url),
        display_width(&hidden),
        display_width(auth),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
    .saturating_add(6);
    let proportional_width = area.width.saturating_mul(4) / 5;
    let width = content_width(area, longest, 64, 100)
        .max(proportional_width)
        .min(area.width);
    let popup = bottom_centered_fixed(area, width, FORM_POPUP_HEIGHT);
    frame.render_widget(Clear, popup);
    let title = form_title(form, i18n);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    for (index, field) in form_field_areas(inner, form.field) {
        match index {
            0 => draw_input_field(
                frame,
                field,
                i18n.text("base_url"),
                &form.base_url,
                Some(match form.client {
                    ClientKind::Codex => "https://api.openai.com/v1",
                    ClientKind::Claude => "https://api.anthropic.com",
                }),
                form.field == index,
            ),
            1 => draw_input_field(
                frame,
                field,
                i18n.text("api_key"),
                secret,
                Some(secret_placeholder),
                form.field == index,
            ),
            2 => draw_input_field(
                frame,
                field,
                i18n.text("name"),
                &form.name,
                Some(i18n.text("name_domain_hint")),
                form.field == index,
            ),
            3 => draw_input_field(
                frame,
                field,
                i18n.text("description"),
                &form.description,
                Some(i18n.text("description_hint")),
                form.field == index,
            ),
            _ => {
                let selected = form.field == index;
                let auth_style = if selected {
                    Style::default().fg(RED).bg(INPUT_BG)
                } else {
                    Style::default().fg(WHITE).bg(INPUT_BG)
                };
                frame.render_widget(
                    Paragraph::new(auth).style(auth_style).block(
                        Block::default()
                            .title(i18n.text("auth"))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(if selected { RED } else { MUTED })),
                    ),
                    field,
                );
            }
        }
    }
}

fn form_title(form: &ProviderForm, i18n: &I18n) -> String {
    let title = if form.id.is_some() {
        i18n.text("edit_provider")
    } else {
        i18n.text("add_provider")
    };
    if form.discovering_models {
        format!("{title} · {}", i18n.text("fetching_models"))
    } else {
        title.to_owned()
    }
}

pub(in crate::tui) fn form_field_areas(area: Rect, selected: usize) -> Vec<(usize, Rect)> {
    let visible = usize::from(area.height / FORM_FIELD_HEIGHT).min(FORM_FIELD_COUNT);
    if visible == 0 {
        return Vec::new();
    }
    let selected = selected.min(FORM_FIELD_COUNT - 1);
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(FORM_FIELD_COUNT - visible);
    (start..start + visible)
        .enumerate()
        .map(|(offset, index)| {
            (
                index,
                Rect {
                    x: area.x,
                    y: area.y.saturating_add(
                        u16::try_from(offset).unwrap_or(u16::MAX) * FORM_FIELD_HEIGHT,
                    ),
                    width: area.width,
                    height: FORM_FIELD_HEIGHT,
                },
            )
        })
        .collect()
}
