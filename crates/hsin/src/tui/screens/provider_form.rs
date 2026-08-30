use hsin_core::{
    AuthScheme, ClientKind, OPENAI_CODEX_CONFIG_NAME, ProviderProxyMode, ProviderScope,
    ProxyProtocol,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::i18n::I18n;

use super::super::{
    state::{
        ProviderForm, form_auth_field, form_description_field, form_field_count, form_image_field,
        form_network_proxy_field, form_proxy_host_field, form_proxy_password_field,
        form_proxy_port_field, form_proxy_protocol_field, form_proxy_username_field,
        primary_codex_form,
    },
    theme::{INPUT_BG, RED, WHITE},
    widgets::{
        centered_fixed, content_width, display_width, draw_input_field, draw_row_scroll_indicators,
        scrolling_rows,
    },
};

const FORM_FIELD_HEIGHT: u16 = 3;

struct FormValues<'a> {
    secret: &'a str,
    secret_placeholder: &'a str,
    auth: &'static str,
    remote_compaction: String,
    image_generation: String,
    network_proxy: String,
    proxy_protocol: &'static str,
    proxy_password: String,
    proxy_password_placeholder: &'a str,
}

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
    let hidden_proxy_password = "•".repeat(form.proxy_password.chars().count());
    let proxy_password = if form.secret_visible {
        form.proxy_password.to_string()
    } else {
        hidden_proxy_password
    };
    let proxy_password_placeholder = if form.proxy_password_clear {
        i18n.text("proxy_password_clear_hint")
    } else if form.id.is_some() && form.network_proxy.manual.password_configured {
        i18n.text("api_key_preserve_hint")
    } else {
        i18n.text("optional")
    };
    let auth = match form.auth_scheme {
        AuthScheme::Bearer => "‹ Bearer ›",
        AuthScheme::XApiKey => "‹ X-API-Key ›",
        AuthScheme::OAuth => "‹ OAuth ›",
    };
    let remote_compaction = if form.codex_config_name.trim() == OPENAI_CODEX_CONFIG_NAME {
        format!("‹ {} ›", i18n.text("enabled"))
    } else {
        format!("‹ {} ›", i18n.text("disabled"))
    };
    let values = FormValues {
        secret,
        secret_placeholder,
        auth,
        remote_compaction,
        image_generation: if form.codex_image.enabled {
            format!("‹ {} ›", i18n.text("enabled"))
        } else {
            format!("‹ {} ›", i18n.text("disabled"))
        },
        network_proxy: format!(
            "‹ {} ›",
            match form.network_proxy.mode {
                ProviderProxyMode::Inherit => i18n.text("upstream_proxy_inherit"),
                ProviderProxyMode::Direct => i18n.text("upstream_proxy_direct"),
                ProviderProxyMode::System => i18n.text("upstream_proxy_system"),
                ProviderProxyMode::Manual => i18n.text("upstream_proxy_manual"),
            }
        ),
        proxy_protocol: match form.network_proxy.manual.protocol {
            ProxyProtocol::Http => "‹ HTTP ›",
            ProxyProtocol::Socks5 => "‹ SOCKS5 ›",
        },
        proxy_password,
        proxy_password_placeholder,
    };
    let field_count = form_field_count(form);
    let popup = form_popup(area, form, &values, field_count);
    frame.render_widget(Clear, popup);
    let title = form_title(form, i18n);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED));
    let inner = block.inner(popup);
    let fields = form_field_areas(inner, form.field, field_count);
    frame.render_widget(block, popup);
    for &(index, field) in &fields {
        draw_form_field(frame, field, index, form, i18n, &values);
    }
    draw_row_scroll_indicators(frame, popup, &fields, field_count);
}

