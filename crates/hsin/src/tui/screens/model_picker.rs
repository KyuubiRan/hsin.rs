use hsin_core::ClientSettings;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::i18n::I18n;

use super::super::{
    state::{ContextPicker, ModelPicker, ModelPickerMode, visible_models},
    theme::RED,
    widgets::{
        centered_fixed, content_width, display_width, draw_input_field, draw_list_scroll_indicators,
    },
};

pub(super) fn draw_context_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &ContextPicker,
    settings: &ClientSettings,
    i18n: &I18n,
) {
    let presets = picker.kind.presets(settings);
    let width = content_width(area, 20, 32, 50);
    let height = u16::try_from(presets.len().saturating_add(3).min(14)).unwrap_or(14);
    let popup = centered_fixed(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(i18n.text(picker.kind.label()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let items = std::iter::once(ListItem::new(i18n.text("context_preset_empty")))
        .chain(presets.iter().map(|value| ListItem::new(value.to_string())))
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(picker.selected.min(presets.len())));
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("› ").highlight_style(
            Style::default()
                .fg(RED)
                .bg(Color::Rgb(55, 28, 32))
                .add_modifier(Modifier::BOLD),
        ),
        inner,
        &mut state,
    );
    draw_list_scroll_indicators(frame, popup, inner, &state, (0..=presets.len()).map(|_| 1));
}

pub(super) fn draw_models(frame: &mut Frame<'_>, area: Rect, picker: &ModelPicker, i18n: &I18n) {
    let models = visible_models(picker);
    let longest = models
        .iter()
        .map(|model| display_width(model))
        .chain(std::iter::once(display_width(i18n.text("model_no_change"))))
        .max()
        .unwrap_or(0)
        .saturating_add(6);
    let width = content_width(area, longest, 42, 82);
    if let ModelPickerMode::Manual(value) = &picker.mode {
        let popup = centered_fixed(area, width, 5);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(i18n.text("model_manual"))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(RED));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        draw_input_field(
            frame,
            inner,
            i18n.text("model"),
            value,
            None,
            Some(picker.cursor),
            true,
        );
        return;
    }

    let has_search = matches!(picker.mode, ModelPickerMode::Search) || !picker.query.is_empty();
    let search_height = u16::from(has_search) * 3;
    let list_height = u16::try_from(models.len().saturating_add(1).min(12)).unwrap_or(12);
    let height = 2_u16
        .saturating_add(search_height)
        .saturating_add(list_height.max(1));
    let popup = centered_fixed(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(i18n.text("select_model"))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(search_height), Constraint::Min(1)])
        .split(inner);
    if has_search {
        draw_input_field(
            frame,
            rows[0],
            i18n.text("search"),
            &picker.query,
            None,
            matches!(picker.mode, ModelPickerMode::Search).then_some(picker.cursor),
            true,
        );
    }
    let mut items = Vec::with_capacity(models.len() + 1);
    items.push(ListItem::new(i18n.text("model_no_change")));
    items.extend(models.into_iter().map(ListItem::new));
    let item_count = items.len();
    let mut state = ListState::default().with_selected(Some(picker.selected.min(items.len() - 1)));
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("› ").highlight_style(
            Style::default()
                .fg(RED)
                .bg(Color::Rgb(55, 28, 32))
                .add_modifier(Modifier::BOLD),
        ),
        rows[1],
        &mut state,
    );
    draw_list_scroll_indicators(frame, popup, rows[1], &state, (0..item_count).map(|_| 1));
}
