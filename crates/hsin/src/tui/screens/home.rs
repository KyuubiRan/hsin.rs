use hsin_core::{AuthScheme, ConnectionMode};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::i18n::I18n;

use super::super::{
    state::{InputMode, MAPPING_TIERS, State},
    theme::{MUTED, RED, WHITE},
    widgets::draw_input_field,
};

pub(super) fn draw_provider_list(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut State,
    i18n: &I18n,
) {
    let active = state.active_id().map(str::to_owned);
    let searching = !state.active_query().is_empty();
    let providers = state.visible_providers();
    let items = if providers.is_empty() {
        vec![ListItem::new(if state.loading {
            i18n.text("loading")
        } else if searching {
            i18n.text("no_search_results")
        } else {
            i18n.text("no_providers")
        })]
    } else {
        providers
            .iter()
            .map(|provider| {
                let marker = if active.as_deref() == Some(provider.id.as_str()) {
                    "●"
                } else {
                    "○"
                };
                let name = if provider.official {
                    i18n.text("official")
                } else {
                    &provider.name
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} "), Style::default().fg(RED)),
                    Span::styled(name, Style::default().fg(WHITE)),
                    Span::styled(
                        if model_mapping_active(provider) {
                            format!(" {}", i18n.text("model_mapping_badge"))
                        } else {
                            String::new()
                        },
                        Style::default().fg(RED),
                    ),
                    Span::styled(
                        format!("\n   {}", provider.base_url),
                        Style::default().fg(MUTED),
                    ),
                ]))
            })
            .collect()
    };
    let mut list_state = ListState::default().with_selected(Some(state.selected));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(55, 28, 32))
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(i18n.text("provider"))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)),
        );
    frame.render_stateful_widget(list, area, &mut list_state);
}

pub(super) fn draw_details(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let Some(provider) = state.selected_provider() else {
        frame.render_widget(
            Paragraph::new(i18n.text("select_provider_details")).block(
                Block::default()
                    .title(i18n.text("status"))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(MUTED)),
            ),
            area,
        );
        return;
    };
    let name = if provider.official {
        i18n.text("official")
    } else {
        &provider.name
    };
    let description = if provider.official {
        match provider.client {
            hsin_core::ClientKind::Codex => i18n.text("official_codex_description"),
            hsin_core::ClientKind::Claude => i18n.text("official_claude_description"),
        }
    } else if provider.description.is_empty() {
        i18n.text("none")
    } else {
        &provider.description
    };
    let credential = match provider.auth_scheme {
        AuthScheme::OAuth => i18n.text("oauth"),
        AuthScheme::Bearer | AuthScheme::XApiKey if provider.credential_configured => {
            provider.credential_preview.as_deref().unwrap_or("••••••••")
        }
        AuthScheme::Bearer | AuthScheme::XApiKey => i18n.text("credential_missing"),
    };
    let auth = match provider.auth_scheme {
        AuthScheme::Bearer => "Bearer",
        AuthScheme::XApiKey => "X-API-Key",
        AuthScheme::OAuth => i18n.text("oauth"),
    };
    let proxy = if state.mode() == ConnectionMode::Proxy && state.proxy_enabled {
        i18n.text("enabled")
    } else {
        i18n.text("disabled")
    };
    let mut lines = vec![
        Line::from(Span::styled(
            name,
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        detail_line(i18n.text("base_url"), &provider.base_url),
        detail_line(i18n.text("api_key"), credential),
        detail_line(i18n.text("description"), description),
        detail_line(i18n.text("auth_type"), auth),
        detail_line(i18n.text("tool_proxy"), proxy),
    ];
    let mapping = model_mapping_summary(provider, i18n);
    if let Some(mapping) = &mapping {
        lines.push(detail_line(i18n.text("model_mapping"), &mapping.state));
        // One tier per line: the mapped IDs are long enough that a joined list wraps mid-name.
        lines.extend(mapping.tiers.iter().map(|tier| {
            Line::from(Span::styled(
                format!("  {tier}"),
                Style::default().fg(WHITE),
            ))
        }));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(i18n.text("status"))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)),
        ),
        area,
    );
}

fn detail_line<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(MUTED)),
        Span::styled(value, Style::default().fg(WHITE)),
    ])
}

/// Whether this provider actually writes model-mapping keys into `settings.json`.
fn model_mapping_active(provider: &hsin_core::Provider) -> bool {
    !provider.official
        && provider
            .claude_model_mapping
            .as_ref()
            .is_some_and(|mapping| !mapping.is_inert())
}

/// The mapping status shown in the details pane: `None` for clients that have no mapping at all,
/// otherwise on/off plus one entry per tier that is written.
struct ModelMappingSummary {
    state: String,
    tiers: Vec<String>,
}

fn model_mapping_summary(
    provider: &hsin_core::Provider,
    i18n: &I18n,
) -> Option<ModelMappingSummary> {
    if provider.client != hsin_core::ClientKind::Claude || provider.official {
        return None;
    }
    if !model_mapping_active(provider) {
        return Some(ModelMappingSummary {
            state: i18n.text("disabled").to_owned(),
            tiers: Vec::new(),
        });
    }
    let mapping = provider.claude_model_mapping.as_ref()?;
    // The session default leads: it is what a fresh Claude Code run actually starts on.
    let default_model = mapping
        .default_model
        .as_deref()
        .map(|model| format!("{}: {model}", i18n.text("model_mapping_default")));
    let tiers = default_model
        .into_iter()
        .chain(
            MAPPING_TIERS
                .iter()
                .zip(mapping.slots())
                .filter_map(|(tier, (_, slot))| {
                    slot.map(|slot| format!("{} → {}", tier.label, slot.resolved_model()))
                }),
        )
        .collect();
    Some(ModelMappingSummary {
        state: i18n.text("enabled").to_owned(),
        tiers,
    })
}

pub(super) fn draw_search(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let cursor = match &state.input {
        InputMode::Search { cursor, .. } => Some(*cursor),
        _ => None,
    };
    draw_input_field(
        frame,
        area,
        i18n.text("search"),
        state.active_query(),
        None,
        cursor,
        true,
    );
}