fn form_popup(
    area: Rect,
    form: &ProviderForm,
    values: &FormValues<'_>,
    field_count: usize,
) -> Rect {
    let longest = [
        display_width(&form.name),
        display_width(&form.codex_config_name),
        display_width(&form.description),
        display_width(&form.base_url),
        display_width(values.secret),
        display_width(values.auth),
        display_width(&values.remote_compaction),
        display_width(&values.image_generation),
        display_width(&values.network_proxy),
        display_width(&form.network_proxy.manual.host),
        display_width(&form.network_proxy.manual.username),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
    .saturating_add(6);
    let proportional_width = area.width.saturating_mul(4) / 5;
    let width = content_width(area, longest, 64, 100)
        .max(proportional_width)
        .min(area.width);
    let popup_height = u16::try_from(field_count)
        .unwrap_or(u16::MAX)
        .saturating_mul(FORM_FIELD_HEIGHT)
        .saturating_add(2);
    centered_fixed(area, width, popup_height)
}

#[allow(clippy::too_many_lines)]
fn draw_form_field(
    frame: &mut Frame<'_>,
    area: Rect,
    index: usize,
    form: &ProviderForm,
    i18n: &I18n,
    values: &FormValues<'_>,
) {
    match index {
        0 => draw_input_field(
            frame,
            area,
            i18n.text("base_url"),
            &form.base_url,
            Some(match form.client {
                ClientKind::Codex => "https://api.openai.com/v1",
                ClientKind::Claude => "https://api.anthropic.com",
            }),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        1 => draw_input_field(
            frame,
            area,
            i18n.text("api_key"),
            values.secret,
            Some(values.secret_placeholder),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        2 => draw_input_field(
            frame,
            area,
            i18n.text("name"),
            &form.name,
            Some(i18n.text("name_domain_hint")),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        3 if primary_codex_form(form) => draw_input_field(
            frame,
            area,
            i18n.text("config_name"),
            &form.codex_config_name,
            Some(i18n.text("config_name_hint")),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        4 if primary_codex_form(form) => draw_choice_field(
            frame,
            area,
            i18n.text("remote_compaction"),
            &values.remote_compaction,
            form.field == index,
        ),
        index if form_image_field(form) == Some(index) => draw_choice_field(
            frame,
            area,
            i18n.text("configure_image_generation"),
            &values.image_generation,
            form.field == index,
        ),
        index if index == form_network_proxy_field(form) => draw_choice_field(
            frame,
            area,
            i18n.text("upstream_proxy"),
            &values.network_proxy,
            form.field == index,
        ),
        index if form_proxy_protocol_field(form) == Some(index) => draw_choice_field(
            frame,
            area,
            i18n.text("proxy_protocol"),
            values.proxy_protocol,
            form.field == index,
        ),
        index if form_proxy_host_field(form) == Some(index) => draw_input_field(
            frame,
            area,
            i18n.text("proxy_address"),
            &form.network_proxy.manual.host,
            Some("127.0.0.1"),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        index if form_proxy_port_field(form) == Some(index) => draw_input_field(
            frame,
            area,
            i18n.text("proxy_port"),
            &form.proxy_port,
            Some("7890"),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        index if form_proxy_username_field(form) == Some(index) => draw_input_field(
            frame,
            area,
            i18n.text("proxy_username"),
            &form.network_proxy.manual.username,
            Some(i18n.text("optional")),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        index if form_proxy_password_field(form) == Some(index) => draw_input_field(
            frame,
            area,
            i18n.text("proxy_password"),
            &values.proxy_password,
            Some(values.proxy_password_placeholder),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        index if index == form_description_field(form) => draw_input_field(
            frame,
            area,
            i18n.text("description"),
            &form.description,
            Some(i18n.text("description_hint")),
            (form.field == index).then_some(form.cursor),
            true,
        ),
        index if index == form_auth_field(form) => draw_choice_field(
            frame,
            area,
            i18n.text("auth"),
            values.auth,
            form.field == index,
        ),
        _ => {}
    }
}

fn draw_choice_field(frame: &mut Frame<'_>, area: Rect, label: &str, value: &str, selected: bool) {
    let foreground = if selected { RED } else { WHITE };
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

fn form_title(form: &ProviderForm, i18n: &I18n) -> String {
    let title = if form.scope == ProviderScope::ImageOnly && form.id.is_some() {
        i18n.text("edit_image_provider")
    } else if form.scope == ProviderScope::ImageOnly {
        i18n.text("add_image_provider")
    } else if form.id.is_some() {
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

pub(in crate::tui) fn form_field_areas(
    area: Rect,
    selected: usize,
    field_count: usize,
) -> Vec<(usize, Rect)> {
    scrolling_rows(area, selected, field_count, FORM_FIELD_HEIGHT)
}
