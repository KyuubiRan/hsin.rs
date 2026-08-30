use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, ListState, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

use crate::i18n::I18n;

use super::{
    state::{InputMode, ModelPickerMode, SettingsPage, State},
    theme::{INPUT_BG, MUTED, RED, WHITE},
};

/// `focus` carries the caret position, in characters, when the field has focus, and is `None` when
/// it does not. `enabled` is whether the field can be focused at all — a dimmed border means the
/// field is inert, not merely unfocused.
pub(super) fn draw_input_field(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    placeholder: Option<&str>,
    focus: Option<usize>,
    enabled: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let available = usize::from(area.width.saturating_sub(2));
    let caret = focus.unwrap_or_else(|| value.chars().count());
    let (visible, column) = visible_window(value, caret, available);
    let line = if visible.is_empty() {
        Line::from(Span::styled(
            placeholder.unwrap_or(""),
            Style::default().fg(MUTED),
        ))
    } else {
        Line::from(Span::styled(
            &visible,
            Style::default().fg(if enabled { WHITE } else { MUTED }),
        ))
    };
    let border = if focus.is_some() {
        RED
    } else if enabled {
        WHITE
    } else {
        MUTED
    };
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::default().bg(INPUT_BG))
            .block(
                Block::default()
                    .title(label)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border)),
            ),
        area,
    );
    if focus.is_some() && area.width > 2 && area.height > 2 {
        let offset = u16::try_from(column).unwrap_or(0);
        frame.set_cursor_position((area.x.saturating_add(1 + offset), area.y.saturating_add(1)));
    }
}

/// A persistent warning strip for conditions the operator must act on. Unlike
/// `state.notice`, which is transient feedback in the footer, these stay until
/// the underlying condition is gone.
pub(super) fn banner_text<'a>(state: &State, i18n: &'a I18n) -> Option<&'a str> {
    if state.status.security_locked {
        return Some(i18n.text("banner_key_store_locked"));
    }
    if !state.status.recovery_key_exported {
        return Some(i18n.text("banner_recovery_key_missing"));
    }
    None
}

pub(super) fn draw_banner(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let Some(text) = banner_text(state, i18n) else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(WHITE).bg(RED),
        )))
        .wrap(Wrap { trim: true }),
        area,
    );
}

