use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};

use crate::i18n::I18n;

use super::{
    state::{InputMode, State},
    widgets::{draw_banner, draw_footer},
};

mod header;
mod home;
mod model_mapping;
mod model_picker;
mod provider_form;
mod settings;

use header::draw_header;
#[cfg(test)]
pub(super) use header::{TITLE, VERSION_LABEL};
use home::{draw_details, draw_provider_list, draw_search};
use model_mapping::draw_model_mapping;
use model_picker::draw_models;
use provider_form::draw_form;
#[cfg(test)]
pub(super) use provider_form::form_field_areas;
use settings::{draw_confirm, draw_settings_screen};

pub(super) fn draw(frame: &mut Frame<'_>, state: &mut State, i18n: &I18n) {
    let area = frame.area();
    let header_height = if area.height >= 21 { 7 } else { 3 };
    let banner_height = u16::from(super::widgets::banner_text(state, i18n).is_some());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(banner_height),
            Constraint::Min(7),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, rows[0], state, i18n);
    draw_banner(frame, rows[1], state, i18n);

    if let InputMode::Settings(screen) = &state.input {
        draw_settings_screen(frame, rows[2], state, screen, i18n);
        draw_footer(frame, rows[3], state, i18n);
        return;
    }

    // The search bar stays docked above the list while a filter is active so it never covers the
    // providers behind it, and so a filter committed with enter remains visible.
    let (search_area, content) = if state.active_query().is_empty() {
        (None, rows[2])
    } else {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(4)])
            .split(rows[2]);
        (Some(parts[0]), parts[1])
    };
    if let Some(search_area) = search_area {
        draw_search(frame, search_area, state, i18n);
    }

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(content);
    draw_provider_list(frame, columns[0], state, i18n);
    draw_details(frame, columns[1], state, i18n);

    // Dialogs opened from the provider form centre over the same region it uses, so switching
    // between them does not shift the popup on screen.
    let overlay = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: rows[3].y.saturating_sub(area.y),
    };
    if let InputMode::Form(form) = &state.input {
        draw_form(frame, overlay, form, i18n);
        draw_footer(frame, rows[3], state, i18n);
        return;
    }

    match &state.input {
        InputMode::Search(_) | InputMode::Normal => {}
        InputMode::Form(_) => unreachable!("provider form is drawn over the footer"),
        InputMode::Models(picker) => draw_models(frame, rows[2], picker, i18n),
        InputMode::ModelMapping(mapping) => draw_model_mapping(frame, overlay, mapping, i18n),
        InputMode::DeleteConfirm { .. } => draw_confirm(frame, rows[2], i18n),
        InputMode::Settings(_) => unreachable!("settings screen is drawn before the home page"),
    }

    draw_footer(frame, rows[3], state, i18n);
}
