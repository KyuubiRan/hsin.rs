use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hsin_core::{
    AuthScheme, ClaudeModelMapping, ClaudeModelMappingUpdate, ClientKind, ClientSettings,
    LANGUAGE_EN_US, LANGUAGE_SYSTEM, LANGUAGE_ZH_CN, ModelSlot, ModelUpdate, Provider, Settings,
};
use ratatui::{
    backend::TestBackend,
    layout::Rect,
    widgets::{Block, Borders},
};
use zeroize::Zeroizing;

use super::{
    screens::{TITLE, VERSION_LABEL, form_field_areas},
    state::{
        FormSubmission, InputMode, ModelPicker, ModelPickerMode, ProviderClipboard, ProviderForm,
        SettingsPage, SettingsScreen, take_form_submission,
    },
    theme::{INPUT_BG, RED, WHITE},
    widgets::centered_fixed,
};

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, crossterm::event::KeyModifiers::NONE))
}

fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> Action {
    Action::Key(KeyEvent::new(code, modifiers))
}

#[test]
fn a_held_key_keeps_deleting_instead_of_stalling_after_one_character() {
    // Terminals speaking the kitty protocol report auto-repeat as its own event kind, so a held
    // backspace only reaches the reducer if repeats are accepted alongside presses.
    let repeat = Event::Key(KeyEvent {
        kind: crossterm::event::KeyEventKind::Repeat,
        ..KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
    });
    assert!(super::key_action(&repeat).is_some());

    let release = Event::Key(KeyEvent {
        kind: crossterm::event::KeyEventKind::Release,
        ..KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
    });
    assert!(super::key_action(&release).is_none());
}

fn submission() -> FormSubmission {
    FormSubmission {
        id: None,
        revision: None,
        client: ClientKind::Codex,
        name: "Example".into(),
        description: "note".into(),
        base_url: "https://api.example.test/v1".into(),
        auth_scheme: AuthScheme::Bearer,
        secret: Zeroizing::new("secret".into()),
        model: ModelUpdate::Preserve,
        claude_model_mapping: ClaudeModelMappingUpdate::Preserve,
    }
}

fn example_provider() -> Provider {
    Provider {
        id: String::from("provider-1"),
        client: ClientKind::Codex,
        name: String::from("Example"),
        description: String::from("Primary provider"),
        base_url: String::from("https://api.example.test/v1"),
        auth_scheme: AuthScheme::Bearer,
        official: false,
        credential_configured: true,
        credential_preview: Some(String::from("sk-abc***de")),
        model: Some(String::from("gpt-5")),
        claude_model_mapping: None,
        revision: 1,
    }
}

#[test]
fn reducer_switches_client_and_moves_selection() {
    let mut state = State::default();
    assert_eq!(state.language, LANGUAGE_SYSTEM);
    state.reduce(key(KeyCode::Tab));
    assert_eq!(state.client, ClientKind::Claude);
    assert_eq!(state.selected, 0);
    state.reduce(key(KeyCode::BackTab));
    assert_eq!(state.client, ClientKind::Codex);
    state.reduce(modified_key(KeyCode::Tab, KeyModifiers::SHIFT));
    assert_eq!(state.client, ClientKind::Claude);
}

#[test]
fn a_client_opens_on_its_active_provider_and_resumes_where_it_was_left() {
    // Row 0 is the official provider, which is rarely the one actually in use, so both the first
    // load and a first visit to a client aim at whatever that client has active.
    let mut codex_official = example_provider();
    codex_official.id = "official-codex".into();
    codex_official.official = true;
    let mut claude_official = example_provider();
    claude_official.id = "official-claude".into();
    claude_official.client = ClientKind::Claude;
    claude_official.official = true;
    let mut claude_custom = example_provider();
    claude_custom.id = "provider-2".into();
    claude_custom.client = ClientKind::Claude;

    let mut state = State::default();
    state.reduce(Action::Loaded {
        providers: vec![
            codex_official,
            example_provider(),
            claude_official,
            claude_custom,
        ],
        status: crate::rpc::StatusSnapshot {
            codex_active_provider: Some("provider-1".into()),
            claude_active_provider: Some("provider-2".into()),
            ..crate::rpc::StatusSnapshot::default()
        },
        settings: Settings::default(),
    });
    assert_eq!(state.selected, 1);

    state.reduce(key(KeyCode::Up));
    assert_eq!(state.selected, 0);
    state.reduce(key(KeyCode::Tab));
    assert_eq!(state.client, ClientKind::Claude);
    assert_eq!(state.selected, 1);

    state.reduce(key(KeyCode::Tab));
    assert_eq!(state.client, ClientKind::Codex);
    assert_eq!(state.selected, 0);
}

#[test]
fn client_switching_follows_visibility_and_configured_order() {
    let mut state = State {
        client: ClientKind::Claude,
        client_settings: ClientSettings {
            order: vec![ClientKind::Claude, ClientKind::Codex],
            visible: vec![ClientKind::Claude, ClientKind::Codex],
        },
        ..State::default()
    };
    state.reduce(key(KeyCode::Tab));
    assert_eq!(state.client, ClientKind::Codex);
    state.reduce(key(KeyCode::BackTab));
    assert_eq!(state.client, ClientKind::Claude);

    state.client_settings.visible = vec![ClientKind::Claude];
    state.reduce(key(KeyCode::Tab));
    assert_eq!(state.client, ClientKind::Claude);
}

#[test]
fn loaded_settings_switch_away_from_a_hidden_current_client() {
    let mut state = State::default();
    state.reduce(Action::Loaded {
        providers: Vec::new(),
        status: crate::rpc::StatusSnapshot::default(),
        settings: Settings {
            clients: ClientSettings {
                order: vec![ClientKind::Codex, ClientKind::Claude],
                visible: vec![ClientKind::Claude],
            },
            ..Settings::default()
        },
    });
    assert_eq!(state.client, ClientKind::Claude);
}

#[test]
fn client_visibility_keeps_at_least_one_client_enabled() {
    let mut state = State {
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::ClientVisibility { selected: 0 },
        }),
        ..State::default()
    };
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetClients(ClientSettings { visible, .. }))
            if visible == [ClientKind::Claude]
    ));

    state.client_settings.visible = vec![ClientKind::Claude];
    state.input = InputMode::Settings(SettingsScreen {
        selected: 1,
        page: SettingsPage::ClientVisibility { selected: 1 },
    });
    state.reduce(key(KeyCode::Enter));
    assert!(state.take_effect().is_none());
    assert_eq!(state.notice.as_deref(), Some("@client_visibility_minimum"));
}

