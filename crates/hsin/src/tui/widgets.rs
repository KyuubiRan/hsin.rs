use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

use crate::i18n::I18n;

use super::{
    state::{InputMode, ModelPickerMode, SettingsPage, State},
    theme::{INPUT_BG, MUTED, RED, WHITE},
};

pub(super) fn draw_input_field(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    placeholder: Option<&str>,
    selected: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let available = usize::from(area.width.saturating_sub(2));
    let visible = visible_tail(value, available);
    let line = if visible.is_empty() {
        Line::from(Span::styled(
            placeholder.unwrap_or(""),
            Style::default().fg(MUTED),
        ))
    } else {
        Line::from(Span::styled(&visible, Style::default().fg(WHITE)))
    };
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::default().bg(INPUT_BG))
            .block(
                Block::default()
                    .title(label)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if selected { RED } else { MUTED })),
            ),
        area,
    );
    if selected && area.width > 2 && area.height > 2 {
        let offset = u16::try_from(display_width(&visible).min(available)).unwrap_or(0);
        frame.set_cursor_position((area.x.saturating_add(1 + offset), area.y.saturating_add(1)));
    }
}

pub(super) fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let help = match &state.input {
        InputMode::Normal => i18n.text("help"),
        InputMode::Search(_) => i18n.text("search_help"),
        InputMode::Form(_) => i18n.text("form_help"),
        InputMode::Models(picker) => match picker.mode {
            ModelPickerMode::Browse => i18n.text("model_help"),
            ModelPickerMode::Search => i18n.text("model_search_help"),
            ModelPickerMode::Manual(_) => i18n.text("model_manual_help"),
        },
        InputMode::DeleteConfirm { .. } => i18n.text("delete_help"),
        InputMode::Settings(screen) => match &screen.page {
            SettingsPage::Root => i18n.text("settings_root_help"),
            SettingsPage::Proxy {
                editing_port: true, ..
            } => i18n.text("settings_port_help"),
            SettingsPage::Proxy { .. } => i18n.text("settings_proxy_help"),
            SettingsPage::ClientOrder { moving: true, .. } => {
                i18n.text("settings_client_order_moving_help")
            }
            SettingsPage::ClientOrder { .. } => i18n.text("settings_client_order_help"),
            SettingsPage::ClientVisibility { .. } => i18n.text("settings_client_visibility_help"),
            SettingsPage::Language { .. }
            | SettingsPage::Clients { .. }
            | SettingsPage::Import { .. } => i18n.text("settings_submenu_help"),
        },
    };
    let transient = match &state.input {
        InputMode::Form(form) => form.error.map(|error| i18n.text(error).to_owned()),
        InputMode::Models(picker) => picker
            .warning
            .as_ref()
            .map(|warning| format!("{}: {warning}", i18n.text("models_fetch_failed"))),
        _ => None,
    }
    .or_else(|| {
        state.notice.as_ref().map(|notice| {
            notice
                .strip_prefix('@')
                .map_or(notice.as_str(), |key| i18n.text(key))
                .to_owned()
        })
    });
    let (line, color) = transient.map_or_else(|| (help.to_owned(), MUTED), |notice| (notice, RED));
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

pub(super) fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

fn visible_tail(value: &str, width: usize) -> String {
    let mut used = 0;
    let mut reversed = Vec::new();
    for character in value.chars().rev() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            break;
        }
        used += character_width;
        reversed.push(character);
    }
    reversed.into_iter().rev().collect()
}

pub(super) fn content_width(area: Rect, desired: usize, minimum: u16, maximum: u16) -> u16 {
    let desired = u16::try_from(desired).unwrap_or(u16::MAX);
    desired
        .max(minimum.min(area.width))
        .min(maximum)
        .min(area.width)
}

pub(super) fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(area.width.saturating_sub(width) / 2),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(vertical[1])[1]
}

pub(super) fn bottom_centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.bottom().saturating_sub(height),
        width,
        height,
    }
}
