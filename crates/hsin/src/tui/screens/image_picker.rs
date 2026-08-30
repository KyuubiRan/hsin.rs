use hsin_core::{ClientKind, ProviderScope};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::i18n::I18n;

use super::super::{
    state::{ImageModelPicker, ModelPickerMode, State, visible_image_models},
    theme::RED,
    widgets::{
        centered_fixed, content_width, display_width, draw_input_field, draw_list_scroll_indicators,
    },
};

pub(super) fn draw_image_models(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &ImageModelPicker,
    i18n: &I18n,
) {
    let models = visible_image_models(picker);
    let longest = models
        .iter()
        .map(|model| display_width(model).saturating_add(6))
        .max()
        .unwrap_or(0);
    let width = content_width(area, longest, 48, 92);
    if let ModelPickerMode::Manual(value) = &picker.mode {
        let popup = centered_fixed(area, width, 5);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(i18n.text("image_model_manual"))
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
    let popup = centered_fixed(
        area,
        width,
        2_u16
            .saturating_add(search_height)
            .saturating_add(list_height),
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(i18n.text("select_image_models"))
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
    let items = models
        .iter()
        .map(|model| {
            let checked = if picker.checked.contains(*model) {
                "[x]"
            } else {
                "[ ]"
            };
            let preferred = if picker.preferred.as_deref() == Some(*model) {
                " *"
            } else {
                ""
            };
            ListItem::new(format!("{checked} {model}{preferred}"))
        })
        .collect::<Vec<_>>();
    let item_count = items.len();
    let mut state = ListState::default()
        .with_selected((!items.is_empty()).then_some(picker.selected.min(items.len() - 1)));
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("> ").highlight_style(
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

pub(super) fn draw_image_source(frame: &mut Frame<'_>, area: Rect, selected: usize, i18n: &I18n) {
    draw_choice_list(
        frame,
        area,
        i18n.text("add_image_provider"),
        vec![
            i18n.text("image_source_import").to_owned(),
            i18n.text("image_source_manual").to_owned(),
        ],
        selected,
    );
}

pub(super) fn draw_image_import(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &State,
    selected: usize,
    i18n: &I18n,
) {
    let items = state
        .providers
        .iter()
        .filter(|provider| {
            provider.client == ClientKind::Codex
                && provider.scope == ProviderScope::Primary
                && !provider.official
                && provider.credential_configured
                && !provider.codex_image.enabled
        })
        .map(|provider| format!("{}\n  {}", provider.name, provider.base_url))
        .collect::<Vec<_>>();
    let items = if items.is_empty() {
        vec![i18n.text("codex_image_import_empty").to_owned()]
    } else {
        items
    };
    draw_choice_list(
        frame,
        area,
        i18n.text("image_source_import"),
        items,
        selected,
    );
}

fn draw_choice_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: Vec<String>,
    selected: usize,
) {
    let longest = items
        .iter()
        .map(|item| display_width(item))
        .max()
        .unwrap_or(0);
    let width = content_width(area, longest.saturating_add(6), 46, 88);
    let item_heights = items
        .iter()
        .map(|item| item.lines().count().max(1))
        .collect::<Vec<_>>();
    let list_height = item_heights
        .iter()
        .copied()
        .fold(0_usize, usize::saturating_add)
        .clamp(1, 12);
    let height = u16::try_from(list_height).unwrap_or(12).saturating_add(2);
    let popup = centered_fixed(area, width, height);
    frame.render_widget(Clear, popup);
    let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED));
    let list_area = block.inner(popup);
    frame.render_stateful_widget(
        List::new(items.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .fg(RED)
                    .bg(Color::Rgb(55, 28, 32))
                    .add_modifier(Modifier::BOLD),
            )
            .block(block),
        popup,
        &mut state,
    );
    draw_list_scroll_indicators(frame, popup, list_area, &state, item_heights);
}