#[test]
fn client_order_moves_the_selected_client_before_saving() {
    let mut state = State {
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::ClientOrder {
                selected: 1,
                order: ClientKind::ALL.to_vec(),
                moving: false,
            },
        }),
        ..State::default()
    };
    state.reduce(key(KeyCode::Enter));
    state.reduce(key(KeyCode::Up));
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetClients(ClientSettings { order, visible }))
            if order == [ClientKind::Claude, ClientKind::Codex]
                && visible == [ClientKind::Claude, ClientKind::Codex]
    ));
}

#[test]
fn client_configuration_toggles_custom_auth_per_client() {
    let mut state = State {
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::Clients { selected: 0 },
        }),
        ..State::default()
    };
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        &state.input,
        InputMode::Settings(SettingsScreen {
            page: SettingsPage::ClientConfig {
                client: ClientKind::Codex,
                selected: 0,
            },
            ..
        })
    ));
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetClientAuth {
            client: ClientKind::Codex,
            disable_custom_auth: true,
        })
    ));

    state.client_auth.codex_disable_custom_auth = true;
    state.loading = false;
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetClientAuth {
            client: ClientKind::Codex,
            disable_custom_auth: false,
        })
    ));
}

#[test]
fn home_p_toggles_the_current_client_proxy_mode() {
    let mut state = State::default();
    state.status.codex_active_provider = Some("provider-1".into());
    state.reduce(key(KeyCode::Char('p')));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetMode {
            client: ClientKind::Codex,
            mode: hsin_core::ConnectionMode::Proxy,
        })
    ));
    state.status.codex_mode = hsin_core::ConnectionMode::Proxy;
    state.reduce(key(KeyCode::Char('p')));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetMode {
            client: ClientKind::Codex,
            mode: hsin_core::ConnectionMode::Direct,
        })
    ));
}

#[test]
fn home_p_without_a_provider_shows_a_localized_notice_without_rpc() {
    let mut state = State::default();
    state.reduce(key(KeyCode::Char('p')));
    assert!(state.take_effect().is_none());
    assert_eq!(state.notice.as_deref(), Some("@proxy_requires_provider"));
}

#[test]
fn home_c_queues_a_revision_bound_provider_copy() {
    let provider = example_provider();
    let mut state = State {
        providers: vec![provider.clone()],
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('c')));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::CopyProvider(copied))
            if copied.id == provider.id && copied.revision == provider.revision
    ));
}

#[test]
fn home_v_pastes_a_same_client_copy_as_an_editable_add_form() {
    let provider = example_provider();
    let mut state = State {
        providers: vec![provider.clone()],
        loading: false,
        ..State::default()
    };
    state.reduce(Action::ProviderCopied(ProviderClipboard {
        provider,
        secret: Zeroizing::new("copied-secret".into()),
    }));
    state.reduce(key(KeyCode::Char('v')));
    {
        let InputMode::Form(form) = &state.input else {
            panic!("paste must open an add form");
        };
        assert!(form.id.is_none());
        assert_eq!(form.client, ClientKind::Codex);
        assert_eq!(form.name, "Example copy");
        assert_eq!(form.base_url, "https://api.example.test/v1");
        assert_eq!(form.auth_scheme, AuthScheme::Bearer);
        assert!(form.secret.is_empty());
        assert_eq!(
            form.copied_secret.as_deref().map(String::as_str),
            Some("copied-secret")
        );
    }
    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw pasted provider form");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("<unchanged>"));

    let InputMode::Form(form) = &mut state.input else {
        panic!("paste form must remain open");
    };
    let submission = take_form_submission(form).unwrap();
    assert_eq!(submission.secret.as_str(), "copied-secret");
}

#[test]
fn home_v_converts_a_codex_provider_for_claude() {
    let provider = example_provider();
    let mut state = State {
        client: ClientKind::Claude,
        providers: vec![provider.clone()],
        loading: false,
        ..State::default()
    };
    state.reduce(Action::ProviderCopied(ProviderClipboard {
        provider,
        secret: Zeroizing::new("copied-secret".into()),
    }));
    state.reduce(key(KeyCode::Char('v')));
    assert!(matches!(
        &state.input,
        InputMode::Form(form)
            if form.id.is_none()
                && form.client == ClientKind::Claude
                && form.name == "Example copy"
                && form.base_url == "https://api.example.test"
                && form.auth_scheme == AuthScheme::XApiKey
                && form.copied_secret.is_some()
    ));
}

#[test]
fn home_v_converts_a_claude_provider_for_codex() {
    let mut provider = example_provider();
    provider.client = ClientKind::Claude;
    provider.base_url = "https://api.example.test".into();
    provider.auth_scheme = AuthScheme::XApiKey;
    let mut state = State {
        providers: vec![provider.clone()],
        loading: false,
        ..State::default()
    };
    state.reduce(Action::ProviderCopied(ProviderClipboard {
        provider,
        secret: Zeroizing::new("copied-secret".into()),
    }));
    state.reduce(key(KeyCode::Char('v')));
    assert!(matches!(
        &state.input,
        InputMode::Form(form)
            if form.client == ClientKind::Codex
                && form.base_url == "https://api.example.test/v1"
                && form.auth_scheme == AuthScheme::Bearer
    ));
}

#[test]
fn pasted_api_key_can_be_replaced_before_submission() {
    let provider = example_provider();
    let mut state = State {
        providers: vec![provider.clone()],
        loading: false,
        ..State::default()
    };
    state.reduce(Action::ProviderCopied(ProviderClipboard {
        provider,
        secret: Zeroizing::new("copied-secret".into()),
    }));
    state.reduce(key(KeyCode::Char('v')));
    state.reduce(key(KeyCode::Down));
    for character in "replacement-secret".chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
    let InputMode::Form(form) = &mut state.input else {
        panic!("paste must open an add form");
    };
    let submission = take_form_submission(form).unwrap();
    assert_eq!(submission.secret.as_str(), "replacement-secret");
}

