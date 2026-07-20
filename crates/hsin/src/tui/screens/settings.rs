use hsin_core::{ClientKind, LANGUAGE_EN_US, LANGUAGE_ZH_CN};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
    },
};

use crate::i18n::I18n;

use super::super::{
    state::{SettingsPage, SettingsScreen, State},
    theme::{MUTED, RED, WHITE},
    widgets::{centered_fixed, content_width, display_width},
};

pub(super) fn draw_confirm(frame: &mut Frame<'_>, area: Rect, i18n: &I18n) {
    let width = content_width(area, display_width(i18n.text("confirm_delete")) + 4, 30, 72);
    let popup = centered_fixed(area, width, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(i18n.text("confirm_delete"))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(i18n.text("confirm"))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(RED)),
            ),
        popup,
    );
}

#[allow(clippy::too_many_lines)]
pub(super) fn draw_settings_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &State,
    screen: &SettingsScreen,
    i18n: &I18n,
) {
    let proxy = if state.proxy_enabled {
        i18n.text("enabled")
    } else {
        i18n.text("disabled")
    };
    let proxy_switch = if state.proxy_enabled {
        i18n.text("on")
    } else {
        i18n.text("off")
    };
    let language = match state.language.as_str() {
        LANGUAGE_EN_US => i18n.text("language_en_us"),
        LANGUAGE_ZH_CN => i18n.text("language_zh_cn"),
        _ => i18n.text("language_system"),
    };
    let visible_clients = format!(
        "{} / {}",
        state.client_settings.visible.len(),
        ClientKind::ALL.len()
    );
    let client_order = state
        .client_settings
        .order
        .iter()
        .map(|client| client_label(*client, i18n))
        .collect::<Vec<_>>()
        .join(" → ");
    let auth_warning = matches!(&screen.page, SettingsPage::ClientConfig { selected: 0, .. });

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    let option_width = usize::from(columns[0].width.saturating_sub(4));
    let (items, selected, list_title, detail_title, description, current) = match &screen.page {
        SettingsPage::Root => {
            let items = vec![
                settings_option_item(i18n.text("proxy_master"), proxy, option_width),
                ListItem::new(i18n.text("client_configuration")),
                settings_option_item(i18n.text("language"), language, option_width),
            ];
            match screen.selected {
                0 => (
                    items,
                    screen.selected,
                    i18n.text("settings_options"),
                    i18n.text("proxy_master"),
                    i18n.text("settings_proxy_master_description"),
                    Some(proxy),
                ),
                1 => (
                    items,
                    screen.selected,
                    i18n.text("settings_options"),
                    i18n.text("client_configuration"),
                    i18n.text("settings_client_configuration_description"),
                    None,
                ),
                2 => (
                    items,
                    screen.selected,
                    i18n.text("settings_options"),
                    i18n.text("language"),
                    i18n.text("settings_language_description"),
                    Some(language),
                ),
                _ => unreachable!("root settings selection is bounded"),
            }
        }
        SettingsPage::Proxy { selected, port, .. } => {
            let (detail_title, description, current) = match *selected {
                0 => (
                    i18n.text("proxy_switch"),
                    i18n.text("settings_proxy_master_description"),
                    proxy_switch,
                ),
                1 => (
                    i18n.text("proxy_address"),
                    i18n.text("settings_proxy_address_description"),
                    state.proxy_host.as_str(),
                ),
                _ => (
                    i18n.text("proxy_port"),
                    i18n.text("settings_proxy_port_description"),
                    port.as_str(),
                ),
            };
            (
                vec![
                    settings_option_item(i18n.text("proxy_switch"), proxy_switch, option_width),
                    settings_option_item(
                        i18n.text("proxy_address"),
                        &state.proxy_host,
                        option_width,
                    ),
                    settings_option_item(i18n.text("proxy_port"), port, option_width),
                ],
                *selected,
                i18n.text("proxy_master"),
                detail_title,
                description,
                Some(current),
            )
        }
        SettingsPage::Language { selected } => {
            let (detail_title, description) = match *selected {
                0 => (
                    i18n.text("language_system"),
                    i18n.text("settings_system_description"),
                ),
                1 => (
                    i18n.text("language_en_us"),
                    i18n.text("settings_en_us_description"),
                ),
                _ => (
                    i18n.text("language_zh_cn"),
                    i18n.text("settings_zh_cn_description"),
                ),
            };
            (
                vec![
                    ListItem::new(i18n.text("language_system")),
                    ListItem::new(i18n.text("language_en_us")),
                    ListItem::new(i18n.text("language_zh_cn")),
                ],
                *selected,
                i18n.text("language"),
                detail_title,
                description,
                Some(language),
            )
        }
        SettingsPage::Clients { selected } => {
            let (detail_title, description, current) = match *selected {
                0 => (
                    i18n.text("codex_configuration"),
                    i18n.text("settings_codex_configuration_description"),
                    None,
                ),
                1 => (
                    i18n.text("claude_configuration"),
                    i18n.text("settings_claude_configuration_description"),
                    None,
                ),
                2 => (
                    i18n.text("client_visibility"),
                    i18n.text("settings_client_visibility_description"),
                    Some(visible_clients.as_str()),
                ),
                _ => (
                    i18n.text("client_order"),
                    i18n.text("settings_client_order_description"),
                    Some(client_order.as_str()),
                ),
            };
            (
                vec![
                    ListItem::new(i18n.text("codex_configuration")),
                    ListItem::new(i18n.text("claude_configuration")),
                    settings_option_item(
                        i18n.text("client_visibility"),
                        &visible_clients,
                        option_width,
                    ),
                    ListItem::new(i18n.text("client_order")),
                ],
                *selected,
                i18n.text("client_configuration"),
                detail_title,
                description,
                current,
            )
        }
        SettingsPage::ClientConfig { client, selected } => {
            let disabled = state.client_auth.disable_custom_auth(*client);
            let (detail_title, description, current) = if *selected == 0 {
                (
                    i18n.text("disable_custom_auth"),
                    i18n.text("settings_disable_custom_auth_description"),
                    Some(if disabled {
                        i18n.text("on")
                    } else {
                        i18n.text("off")
                    }),
                )
            } else {
                (
                    i18n.text("import_current"),
                    i18n.text("settings_import_current_description"),
                    None,
                )
            };
            (
                vec![
                    settings_option_item(
                        i18n.text("disable_custom_auth"),
                        if disabled {
                            i18n.text("on")
                        } else {
                            i18n.text("off")
                        },
                        option_width,
                    ),
                    ListItem::new(i18n.text("import_current")),
                ],
                *selected,
                client_config_label(*client, i18n),
                detail_title,
                description,
                current,
            )
        }
        SettingsPage::ClientVisibility { selected } => {
            let selected_client = state.client_settings.order[*selected];
            let selected_visible = state.client_settings.visible.contains(&selected_client);
            (
                state
                    .client_settings
                    .order
                    .iter()
                    .map(|client| {
                        settings_option_item(
                            client_label(*client, i18n),
                            if state.client_settings.visible.contains(client) {
                                i18n.text("on")
                            } else {
                                i18n.text("off")
                            },
                            option_width,
                        )
                    })
                    .collect(),
                *selected,
                i18n.text("client_visibility"),
                client_label(selected_client, i18n),
                i18n.text("settings_client_visibility_item_description"),
                Some(if selected_visible {
                    i18n.text("on")
                } else {
                    i18n.text("off")
                }),
            )
        }
        SettingsPage::ClientOrder {
            selected,
            order,
            moving,
        } => (
            order
                .iter()
                .enumerate()
                .map(|(index, client)| {
                    ListItem::new(format!("{}. {}", index + 1, client_label(*client, i18n)))
                })
                .collect(),
            *selected,
            i18n.text("client_order"),
            client_label(order[*selected], i18n),
            i18n.text(if *moving {
                "settings_client_order_moving_description"
            } else {
                "settings_client_order_item_description"
            }),
            None,
        ),
    };
    let mut list_state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .highlight_symbol("› ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(
            Style::default()
                .fg(RED)
                .bg(Color::Rgb(55, 28, 32))
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(list_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)),
        );
    frame.render_stateful_widget(list, columns[0], &mut list_state);

    frame.render_widget(
        Paragraph::new(
            [
                Line::from(Span::styled(
                    detail_title,
                    Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                if auth_warning {
                    auth_warning_line(i18n)
                } else {
                    Line::from(description)
                },
            ]
            .into_iter()
            .chain(current.into_iter().flat_map(|current| {
                [
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(
                            format!("{}: ", i18n.text("settings_current")),
                            Style::default().fg(MUTED),
                        ),
                        Span::styled(current, Style::default().fg(RED)),
                    ]),
                ]
            }))
            .collect::<Vec<_>>(),
        )
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(i18n.text("settings_description"))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)),
        ),
        columns[1],
    );
}

fn client_label(client: ClientKind, i18n: &I18n) -> &str {
    match client {
        ClientKind::Codex => i18n.text("codex"),
        ClientKind::Claude => i18n.text("claude"),
    }
}

fn client_config_label(client: ClientKind, i18n: &I18n) -> &str {
    match client {
        ClientKind::Codex => i18n.text("codex_configuration"),
        ClientKind::Claude => i18n.text("claude_configuration"),
    }
}

fn auth_warning_line(i18n: &I18n) -> Line<'_> {
    Line::from(vec![
        Span::raw(i18n.text("settings_disable_custom_auth_warning_prefix")),
        Span::styled(
            i18n.text("settings_disable_custom_auth_warning_red"),
            Style::default().fg(RED),
        ),
        Span::raw(i18n.text("settings_disable_custom_auth_warning_suffix")),
    ])
}

fn settings_option_item(label: &str, current: &str, width: usize) -> ListItem<'static> {
    let current = format!("[{current}]");
    let padding = width
        .saturating_sub(display_width(label) + display_width(&current))
        .max(1);
    ListItem::new(format!("{label}{}{current}", " ".repeat(padding)))
}