pub(super) fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let help = match &state.input {
        InputMode::Normal => i18n.text(if state.image_section {
            "image_help"
        } else {
            "help"
        }),
        InputMode::Search { .. } => i18n.text("search_help"),
        InputMode::Form(_) => i18n.text("form_help"),
        InputMode::Models(picker) => match picker.mode {
            ModelPickerMode::Browse => i18n.text("model_help"),
            ModelPickerMode::Search => i18n.text("model_search_help"),
            ModelPickerMode::Manual(_) => i18n.text("model_manual_help"),
        },
        InputMode::ImageModels(picker) => match picker.mode {
            ModelPickerMode::Browse => i18n.text("image_model_help"),
            ModelPickerMode::Search => i18n.text("model_search_help"),
            ModelPickerMode::Manual(_) => i18n.text("model_manual_help"),
        },
        InputMode::ImageSource { .. } | InputMode::ImageImport { .. } => {
            i18n.text("image_source_help")
        }
        InputMode::ModelMapping(_) => i18n.text("model_mapping_help"),
        InputMode::DeleteConfirm { .. } => i18n.text("delete_help"),
        InputMode::Settings(screen) => match &screen.page {
            SettingsPage::Root => i18n.text("settings_root_help"),
            SettingsPage::Proxy {
                editing_host: true, ..
            } => i18n.text("settings_address_help"),
            SettingsPage::Proxy {
                editing_port: true, ..
            } => i18n.text("settings_port_help"),
            SettingsPage::Proxy { .. } => i18n.text("settings_proxy_help"),
            SettingsPage::UpstreamProxy { .. } => i18n.text("settings_upstream_proxy_help"),
            SettingsPage::ClientOrder { moving: true, .. } => {
                i18n.text("settings_client_order_moving_help")
            }
            SettingsPage::ClientOrder { .. } => i18n.text("settings_client_order_help"),
            SettingsPage::ClientVisibility { .. } => i18n.text("settings_client_visibility_help"),
            SettingsPage::ClientConfig { .. } => i18n.text("settings_client_auth_help"),
            SettingsPage::Language { .. } | SettingsPage::Clients { .. } => {
                i18n.text("settings_submenu_help")
            }
        },
    };
    // With a committed filter, esc clears it instead of quitting; advertise that.
    let help = if matches!(state.input, InputMode::Normal) && !state.search.is_empty() {
        format!("{help} · {}", i18n.text("search_clear_hint"))
    } else {
        help.to_owned()
    };
    let transient = match &state.input {
        InputMode::Form(form) => form.error.map(|error| i18n.text(error).to_owned()),
        InputMode::Models(picker) => picker
            .warning
            .as_ref()
            .map(|warning| format!("{}: {warning}", i18n.text("models_fetch_failed"))),
        InputMode::ImageModels(picker) => picker.warning.as_ref().map(|warning| {
            warning
                .strip_prefix('@')
                .map_or(warning.as_str(), |key| i18n.text(key))
                .to_owned()
        }),
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
    let (line, color) = match transient {
        Some(notice) => (notice, RED),
        // The armed delete prompt is the one help line that is about to do something destructive,
        // and it replaced a red dialog, so it keeps the warning colour rather than fading out.
        None => (
            help,
            if matches!(state.input, InputMode::DeleteConfirm { .. }) {
                RED
            } else {
                MUTED
            },
        ),
    };
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

/// The slice of `value` that fits in `width`, and the caret's column inside it. The window is
/// anchored on the caret, so text scrolls once the caret would sit past the right edge.
fn visible_window(value: &str, caret: usize, width: usize) -> (String, usize) {
    let characters = value.chars().collect::<Vec<char>>();
    let caret = caret.min(characters.len());
    let head = characters[..caret].iter().collect::<String>();
    let mut window = visible_tail(&head, width);
    let column = display_width(&window);
    let mut used = column;
    for character in &characters[caret..] {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            break;
        }
        used += character_width;
        window.push(*character);
    }
    (window, column)
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

pub(super) fn scrolling_rows(
    area: Rect,
    selected: usize,
    row_count: usize,
    row_height: u16,
) -> Vec<(usize, Rect)> {
    if row_height == 0 || row_count == 0 {
        return Vec::new();
    }
    let visible = usize::from(area.height / row_height).min(row_count);
    if visible == 0 {
        return Vec::new();
    }
    let selected = selected.min(row_count - 1);
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(row_count - visible);
    (start..start + visible)
        .enumerate()
        .map(|(offset, index)| {
            (
                index,
                Rect {
                    x: area.x,
                    y: area.y.saturating_add(
                        u16::try_from(offset)
                            .unwrap_or(u16::MAX)
                            .saturating_mul(row_height),
                    ),
                    width: area.width,
                    height: row_height,
                },
            )
        })
        .collect()
}

pub(super) fn draw_row_scroll_indicators(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[(usize, Rect)],
    row_count: usize,
) {
    let has_rows_above = rows.first().is_some_and(|(index, _)| *index > 0);
    let has_rows_below = rows
        .last()
        .is_some_and(|(index, _)| index.saturating_add(1) < row_count);
    draw_scroll_indicators(frame, area, has_rows_above, has_rows_below);
}

pub(super) fn draw_list_scroll_indicators(
    frame: &mut Frame<'_>,
    area: Rect,
    viewport: Rect,
    state: &ListState,
    item_heights: impl IntoIterator<Item = usize>,
) {
    let offset = state.offset();
    let remaining_height = item_heights
        .into_iter()
        .skip(offset)
        .fold(0_usize, usize::saturating_add);
    draw_scroll_indicators(
        frame,
        area,
        offset > 0,
        remaining_height > usize::from(viewport.height),
    );
}

fn draw_scroll_indicators(
    frame: &mut Frame<'_>,
    area: Rect,
    has_content_above: bool,
    has_content_below: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let x = area.x.saturating_add(area.width.saturating_sub(1) / 2);
    if has_content_above {
        frame.render_widget(
            Paragraph::new("^").style(Style::default().fg(WHITE)),
            Rect::new(x, area.y, 1, 1),
        );
    }
    if has_content_below {
        frame.render_widget(
            Paragraph::new("v").style(Style::default().fg(WHITE)),
            Rect::new(x, area.bottom().saturating_sub(1), 1, 1),
        );
    }
}