#[test]
fn official_provider_copy_is_rejected_without_resolving_a_credential() {
    let mut provider = example_provider();
    provider.official = true;
    provider.auth_scheme = AuthScheme::OAuth;
    let mut state = State {
        providers: vec![provider],
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('c')));
    assert!(state.take_effect().is_none());
    assert_eq!(state.notice.as_deref(), Some("@copy_official_unsupported"));
}

#[test]
fn pasted_provider_names_avoid_target_client_conflicts() {
    let source = example_provider();
    let mut existing = example_provider();
    existing.id = "provider-2".into();
    existing.name = "Example copy".into();
    let mut state = State {
        providers: vec![source.clone(), existing],
        loading: false,
        ..State::default()
    };
    state.reduce(Action::ProviderCopied(ProviderClipboard {
        provider: source,
        secret: Zeroizing::new("copied-secret".into()),
    }));
    state.reduce(key(KeyCode::Char('v')));
    assert!(matches!(&state.input, InputMode::Form(form) if form.name == "Example copy 2"));
}

#[test]
fn settings_menu_queues_proxy_and_language_changes() {
    let mut state = State::default();
    state.reduce(key(KeyCode::Char('o')));
    assert!(matches!(
        &state.input,
        InputMode::Settings(SettingsScreen {
            selected: 0,
            page: SettingsPage::Root,
        })
    ));
    state.reduce(key(KeyCode::Enter));
    assert!(state.take_effect().is_none());
    assert!(matches!(
        &state.input,
        InputMode::Settings(SettingsScreen {
            page: SettingsPage::Proxy { selected: 0, .. },
            ..
        })
    ));
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetProxyEnabled(true))
    ));
    state.reduce(key(KeyCode::Esc));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Enter));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetLanguage(language)) if language == LANGUAGE_ZH_CN
    ));
}

#[test]
fn proxy_settings_edits_address_and_port_while_enabled() {
    let mut state = State {
        proxy_enabled: true,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('o')));
    state.reduce(key(KeyCode::Enter));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Enter));
    for character in "0.0.0.0".chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetProxyHost(host)) if host == "0.0.0.0"
    ));

    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Enter));
    for character in "1234".chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::SetProxyPort(1234))
    ));
}

#[test]
fn client_configuration_imports_the_corresponding_current_provider() {
    for client in ClientKind::ALL {
        let mut state = State {
            input: InputMode::Settings(SettingsScreen {
                selected: match client {
                    ClientKind::Codex => 0,
                    ClientKind::Claude => 1,
                },
                page: SettingsPage::ClientConfig {
                    client,
                    selected: 0,
                },
            }),
            ..State::default()
        };
        state.reduce(key(KeyCode::Down));
        state.reduce(key(KeyCode::Enter));
        assert!(matches!(
            state.take_effect(),
            Some(Effect::ImportCurrent(imported_client)) if imported_client == client
        ));
        assert!(matches!(
            state.input,
            InputMode::Settings(SettingsScreen {
                page: SettingsPage::ClientConfig { selected: 1, .. },
                ..
            })
        ));
    }
}

#[test]
fn loaded_language_is_applied_without_restarting_the_tui() {
    let mut state = State::default();
    let mut locale = I18n::new(Some(LANGUAGE_EN_US));
    reduce_action(
        &mut state,
        &mut locale,
        true,
        Action::Loaded {
            providers: Vec::new(),
            status: crate::rpc::StatusSnapshot::default(),
            settings: Settings {
                language: LANGUAGE_ZH_CN.into(),
                ..Settings::default()
            },
        },
    );
    assert_eq!(locale.text("settings"), "设置");

    let mut state = State::default();
    let mut overridden = I18n::new(Some(LANGUAGE_EN_US));
    reduce_action(
        &mut state,
        &mut overridden,
        false,
        Action::Loaded {
            providers: Vec::new(),
            status: crate::rpc::StatusSnapshot::default(),
            settings: Settings {
                language: LANGUAGE_ZH_CN.into(),
                ..Settings::default()
            },
        },
    );
    assert_eq!(overridden.text("settings"), "Settings");
}

#[test]
fn hidden_ijkl_navigation_works_on_home_and_settings() {
    let mut state = State::default();
    state.reduce(key(KeyCode::Char('l')));
    assert_eq!(state.client, ClientKind::Claude);
    state.reduce(key(KeyCode::Char('j')));
    assert_eq!(state.client, ClientKind::Codex);

    state.reduce(key(KeyCode::Char('o')));
    state.reduce(key(KeyCode::Char('k')));
    state.reduce(key(KeyCode::Char('k')));
    assert!(matches!(
        &state.input,
        InputMode::Settings(SettingsScreen { selected: 2, .. })
    ));
    state.reduce(key(KeyCode::Char('l')));
    assert!(matches!(
        &state.input,
        InputMode::Settings(SettingsScreen {
            page: SettingsPage::Language { .. },
            ..
        })
    ));
    state.reduce(key(KeyCode::Char('j')));
    assert!(matches!(
        &state.input,
        InputMode::Settings(SettingsScreen {
            page: SettingsPage::Root,
            ..
        })
    ));
}

#[test]
fn escape_quits_normal_screen() {
    let mut state = State::default();
    assert_eq!(state.reduce(key(KeyCode::Esc)), Transition::Quit);
}

#[test]
fn add_form_defaults_empty_name_to_base_url_host() {
    let mut form = ProviderForm {
        id: None,
        revision: None,
        client: ClientKind::Codex,
        name: String::new(),
        description: "note".into(),
        base_url: "https://api.example.test/v1".into(),
        auth_scheme: AuthScheme::Bearer,
        secret: Zeroizing::new("secret".into()),
        copied_secret: None,
        field: 0,
        error: None,
        secret_visible: false,
        discovering_models: false,
        claude_model_mapping: None,
    };
    let submission = take_form_submission(&mut form).unwrap();
    assert_eq!(submission.name, "example");

    form.id = Some("provider-1".into());
    form.base_url = "https://edited.example.test/v1".into();
    let submission = take_form_submission(&mut form).unwrap();
    assert_eq!(submission.name, "example");
}

