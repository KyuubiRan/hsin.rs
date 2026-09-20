use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::i18n::I18n;

use super::super::{
    state::{MappingModelPicker, ModelPickerMode, visible_mapping_models},
    theme::{MUTED, RED},
    widgets::{
        centered_fixed, content_width, display_width, draw_input_field, draw_list_scroll_indicators,
    },
};

/// The model-list dialog a mapping row opens with tab.
///
/// It is drawn on top of the mapping dialog it was opened from, which stays visible underneath so
/// the operator can still see which tier they are filling in — and so cancelling reads as a step
/// back rather than a different screen.
pub(super) fn draw_mapping_models(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &MappingModelPicker,
    i18n: &I18n,
) {
    let models = visible_mapping_models(picker);
    let longest = models
        .iter()
        .map(|model| display_width(model))
        .max()
        .unwrap_or(0)
        .saturating_add(6);
    let width = content_width(area, longest, 42, 82);
    if let ModelPickerMode::Manual(value) = &picker.mode {
        let popup = centered_fixed(area, width, 5);
        frame.render_widget(Clear, popup);
        let title = mapping_row_title(picker, i18n);
        let block = Block::default()
            .title(format!("{} · {}", i18n.text("model_manual"), title))
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
    let list_height = u16::try_from(models.len().clamp(1, 12)).unwrap_or(12);
    let height = 2_u16
        .saturating_add(search_height)
        .saturating_add(list_height);
    let popup = centered_fixed(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(
            "{} · {}",
            i18n.text("select_model"),
            mapping_row_title(picker, i18n)
        ))
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
    if models.is_empty() {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(i18n.text("mapping_models_empty"))
                .style(Style::default().fg(MUTED)),
            rows[1],
        );
        return;
    }
    let items = models.into_iter().map(ListItem::new).collect::<Vec<_>>();
    let item_count = items.len();
    let mut state = ListState::default().with_selected(Some(picker.selected.min(item_count - 1)));
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

/// The label of the row the dialog was opened from, so the popup title says which tier it fills.
fn mapping_row_title<'a>(picker: &MappingModelPicker, i18n: &'a I18n) -> &'a str {
    picker.field.label(i18n)
}