#[test]
fn edit_form_shortens_only_generated_host_names() {
    let mut generated = example_provider();
    generated.name = "ai.router.team".into();
    generated.base_url = "https://ai.router.team/v1".into();
    let mut state = State {
        providers: vec![generated],
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('e')));
    assert!(matches!(&state.input, InputMode::Form(form) if form.name == "router"));

    let mut custom = example_provider();
    custom.name = "My Router".into();
    custom.base_url = "https://ai.router.team/v1".into();
    state.providers = vec![custom];
    state.input = InputMode::Normal;
    state.reduce(key(KeyCode::Char('e')));
    assert!(matches!(&state.input, InputMode::Form(form) if form.name == "My Router"));
}

#[test]
fn form_input_order_starts_with_url_then_api_key() {
    let mut state = State::default();
    state.reduce(key(KeyCode::Char('a')));
    state.reduce(key(KeyCode::Char('u')));
    state.reduce(key(KeyCode::Tab));
    state.reduce(key(KeyCode::Char('k')));
    assert!(matches!(
        &state.input,
        InputMode::Form(form) if form.base_url == "u" && form.secret.as_str() == "k"
    ));
}

#[test]
fn control_u_clears_the_selected_form_field() {
    let mut state = State::default();
    state.reduce(key(KeyCode::Char('a')));
    for character in "https://example.test".chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
    state.reduce(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(matches!(&state.input, InputMode::Form(form) if form.base_url.is_empty()));
}

#[test]
fn control_h_toggles_api_key_visibility() {
    let mut state = State::default();
    state.reduce(key(KeyCode::Char('a')));
    state.reduce(key(KeyCode::Down));
    for character in "sk-test".chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
    state.reduce(modified_key(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert!(matches!(
        &state.input,
        InputMode::Form(form) if form.secret_visible
    ));

    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw revealed API key");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("sk-test"));
    state.reduce(modified_key(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert!(matches!(
        &state.input,
        InputMode::Form(form) if !form.secret_visible
    ));
}

#[test]
fn form_fields_keep_equal_content_height() {
    let popup = centered_fixed(Rect::new(0, 0, 100, 20), 80, 17);
    let inner = Block::default().borders(Borders::ALL).inner(popup);
    let fields = form_field_areas(inner, 0);
    assert_eq!(fields.len(), 5);
    assert!(fields.iter().all(|(_, field)| field.height == 3));
    // The dialog is centered, so the gap above it matches the gap below.
    assert_eq!(popup.y, 1);
    assert_eq!(popup.y + popup.height, 18);
    assert_eq!(popup.width, 80);
}

#[test]
fn dialogs_stay_centered_as_the_terminal_grows() {
    // A taller terminal must keep the dialog centered rather than pinning it to
    // an edge, so the space above and below stays balanced.
    for height in [20, 40, 60] {
        let popup = centered_fixed(Rect::new(0, 0, 120, height), 80, 17);
        let above = popup.y;
        let below = height - (popup.y + popup.height);
        assert!(
            above.abs_diff(below) <= 1,
            "height {height}: {above} above vs {below} below"
        );
        let left = popup.x;
        let right = 120 - (popup.x + popup.width);
        assert!(
            left.abs_diff(right) <= 1,
            "height {height}: {left} left vs {right} right"
        );
    }
}

#[test]
fn short_form_scrolls_without_compressing_fields() {
    let first = form_field_areas(Rect::new(0, 0, 80, 8), 0);
    assert_eq!(
        first.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(first.iter().all(|(_, field)| field.height == 3));

    let last = form_field_areas(Rect::new(0, 0, 80, 8), 4);
    assert_eq!(
        last.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        [3, 4]
    );
    assert!(last.iter().all(|(_, field)| field.height == 3));
}

#[test]
fn short_terminal_scrolls_the_form_to_the_selected_field() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('a')));
    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("short test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw top of short form");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Base URL"));
    assert!(!rendered.contains("Auth"));

    for _ in 0..4 {
        state.reduce(key(KeyCode::Down));
    }
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw bottom of short form");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(!rendered.contains("Base URL"));
    assert!(rendered.contains("Auth"));
}

#[test]
fn model_discovery_keeps_the_provider_dialog_visible_and_locked() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('a')));
    let InputMode::Form(form) = &mut state.input else {
        panic!("add must open the provider form");
    };
    form.base_url = "https://ai.router.team/v1".into();
    form.secret = Zeroizing::new("sk-test-secret".into());

    state.reduce(key(KeyCode::Enter));
    assert!(state.loading);
    assert_eq!(state.notice.as_deref(), Some("@fetching_models"));
    assert!(matches!(
        &state.input,
        InputMode::Form(form)
            if form.discovering_models
                && form.base_url == "https://ai.router.team/v1"
                && form.secret.is_empty()
    ));

    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw model discovery state");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Fetching model list"));
    assert!(rendered.contains("https://ai.router.team/v1"));

    state.reduce(key(KeyCode::Esc));
    assert!(matches!(&state.input, InputMode::Form(form) if form.discovering_models));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::DiscoverModels(FormSubmission { secret, .. }))
            if secret.as_str() == "sk-test-secret"
    ));
}

#[test]
fn failed_model_discovery_can_save_without_changing_model() {
    let mut state = State::default();
    state.reduce(Action::ModelDiscoveryFailed {
        form: submission(),
        message: "HTTP 404".into(),
    });
    assert!(matches!(&state.input, InputMode::Models(picker) if picker.warning.is_some()));
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::Add(FormSubmission {
            model: ModelUpdate::Clear,
            ..
        }))
    ));
}

#[test]
fn model_picker_accepts_manual_model() {
    let mut state = State {
        input: InputMode::Models(ModelPicker {
            form: submission(),
            models: vec!["gpt-5".into()],
            selected: 0,
            query: String::new(),
            mode: ModelPickerMode::Browse,
            warning: None,
        }),
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('m')));
    for character in "custom-model".chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        state.take_effect(),
        Some(Effect::Add(FormSubmission {
            model: ModelUpdate::Set(model),
            ..
        })) if model == "custom-model"
    ));
}

#[test]
fn localized_form_has_input_background_and_context_help() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('a')));
    let locale = I18n::new(Some("zh-CN"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw form");
    let buffer = terminal.backend().buffer();
    let rendered = buffer
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Provider"));
    assert!(rendered.contains("备 注"));
    assert!(rendered.contains("enter"));
    assert!(rendered.contains("sk-****"));
    assert!(buffer.content().iter().any(|cell| cell.bg == INPUT_BG));
}

#[test]
fn edit_form_marks_an_empty_api_key_as_unchanged() {
    let mut state = State {
        providers: vec![example_provider()],
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('e')));
    let locale = I18n::new(Some("zh-CN"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw edit form");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert_eq!(locale.text("api_key_preserve_hint"), "<不修改>");
    assert!(rendered.contains("<不 修 改 >"));
    assert!(!rendered.contains("sk-****"));
}

#[test]
fn validation_errors_temporarily_replace_form_shortcuts() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('a')));
    state.reduce(key(KeyCode::Enter));
    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw validation error");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Base URL is required"));
    assert!(!rendered.contains("ctrl+u clear"));

    state.reduce(key(KeyCode::Char('h')));
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw restored shortcuts");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("ctrl+u clear"));
}

#[test]
fn form_renders_fields_in_requested_order() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('a')));
    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw form");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    let positions = ["Base URL", "API key", "Name", "Description", "Auth"]
        .map(|label| rendered.find(label).expect("field label must render"));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn claude_form_uses_a_base_url_without_v1() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Tab));
    state.reduce(key(KeyCode::Char('a')));
    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw Claude form");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("https://api.anthropic.com"));
    assert!(!rendered.contains("https://api.openai.com/v1"));
}

#[test]
fn language_menu_orders_system_english_then_chinese() {
    let mut state = State {
        loading: false,
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::Language { selected: 0 },
        }),
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw language submenu");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    let positions = ["Follow system", "English (US)", "Simplified Chinese"]
        .map(|label| rendered.find(label).expect("language option must render"));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn header_uses_a_wordmark_title_with_compact_fallback() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw full header");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert_eq!(TITLE, "Hsin");
    assert!(rendered.contains(VERSION_LABEL));
    assert!(rendered.contains("███████╗"));
    assert!(!rendered.contains("╭──────────╮"));

    let mut compact = Terminal::new(TestBackend::new(30, 20)).expect("compact terminal");
    compact
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw compact header");
    let rendered = compact
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Hsin"));
    assert!(rendered.contains(VERSION_LABEL));
    assert!(text_has_foreground_pattern(
        compact.backend().buffer(),
        30,
        "Hsin",
        &[WHITE, RED, WHITE, WHITE]
    ));
    assert!(!rendered.contains("███████╗"));
}

#[test]
fn form_can_cover_the_header_but_preserves_the_footer() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.reduce(key(KeyCode::Char('a')));
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw form over header");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(!rendered.contains(VERSION_LABEL));
    assert!(rendered.contains("ctrl+u clear"));
}

#[test]
fn header_client_switcher_uses_a_selected_background() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw Codex selection");
    let buffer = terminal.backend().buffer();
    assert!(text_has_background(buffer, 80, "Codex", RED));
    assert!(text_has_foreground_pattern(
        buffer,
        80,
        "Codex",
        &[WHITE; 5]
    ));
    assert!(text_has_background(buffer, 80, "Claude Code", INPUT_BG));
    let rendered = buffer
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(!rendered.contains("Client  TAB"));

    state.reduce(key(KeyCode::Tab));
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw Claude selection");
    let buffer = terminal.backend().buffer();
    assert!(text_has_background(buffer, 80, "Codex", INPUT_BG));
    assert!(text_has_background(buffer, 80, "Claude Code", RED));
    assert!(text_has_foreground_pattern(
        buffer,
        80,
        "Claude Code",
        &[WHITE; 11]
    ));
}

#[test]
fn header_respects_client_visibility_and_order() {
    let mut state = State {
        client: ClientKind::Claude,
        client_settings: ClientSettings {
            order: vec![ClientKind::Claude, ClientKind::Codex],
            visible: vec![ClientKind::Claude, ClientKind::Codex],
        },
        loading: false,
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw reordered clients");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.find("Claude Code").unwrap() < rendered.find("Codex").unwrap());

    state.client_settings.visible = vec![ClientKind::Claude];
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw one visible client");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Claude Code"));
    assert!(!rendered.contains("Codex"));
}

#[test]
fn home_keeps_the_status_panel_without_mode_or_language_summary() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw home screen");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Status"));
    assert!(rendered.contains("Select a Provider"));
    assert!(!rendered.contains("Mode:"));
    assert!(!rendered.contains("Language:"));
}

#[test]
fn official_provider_details_are_localized_and_show_oauth() {
    let mut official = example_provider();
    official.id = "official-codex".into();
    official.name = "Official".into();
    official.description.clear();
    official.base_url = "https://api.openai.com/v1".into();
    official.auth_scheme = AuthScheme::OAuth;
    official.official = true;
    official.credential_configured = false;
    let mut state = State {
        providers: vec![official],
        loading: false,
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_ZH_CN));
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw Official details");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    let compact = rendered.replace(' ', "");
    assert!(compact.contains("官方"));
    assert!(compact.contains("OpenAI官方服务"));
    assert!(rendered.contains("OAuth"));
}

fn text_has_background(
    buffer: &ratatui::buffer::Buffer,
    width: usize,
    needle: &str,
    background: ratatui::style::Color,
) -> bool {
    let symbols = needle
        .chars()
        .map(|character| character.to_string())
        .collect::<Vec<_>>();
    buffer.content().chunks(width).any(|row| {
        row.windows(symbols.len()).any(|cells| {
            cells
                .iter()
                .zip(&symbols)
                .all(|(cell, symbol)| cell.symbol() == symbol && cell.bg == background)
        })
    })
}

fn text_has_foreground_pattern(
    buffer: &ratatui::buffer::Buffer,
    width: usize,
    needle: &str,
    foregrounds: &[ratatui::style::Color],
) -> bool {
    let symbols = needle
        .chars()
        .map(|character| character.to_string())
        .collect::<Vec<_>>();
    buffer.content().chunks(width).any(|row| {
        row.windows(symbols.len()).any(|cells| {
            cells
                .iter()
                .zip(&symbols)
                .zip(foregrounds)
                .all(|((cell, symbol), foreground)| {
                    cell.symbol() == symbol && cell.fg == *foreground
                })
        })
    })
}

#[test]
fn renders_provider_and_compact_windows() {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.providers.push(example_provider());
    let locale = I18n::new(Some("en-US"));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw full terminal");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Example"));
    assert!(rendered.contains("api.example.test"));
    assert!(rendered.contains("Primary provider"));
    assert!(rendered.contains("sk-abc***de"));
    assert!(rendered.contains("Bearer"));
    assert!(rendered.contains("Client proxy"));
    assert!(rendered.contains("a add"));
    assert!(rendered.contains("e edit"));
    assert!(rendered.contains("d delete"));
    assert!(rendered.contains("███████╗"));
    assert!(!rendered.contains("Provider control plane"));

    state.input = InputMode::Settings(SettingsScreen {
        selected: 0,
        page: SettingsPage::Root,
    });
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw settings");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Settings"));
    assert!(!rendered.contains("Options"));
    assert!(rendered.contains("Description"));
    assert!(!rendered.contains("Import current Provider"));
    assert!(rendered.contains("Control whether hsind"));
    assert!(!rendered.contains("2 / 2"));
    assert!(!rendered.contains("Example"));
    assert!(!rendered.contains("Codex"));
    assert!(!rendered.contains("Claude Code"));
    let settings_rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(80)
        .collect::<Vec<_>>();
    let proxy_row = settings_rows
        .iter()
        .find(|row| {
            let text = row
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            text.contains("Proxy settings") && text.contains("[disabled]")
        })
        .expect("proxy row with bracketed current value");
    let language_row = settings_rows
        .iter()
        .find(|row| {
            let text = row
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            text.contains("Language") && text.contains("[Follow system]")
        })
        .expect("language row with bracketed current value");
    assert_eq!(
        proxy_row.iter().position(|cell| cell.symbol() == "]"),
        language_row.iter().position(|cell| cell.symbol() == "]")
    );

    state.reduce(key(KeyCode::Enter));
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw proxy submenu");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Master switch"));
    assert!(rendered.contains("enter change"));

    let mut compact = Terminal::new(TestBackend::new(30, 8)).expect("compact terminal");
    compact
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw compact terminal");
}

#[test]
fn custom_auth_warning_colors_only_the_security_phrase_red() {
    let mut state = State {
        loading: false,
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::ClientConfig {
                client: ClientKind::Codex,
                selected: 0,
            },
        }),
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_ZH_CN));
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw client auth settings");
    let buffer = terminal.backend().buffer();
    let rendered = buffer
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.replace(' ', "").contains("禁用自定义Auth"));
    assert!(!rendered.contains("HSIN_MANAGED_KEY"));
    for symbol in ["安", "全", "性", "降", "低"] {
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == symbol && cell.fg == RED)
        );
    }
    for symbol in ["这", "会", "使", "如", "果"] {
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == symbol && cell.fg != RED)
        );
    }
}

#[test]
fn proxy_settings_renders_switch_address_and_port() {
    let mut state = State {
        loading: false,
        input: InputMode::Settings(SettingsScreen {
            selected: 0,
            page: SettingsPage::Proxy {
                selected: 0,
                host: "127.0.0.1".into(),
                port: "9999".into(),
                editing_host: false,
                editing_port: false,
            },
        }),
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw proxy settings");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    for expected in [
        "Master switch",
        "[off]",
        "Address",
        "[127.0.0.1]",
        "Port",
        "[9999]",
    ] {
        assert!(rendered.contains(expected), "missing {expected}");
    }
}

#[test]
fn client_settings_render_visibility_and_move_mode() {
    let mut state = State {
        loading: false,
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::ClientVisibility { selected: 1 },
        }),
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw client visibility");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Displayed clients"));
    assert!(rendered.contains("[on]"));
    assert!(rendered.contains("enter toggle"));

    state.input = InputMode::Settings(SettingsScreen {
        selected: 1,
        page: SettingsPage::ClientOrder {
            selected: 0,
            order: ClientKind::ALL.to_vec(),
            moving: true,
        },
    });
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw client order movement");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Client order"));
    assert!(rendered.contains("1. Codex"));
    assert!(rendered.contains("move"));
    assert!(rendered.contains("enter save"));
}

#[test]
fn settings_header_hides_clients_at_full_and_compact_sizes() {
    let mut state = State {
        loading: false,
        input: InputMode::Settings(SettingsScreen {
            selected: 0,
            page: SettingsPage::Root,
        }),
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    for (width, height) in [(80, 24), (30, 20)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut state, &locale))
            .expect("draw settings header");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("Codex"));
        assert!(!rendered.contains("Claude Code"));
        assert!(rendered.contains(VERSION_LABEL));
    }
}

#[test]
fn client_configuration_renders_import_current_provider() {
    let mut state = State {
        loading: false,
        input: InputMode::Settings(SettingsScreen {
            selected: 1,
            page: SettingsPage::ClientConfig {
                client: ClientKind::Claude,
                selected: 1,
            },
        }),
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw Claude Code settings");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("Claude Code configuration"));
    assert!(rendered.contains("Import current Provider"));
    assert!(rendered.contains("A new Provider is created only when"));
    assert!(!rendered.contains("Import all"));
    assert!(!rendered.contains("reduces security"));
}

fn rendered_with(status: crate::rpc::StatusSnapshot) -> String {
    let mut state = State {
        providers: vec![example_provider()],
        loading: false,
        status,
        ..State::default()
    };
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, &mut state, &locale))
        .expect("draw banner");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>()
}

#[test]
fn a_locked_key_store_is_announced_with_the_recovery_command() {
    // A locked daemon still serves, so the only signal the operator gets is the
    // banner. Without it the state is invisible until an action fails.
    let rendered = rendered_with(crate::rpc::StatusSnapshot {
        security_locked: true,
        ..crate::rpc::StatusSnapshot::default()
    });
    assert!(rendered.contains("Key store locked"));
    assert!(rendered.contains("import-recovery-key"));
}

#[test]
fn a_missing_recovery_key_is_announced_until_it_is_exported() {
    let warned = rendered_with(crate::rpc::StatusSnapshot {
        recovery_key_exported: false,
        ..crate::rpc::StatusSnapshot::default()
    });
    assert!(warned.contains("No recovery key exported"));
    assert!(warned.contains("export-recovery-key"));

    let held = rendered_with(crate::rpc::StatusSnapshot::default());
    assert!(!held.contains("No recovery key exported"));
}

fn named_provider(id: &str, name: &str, base_url: &str) -> Provider {
    Provider {
        id: id.into(),
        name: name.into(),
        base_url: base_url.into(),
        ..example_provider()
    }
}

fn searchable_state() -> State {
    let mut state = State {
        loading: false,
        ..State::default()
    };
    state.providers = vec![
        named_provider("a", "Alpha", "https://alpha.test/v1"),
        named_provider("b", "Beta", "https://beta.test/v1"),
    ];
    state
}

fn type_query(state: &mut State, query: &str) {
    for character in query.chars() {
        state.reduce(key(KeyCode::Char(character)));
    }
}

fn render(state: &mut State, width: u16, height: u16) -> String {
    let locale = I18n::new(Some(LANGUAGE_EN_US));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, state, &locale))
        .expect("draw");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>()
}

#[test]
fn enter_applies_the_search_instead_of_discarding_it() {
    // The footer advertises "enter apply"; before this the query lived inside the input mode and
    // was dropped the moment enter returned to the normal screen.
    let mut state = searchable_state();
    state.reduce(key(KeyCode::Char('/')));
    type_query(&mut state, "alpha");
    state.reduce(key(KeyCode::Enter));

    assert!(matches!(state.input, InputMode::Normal));
    assert_eq!(state.search, "alpha");
    let visible = state.visible_providers();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].name, "Alpha");
}

#[test]
fn escape_in_the_search_box_keeps_the_previously_applied_filter() {
    let mut state = searchable_state();
    state.reduce(key(KeyCode::Char('/')));
    type_query(&mut state, "alpha");
    state.reduce(key(KeyCode::Enter));

    // Reopening prefills the committed query, and abandoning the edit must not drop it.
    state.reduce(key(KeyCode::Char('/')));
    assert!(matches!(&state.input, InputMode::Search(query) if query == "alpha"));
    type_query(&mut state, "zzz");
    state.reduce(key(KeyCode::Esc));

    assert_eq!(state.search, "alpha");
    assert_eq!(state.visible_providers().len(), 1);
}

#[test]
fn control_u_clears_the_search_query_instead_of_typing_u() {
    let mut state = searchable_state();
    state.reduce(key(KeyCode::Char('/')));
    type_query(&mut state, "alpha");
    state.reduce(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));

    assert!(matches!(&state.input, InputMode::Search(query) if query.is_empty()));
    assert_eq!(state.visible_providers().len(), 2);
}

#[test]
fn escape_clears_an_active_filter_before_it_quits() {
    // Quitting is the only way out of the normal screen, so esc has to do double duty: an applied
    // filter would otherwise be unclearable without reopening the search box.
    let mut state = searchable_state();
    state.reduce(key(KeyCode::Char('/')));
    type_query(&mut state, "alpha");
    state.reduce(key(KeyCode::Enter));

    assert_eq!(state.reduce(key(KeyCode::Esc)), Transition::Continue);
    assert!(state.search.is_empty());
    assert_eq!(state.visible_providers().len(), 2);
    assert_eq!(state.reduce(key(KeyCode::Esc)), Transition::Quit);
}

#[test]
fn q_still_quits_with_an_active_filter() {
    let mut state = searchable_state();
    state.search = "alpha".into();
    assert_eq!(state.reduce(key(KeyCode::Char('q'))), Transition::Quit);
}

#[test]
fn an_empty_search_result_is_distinct_from_an_unconfigured_provider_list() {
    // Both render an empty list, but "no providers configured" sends the operator off to add one
    // when the real problem is the filter.
    let mut state = searchable_state();
    state.search = "nothing-matches".into();
    let filtered = render(&mut state, 100, 32);
    assert!(filtered.contains("No providers match the search"));
    assert!(!filtered.contains("No providers configured"));

    let mut empty = State {
        loading: false,
        ..State::default()
    };
    let rendered = render(&mut empty, 100, 32);
    assert!(rendered.contains("No providers configured"));
}

#[test]
fn the_search_bar_docks_above_the_list_rather_than_covering_it() {
    let mut state = searchable_state();
    state.reduce(key(KeyCode::Char('/')));
    type_query(&mut state, "a");
    let rendered = render(&mut state, 100, 32);
    // The bar is visible and both matches are still readable behind it.
    assert!(rendered.contains("Search"));
    assert!(rendered.contains("Alpha"));
    assert!(rendered.contains("Beta"));
}

#[test]
fn the_search_bar_stays_visible_while_a_filter_is_applied() {
    // Without it the list is silently short with nothing on screen explaining why.
    let mut state = searchable_state();
    state.search = "alpha".into();
    let rendered = render(&mut state, 100, 32);
    assert!(rendered.contains("Search"));
    assert!(rendered.contains("esc clear search"));
}

#[test]
fn the_search_bar_appears_as_soon_as_the_search_box_opens() {
    // The footer switches to the search hints on `/`, so the bar has to appear with it rather than
    // waiting for the first typed character.
    let mut state = searchable_state();
    state.reduce(key(KeyCode::Char('/')));
    let rendered = render(&mut state, 100, 32);
    assert!(rendered.contains("Search"));
}

fn claude_form(mapping: Option<ClaudeModelMapping>) -> ProviderForm {
    ProviderForm {
        id: None,
        revision: None,
        client: ClientKind::Claude,
        name: "Claude".into(),
        description: String::new(),
        base_url: "https://api.example.test".into(),
        auth_scheme: AuthScheme::XApiKey,
        secret: Zeroizing::new("secret".into()),
        copied_secret: None,
        field: 0,
        error: None,
        secret_visible: false,
        discovering_models: false,
        claude_model_mapping: mapping,
    }
}

fn mapping_state(mapping: Option<ClaudeModelMapping>) -> State {
    let mut state = State {
        client: ClientKind::Claude,
        loading: false,
        input: InputMode::Form(claude_form(mapping)),
        ..State::default()
    };
    state.reduce(key(KeyCode::Enter));
    state
}

fn submitted_mapping(state: &mut State) -> ClaudeModelMappingUpdate {
    match state.take_effect() {
        Some(Effect::Add(submission) | Effect::Edit(submission)) => submission.claude_model_mapping,
        other => panic!("expected an add/edit effect, got {}", other.is_some()),
    }
}

#[test]
fn a_claude_form_opens_the_mapping_dialog_instead_of_saving_immediately() {
    // Codex resolves its model by discovery; Claude has no discovery endpoint, so the mapping
    // dialog is where the tiers get chosen.
    let mut state = mapping_state(None);
    assert!(matches!(state.input, InputMode::ModelMapping(_)));
    assert!(state.take_effect().is_none());
}

#[test]
fn arrow_keys_do_not_flip_a_tier_1m_box() {
    // ←/→ belong to the model text a tier row is focused on; only space touches its 1M box, so a
    // stray arrow while typing cannot silently change what gets requested upstream.
    let mut state = mapping_state(None);
    state.reduce(key(KeyCode::Char(' '))); // enable the mapping
    state.reduce(key(KeyCode::Down)); // Fable
    state.reduce(key(KeyCode::Tab));
    state.reduce(key(KeyCode::Right));
    state.reduce(key(KeyCode::Left));

    let InputMode::ModelMapping(mapping) = &state.input else {
        panic!("expected the mapping dialog");
    };
    assert!(mapping.enabled);
    assert!(!mapping.rows[0].context_1m);

    state.reduce(key(KeyCode::Char(' ')));
    let InputMode::ModelMapping(mapping) = &state.input else {
        panic!("expected the mapping dialog");
    };
    assert!(mapping.rows[0].context_1m);
}

#[test]
fn tab_completes_the_default_model_for_the_focused_tier() {
    let mut state = mapping_state(None);
    state.reduce(key(KeyCode::Char(' '))); // enable the mapping
    state.reduce(key(KeyCode::Down)); // Fable
    state.reduce(key(KeyCode::Tab));
    state.reduce(key(KeyCode::Down)); // Opus
    state.reduce(key(KeyCode::Tab));
    state.reduce(key(KeyCode::Char(' '))); // 1M on Opus

    state.reduce(key(KeyCode::Enter));
    let ClaudeModelMappingUpdate::Set(mapping) = submitted_mapping(&mut state) else {
        panic!("expected a mapping");
    };
    assert!(mapping.enabled);
    assert_eq!(
        mapping.fable,
        Some(ModelSlot {
            model: "claude-fable-5".into(),
            context_1m: false,
        })
    );
    assert_eq!(
        mapping.opus,
        Some(ModelSlot {
            model: "claude-opus-5".into(),
            context_1m: true,
        })
    );
    assert_eq!(mapping.sonnet, None);
}

#[test]
fn tab_does_not_overwrite_a_model_the_operator_typed() {
    let mut state = mapping_state(None);
    state.reduce(key(KeyCode::Char(' ')));
    state.reduce(key(KeyCode::Down));
    type_query(&mut state, "custom");
    state.reduce(key(KeyCode::Tab));

    state.reduce(key(KeyCode::Enter));
    let ClaudeModelMappingUpdate::Set(mapping) = submitted_mapping(&mut state) else {
        panic!("expected a mapping");
    };
    assert_eq!(mapping.fable.expect("fable tier").model, "custom");
}

#[test]
fn control_u_clears_the_focused_mapping_row() {
    let mut state = mapping_state(None);
    state.reduce(key(KeyCode::Char(' ')));
    state.reduce(key(KeyCode::Down));
    state.reduce(key(KeyCode::Tab));
    state.reduce(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));

    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        submitted_mapping(&mut state),
        ClaudeModelMappingUpdate::Clear
    ));
}

#[test]
fn the_haiku_tier_cannot_request_1m_context() {
    // claude-haiku-4-5 has no 1M variant upstream, so the row renders no checkbox and the toggle
    // key is inert there.
    let mut state = mapping_state(None);
    state.reduce(key(KeyCode::Char(' ')));
    for _ in 0..4 {
        state.reduce(key(KeyCode::Down));
    }
    state.reduce(key(KeyCode::Tab));
    state.reduce(key(KeyCode::Char(' ')));

    state.reduce(key(KeyCode::Enter));
    let ClaudeModelMappingUpdate::Set(mapping) = submitted_mapping(&mut state) else {
        panic!("expected a mapping");
    };
    assert_eq!(
        mapping.haiku,
        Some(ModelSlot {
            model: "claude-haiku-4-5".into(),
            context_1m: false,
        })
    );
}

#[test]
fn a_disabled_master_toggle_writes_no_mapping_at_all() {
    let mut state = mapping_state(Some(ClaudeModelMapping {
        enabled: true,
        opus: Some(ModelSlot {
            model: "claude-opus-5".into(),
            context_1m: true,
        }),
        ..ClaudeModelMapping::default()
    }));
    // The dialog prefills from the provider, so turning the master switch off is the whole gesture.
    state.reduce(key(KeyCode::Char(' ')));
    state.reduce(key(KeyCode::Enter));
    assert!(matches!(
        submitted_mapping(&mut state),
        ClaudeModelMappingUpdate::Clear
    ));
}

#[test]
fn editing_a_claude_provider_prefills_the_existing_mapping() {
    let mut state = mapping_state(Some(ClaudeModelMapping {
        enabled: true,
        sonnet: Some(ModelSlot {
            model: "my-sonnet".into(),
            context_1m: true,
        }),
        ..ClaudeModelMapping::default()
    }));
    let InputMode::ModelMapping(mapping) = &state.input else {
        panic!("expected the mapping dialog");
    };
    assert!(mapping.enabled);
    assert_eq!(mapping.rows[2].model, "my-sonnet");
    assert!(mapping.rows[2].context_1m);

    let rendered = render(&mut state, 100, 32);
    assert!(rendered.contains("my-sonnet"));
    assert!(rendered.contains("Model mapping"));
}

#[test]
fn the_mapping_dialog_sits_where_the_provider_form_did() {
    // The mapping dialog opens straight out of the form on enter; centring them over different
    // regions made the popup jump down the screen at that moment.
    let mut form = State {
        client: ClientKind::Claude,
        loading: false,
        input: InputMode::Form(claude_form(None)),
        ..State::default()
    };
    let form_row = popup_row(&render(&mut form, 100, 32), "Add provider");

    let mut mapping = mapping_state(None);
    let mapping_row = popup_row(&render(&mut mapping, 100, 32), "Model mapping");

    assert_eq!(form_row, mapping_row);
}

/// The 0-based screen row carrying `title`, from a `render` dump of a 100-column terminal.
fn popup_row(rendered: &str, title: &str) -> usize {
    rendered
        .chars()
        .collect::<Vec<char>>()
        .chunks(100)
        .position(|row| row.iter().collect::<String>().contains(title))
        .unwrap_or_else(|| panic!("{title} is not on screen"))
}
