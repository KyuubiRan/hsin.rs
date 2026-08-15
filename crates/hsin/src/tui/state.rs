use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hsin_core::{
    AuthScheme, ClaudeModelMapping, ClaudeModelMappingUpdate, ClientAuthSettings, ClientKind,
    ClientSettings, ConnectionMode, LANGUAGE_EN_US, LANGUAGE_SYSTEM, LANGUAGE_ZH_CN,
    ModelDiscovery, ModelSlot, ModelUpdate, Provider, Settings, convert_provider_base_url,
    normalize_generated_provider_name, provider_name_from_url,
};
use zeroize::Zeroizing;

use crate::rpc::StatusSnapshot;

use super::effects::Effect;

pub(super) enum Action {
    Key(KeyEvent),
    Loaded {
        providers: Vec<Provider>,
        status: StatusSnapshot,
        settings: Settings,
    },
    Notice(&'static str),
    Failed(String),
    ModelsDiscovered {
        form: FormSubmission,
        discovery: ModelDiscovery,
    },
    ModelDiscoveryFailed {
        form: FormSubmission,
        message: String,
    },
    ProviderCopied(ProviderClipboard),
    /// Drives the timers the UI owns; today only the delete confirmation, which lapses on its own.
    Tick,
}

/// How long a `d` press stays armed before the confirmation lapses, so a stray second `d` typed
/// much later cannot delete a provider.
pub(super) const DELETE_CONFIRM_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Transition {
    Continue,
    Quit,
}

pub(super) struct State {
    pub(super) client: ClientKind,
    pub(super) providers: Vec<Provider>,
    pub(super) selected: usize,
    pub(super) status: StatusSnapshot,
    pub(super) language: String,
    pub(super) proxy_enabled: bool,
    pub(super) proxy_host: String,
    pub(super) proxy_port: u16,
    pub(super) client_settings: ClientSettings,
    pub(super) client_auth: ClientAuthSettings,
    pub(super) claude_model_names_enabled: bool,
    pub(super) clipboard: Option<ProviderClipboard>,
    pub(super) loading: bool,
    pub(super) notice: Option<String>,
    pub(super) input: InputMode,
    pub(super) pending_effect: Option<Effect>,
    /// Filter committed with enter; survives leaving [`InputMode::Search`].
    pub(super) search: String,
    /// Where the cursor sat in each client left behind, so returning to one resumes there instead
    /// of dropping back onto the official provider at the top.
    pub(super) parked: HashMap<ClientKind, usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            client: ClientKind::Codex,
            providers: Vec::new(),
            selected: 0,
            status: StatusSnapshot::default(),
            language: LANGUAGE_SYSTEM.into(),
            proxy_enabled: false,
            proxy_host: "127.0.0.1".into(),
            proxy_port: 9999,
            client_settings: ClientSettings::default(),
            client_auth: ClientAuthSettings::default(),
            claude_model_names_enabled: true,
            clipboard: None,
            loading: true,
            notice: None,
            input: InputMode::Normal,
            pending_effect: None,
            search: String::new(),
            parked: HashMap::new(),
        }
    }
}

#[derive(Default)]
pub(super) enum InputMode {
    #[default]
    Normal,
    Search {
        query: String,
        cursor: usize,
    },
    Form(ProviderForm),
    Models(ModelPicker),
    ModelMapping(ModelMappingForm),
    DeleteConfirm {
        id: String,
        revision: u64,
        expires_at: Instant,
    },
    Settings(SettingsScreen),
}

pub(super) struct SettingsScreen {
    pub(super) selected: usize,
    pub(super) page: SettingsPage,
}

pub(super) enum SettingsPage {
    Root,
    Proxy {
        selected: usize,
        host: String,
        port: String,
        editing_host: bool,
        editing_port: bool,
    },
    Language {
        selected: usize,
    },
    Clients {
        selected: usize,
    },
    ClientConfig {
        client: ClientKind,
        selected: usize,
    },
    ClientVisibility {
        selected: usize,
    },
    ClientOrder {
        selected: usize,
        order: Vec<ClientKind>,
        moving: bool,
    },
}

pub(super) struct ProviderForm {
    pub(super) id: Option<String>,
    pub(super) revision: Option<u64>,
    pub(super) client: ClientKind,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) base_url: String,
    pub(super) auth_scheme: AuthScheme,
    pub(super) secret: Zeroizing<String>,
    pub(super) copied_secret: Option<Zeroizing<String>>,
    pub(super) field: usize,
    pub(super) error: Option<&'static str>,
    pub(super) secret_visible: bool,
    pub(super) discovering_models: bool,
    /// Caret position, in characters, inside the focused field. Reset whenever the focus moves.
    pub(super) cursor: usize,
    /// Carried through the form so the mapping dialog can prefill an existing provider's tiers.
    pub(super) claude_model_mapping: Option<ClaudeModelMapping>,
}

pub(super) struct ProviderClipboard {
    pub(super) provider: Provider,
    pub(super) secret: Zeroizing<String>,
}

pub(super) struct FormSubmission {
    pub(super) id: Option<String>,
    pub(super) revision: Option<u64>,
    pub(super) client: ClientKind,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) base_url: String,
    pub(super) auth_scheme: AuthScheme,
    pub(super) secret: Zeroizing<String>,
    pub(super) model: ModelUpdate,
    pub(super) claude_model_mapping: ClaudeModelMappingUpdate,
}

/// The Claude model tiers, in display order. The default is the ghost text shown in an empty box
/// and completed by tab; `context_1m` marks the tiers that actually have a 1M-context variant.
pub(super) struct MappingTier {
    pub(super) label: &'static str,
    pub(super) default_model: &'static str,
    pub(super) context_1m: bool,
}

pub(super) const MAPPING_TIERS: [MappingTier; 4] = [
    MappingTier {
        label: "Fable",
        default_model: "claude-fable-5",
        context_1m: true,
    },
    MappingTier {
        label: "Opus",
        default_model: "claude-opus-5",
        context_1m: true,
    },
    MappingTier {
        label: "Sonnet",
        default_model: "claude-sonnet-5",
        context_1m: true,
    },
    MappingTier {
        // Haiku 4.5 has no 1M-context variant upstream, so it gets no checkbox.
        label: "Haiku",
        default_model: "claude-haiku-4-5",
        context_1m: false,
    },
];

#[derive(Default, Clone)]
pub(super) struct MappingRow {
    pub(super) model: String,
    pub(super) context_1m: bool,
}

/// Second step of the Claude provider form: map Claude Code's model tiers onto upstream IDs.
pub(super) struct ModelMappingForm {
    pub(super) form: FormSubmission,
    pub(super) enabled: bool,
    /// `ANTHROPIC_MODEL` — the session default, independent of the four tiers.
    pub(super) default_model: String,
    pub(super) default_context_1m: bool,
    pub(super) rows: [MappingRow; 4],
    /// `0` is the master toggle, `1` the default model, `2..=5` the tier rows.
    pub(super) field: usize,
    /// Caret position, in characters, inside the focused text row.
    pub(super) cursor: usize,
}

impl ModelMappingForm {
    fn from_existing(form: FormSubmission, existing: Option<&ClaudeModelMapping>) -> Self {
        let slot = |slot: Option<&ModelSlot>| {
            slot.map_or_else(MappingRow::default, |slot| MappingRow {
                model: slot.model.clone(),
                context_1m: slot.context_1m,
            })
        };
        let (enabled, default_model, default_context_1m, rows) = existing.map_or_else(
            || {
                (
                    false,
                    String::new(),
                    false,
                    [(); 4].map(|()| MappingRow::default()),
                )
            },
            |mapping| {
                (
                    mapping.enabled,
                    mapping.default_model.clone().unwrap_or_default(),
                    mapping.default_context_1m,
                    [
                        slot(mapping.fable.as_ref()),
                        slot(mapping.opus.as_ref()),
                        slot(mapping.sonnet.as_ref()),
                        slot(mapping.haiku.as_ref()),
                    ],
                )
            },
        );
        Self {
            form,
            enabled,
            default_model,
            default_context_1m,
            rows,
            field: 0,
            cursor: 0,
        }
    }

    /// The mapping to persist, or `None` when nothing would be written.
    fn mapping(&self) -> Option<ClaudeModelMapping> {
        let slot = |index: usize| {
            let row = &self.rows[index];
            let model = row.model.trim();
            (!model.is_empty()).then(|| ModelSlot {
                model: model.to_owned(),
                context_1m: row.context_1m && MAPPING_TIERS[index].context_1m,
            })
        };
        let default_model = self.default_model.trim();
        let mapping = ClaudeModelMapping {
            enabled: self.enabled,
            default_model: (!default_model.is_empty()).then(|| default_model.to_owned()),
            default_context_1m: !default_model.is_empty() && self.default_context_1m,
            fable: slot(0),
            opus: slot(1),
            sonnet: slot(2),
            haiku: slot(3),
        };
        (!mapping.is_inert()).then_some(mapping)
    }

    /// The text row with focus: the default model, then one per tier.
    fn focused_text(&mut self) -> Option<&mut String> {
        match self.field {
            0 => None,
            1 => Some(&mut self.default_model),
            field => Some(&mut self.rows[field - 2].model),
        }
    }
}

pub(super) struct ModelPicker {
    pub(super) form: FormSubmission,
    pub(super) models: Vec<String>,
    pub(super) selected: usize,
    pub(super) query: String,
    pub(super) mode: ModelPickerMode,
    pub(super) warning: Option<String>,
    /// Caret position, in characters, inside whichever of the search or manual boxes is open.
    pub(super) cursor: usize,
}

#[derive(Default)]
pub(super) enum ModelPickerMode {
    #[default]
    Browse,
    Search,
    Manual(String),
}

impl State {
    pub(super) fn reduce(&mut self, action: Action) -> Transition {
        match action {
            Action::Loaded {
                providers,
                status,
                settings,
            } => self.apply_loaded(providers, status, settings),
            Action::Notice(key) => {
                self.notice = Some(format!("@{key}"));
                self.loading = false;
            }
            Action::Tick => {
                if let InputMode::DeleteConfirm { expires_at, .. } = &self.input
                    && Instant::now() >= *expires_at
                {
                    self.input = InputMode::Normal;
                }
            }
            Action::Failed(message) => {
                self.notice = Some(message);
                self.loading = false;
            }
            Action::ModelsDiscovered {
                mut form,
                discovery,
            } => {
                form.base_url = discovery.resolved_base_url;
                self.input = InputMode::Models(ModelPicker {
                    form,
                    models: discovery.models,
                    selected: 0,
                    query: String::new(),
                    mode: ModelPickerMode::Browse,
                    cursor: 0,
                    warning: None,
                });
                self.loading = false;
                self.notice = None;
            }
            Action::ModelDiscoveryFailed { form, message } => {
                self.input = InputMode::Models(ModelPicker {
                    form,
                    models: Vec::new(),
                    selected: 0,
                    query: String::new(),
                    mode: ModelPickerMode::Browse,
                    cursor: 0,
                    warning: Some(message),
                });
                self.loading = false;
                self.notice = None;
            }
            Action::ProviderCopied(clipboard) => {
                self.clipboard = Some(clipboard);
                self.notice = Some("@provider_copied".into());
                self.loading = false;
            }
            Action::Key(key) => return self.reduce_key(key),
        }
        Transition::Continue
    }

    fn apply_loaded(
        &mut self,
        providers: Vec<Provider>,
        status: StatusSnapshot,
        settings: Settings,
    ) {
        self.providers = providers;
        self.status = status;
        self.language = settings.language;
        self.proxy_enabled = settings.proxy_enabled;
        self.proxy_host = settings.proxy_host;
        self.proxy_port = settings.proxy_port;
        self.client_settings = settings.clients;
        self.client_auth = settings.client_auth;
        self.claude_model_names_enabled = settings.claude_model_names_enabled;
        if !self.client_settings.visible.contains(&self.client)
            && let Some(client) = self.client_settings.visible_in_order().first().copied()
        {
            self.client = client;
        }
        if let InputMode::Settings(SettingsScreen {
            page:
                SettingsPage::Proxy {
                    host,
                    port,
                    editing_host,
                    editing_port,
                    ..
                },
            ..
        }) = &mut self.input
        {
            if !*editing_host {
                host.clone_from(&self.proxy_host);
            }
            if !*editing_port {
                *port = self.proxy_port.to_string();
            }
        }
        if let InputMode::Settings(SettingsScreen {
            page:
                SettingsPage::ClientOrder {
                    order,
                    moving: false,
                    ..
                },
            ..
        }) = &mut self.input
        {
            order.clone_from(&self.client_settings.order);
        }
        self.loading = false;
        self.clamp_selection();
        if let Some(index) = self.active_index() {
            self.selected = index;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn reduce_key(&mut self, key: KeyEvent) -> Transition {
        if matches!(&self.input, InputMode::Form(form) if form.discovering_models) {
            return Transition::Continue;
        }
        let current_mode = self.mode();
        let proxy_enabled = self.proxy_enabled;
        let proxy_host = self.proxy_host.clone();
        let proxy_port = self.proxy_port;
        let client_settings = self.client_settings.clone();
        let client_auth = self.client_auth;
        let claude_model_names_enabled = self.claude_model_names_enabled;
        let language_selected = match self.language.as_str() {
            LANGUAGE_EN_US => 1,
            LANGUAGE_ZH_CN => 2,
            _ => 0,
        };
        self.notice = None;
        match &mut self.input {
            InputMode::Form(form) => form.error = None,
            InputMode::Models(picker) => picker.warning = None,
            _ => {}
        }
        match &mut self.input {
            InputMode::Search { query, cursor } => match key.code {
                KeyCode::Enter => {
                    let committed = std::mem::take(query);
                    self.input = InputMode::Normal;
                    self.search = committed;
                    self.selected = 0;
                }
                KeyCode::Esc => {
                    self.input = InputMode::Normal;
                    self.selected = 0;
                }
                KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.clear();
                    *cursor = 0;
                    self.selected = 0;
                }
                _ => {
                    if edit_text(query, cursor, key) {
                        self.selected = 0;
                    }
                }
            },
            InputMode::Form(form) => match key.code {
                KeyCode::Char('h' | 'H') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.secret_visible = !form.secret_visible;
                }
                KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_form_field(form);
                    form.cursor = 0;
                }
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Tab | KeyCode::Down => {
                    form.field = (form.field + 1) % 5;
                    form.cursor = caret_end(form_field_text(form));
                    form.error = None;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.field = (form.field + 4) % 5;
                    form.cursor = caret_end(form_field_text(form));
                    form.error = None;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char('j' | 'l' | ' ')
                    if form.field == 4 =>
                {
                    form.auth_scheme = match form.auth_scheme {
                        AuthScheme::Bearer => AuthScheme::XApiKey,
                        AuthScheme::XApiKey | AuthScheme::OAuth => AuthScheme::Bearer,
                    };
                }
                KeyCode::Enter => match take_form_submission(form) {
                    Ok(submission) => {
                        // Codex resolves a model by discovery; Claude maps the model tiers by hand.
                        match submission.client {
                            ClientKind::Codex => {
                                self.pending_effect = Some(Effect::DiscoverModels(submission));
                                self.loading = true;
                                form.discovering_models = true;
                                self.notice = Some("@fetching_models".into());
                            }
                            ClientKind::Claude => {
                                let existing = form.claude_model_mapping.clone();
                                self.notice = None;
                                self.input = InputMode::ModelMapping(
                                    ModelMappingForm::from_existing(submission, existing.as_ref()),
                                );
                            }
                        }
                    }
                    Err(error) => form.error = Some(error),
                },
                _ => {
                    let cursor = &mut form.cursor;
                    match form.field {
                        0 => edit_text(&mut form.base_url, cursor, key),
                        1 => edit_text(&mut form.secret, cursor, key),
                        2 => edit_text(&mut form.name, cursor, key),
                        3 => edit_text(&mut form.description, cursor, key),
                        _ => false,
                    };
                }
            },
            InputMode::ModelMapping(mapping) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Up | KeyCode::BackTab => {
                    mapping.field = mapping.field.saturating_sub(1);
                    mapping.cursor = mapping_row_caret_end(mapping);
                }
                KeyCode::Down => {
                    let last = if mapping.enabled {
                        MAPPING_TIERS.len() + 1
                    } else {
                        0
                    };
                    mapping.field = (mapping.field + 1).min(last);
                    mapping.cursor = mapping_row_caret_end(mapping);
                }
                KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(text) = mapping.focused_text() {
                        text.clear();
                        mapping.cursor = 0;
                    }
                }
                // Tab completes the ghost default instead of moving focus; the model rows are the
                // only place in the TUI where a suggested value is worth one keystroke.
                KeyCode::Tab => {
                    if let Some(row) = mapping.field.checked_sub(2)
                        && mapping.rows[row].model.trim().is_empty()
                    {
                        MAPPING_TIERS[row]
                            .default_model
                            .clone_into(&mut mapping.rows[row].model);
                        mapping.cursor = caret_end(&mapping.rows[row].model);
                    }
                }
                // On the master switch ←/→ flip it; on a text row they move the caret through the
                // model being typed, which is why space — not an arrow — owns the 1M box.
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if mapping.field == 0 => {
                    mapping.enabled = !mapping.enabled;
                }
                KeyCode::Char(' ') => {
                    if mapping.field == 1 {
                        mapping.default_context_1m = !mapping.default_context_1m;
                    } else if let Some(row) = mapping.field.checked_sub(2)
                        && MAPPING_TIERS[row].context_1m
                    {
                        mapping.rows[row].context_1m = !mapping.rows[row].context_1m;
                    }
                }
                KeyCode::Enter => {
                    let update = mapping.mapping().map_or(
                        ClaudeModelMappingUpdate::Clear,
                        ClaudeModelMappingUpdate::Set,
                    );
                    mapping.form.claude_model_mapping = update;
                    let submission = take_submission(&mut mapping.form);
                    self.pending_effect = Some(if submission.id.is_some() {
                        Effect::Edit(submission)
                    } else {
                        Effect::Add(submission)
                    });
                    self.loading = true;
                    self.input = InputMode::Normal;
                }
                _ => {
                    let cursor = &mut mapping.cursor;
                    match mapping.field {
                        0 => {}
                        1 => {
                            edit_text(&mut mapping.default_model, cursor, key);
                        }
                        field => {
                            edit_text(&mut mapping.rows[field - 2].model, cursor, key);
                        }
                    }
                }
            },
            InputMode::Models(picker) => match &mut picker.mode {
                ModelPickerMode::Browse => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        self.input = InputMode::Normal;
                    }
                    KeyCode::Char('s') => {
                        picker.mode = ModelPickerMode::Search;
                        picker.cursor = caret_end(&picker.query);
                    }
                    KeyCode::Char('m') => {
                        picker.mode = ModelPickerMode::Manual(String::new());
                        picker.cursor = 0;
                    }
                    KeyCode::Up | KeyCode::Char('i') => {
                        picker.selected = picker.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('k') => {
                        let count = visible_model_count(picker);
                        picker.selected = (picker.selected + 1).min(count);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        picker.form.model = selected_model(picker)
                            .map_or(ModelUpdate::Clear, |model| {
                                ModelUpdate::Set(model.to_owned())
                            });
                        let submission = take_submission(&mut picker.form);
                        self.pending_effect = Some(if submission.id.is_some() {
                            Effect::Edit(submission)
                        } else {
                            Effect::Add(submission)
                        });
                        self.loading = true;
                        self.input = InputMode::Normal;
                    }
                    _ => {}
                },
                ModelPickerMode::Search => match key.code {
                    KeyCode::Esc | KeyCode::Enter => picker.mode = ModelPickerMode::Browse,
                    _ => {
                        if edit_text(&mut picker.query, &mut picker.cursor, key) {
                            picker.selected = 0;
                        }
                    }
                },
                ModelPickerMode::Manual(value) => match key.code {
                    KeyCode::Esc => picker.mode = ModelPickerMode::Browse,
                    KeyCode::Enter if !value.trim().is_empty() => {
                        picker.form.model = ModelUpdate::Set(value.trim().to_owned());
                        let submission = take_submission(&mut picker.form);
                        self.pending_effect = Some(if submission.id.is_some() {
                            Effect::Edit(submission)
                        } else {
                            Effect::Add(submission)
                        });
                        self.loading = true;
                        self.input = InputMode::Normal;
                    }
                    _ => {
                        edit_text(value, &mut picker.cursor, key);
                    }
                },
            },
            InputMode::DeleteConfirm {
                id,
                revision,
                expires_at,
            } => {
                // The tick that retires a lapsed confirmation can be up to one frame away, so the
                // deadline is rechecked here rather than trusting the mode to still be armed.
                if key.code == KeyCode::Char('d') && Instant::now() < *expires_at {
                    self.pending_effect = Some(Effect::Remove {
                        id: id.clone(),
                        expected_revision: *revision,
                    });
                    self.loading = true;
                }
                self.input = InputMode::Normal;
            }
            InputMode::Settings(screen) => match &mut screen.page {
                SettingsPage::Root => match key.code {
                    KeyCode::Esc | KeyCode::Char('o' | 'j') | KeyCode::Left => {
                        self.input = InputMode::Normal;
                    }
                    KeyCode::Up | KeyCode::Char('i') => {
                        screen.selected = screen.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('k') => {
                        screen.selected = (screen.selected + 1).min(2);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match screen.selected {
                        0 => {
                            screen.page = SettingsPage::Proxy {
                                selected: 0,
                                host: proxy_host.clone(),
                                port: proxy_port.to_string(),
                                editing_host: false,
                                editing_port: false,
                            };
                        }
                        1 => {
                            screen.page = SettingsPage::Clients { selected: 0 };
                        }
                        _ => {
                            screen.page = SettingsPage::Language {
                                selected: language_selected,
                            };
                        }
                    },
                    _ => {}
                },
                SettingsPage::Proxy {
                    selected,
                    host,
                    port,
                    editing_host,
                    editing_port,
                } => {
                    if *editing_host {
                        match key.code {
                            KeyCode::Esc => {
                                host.clone_from(&proxy_host);
                                *editing_host = false;
                            }
                            KeyCode::Backspace => {
                                host.pop();
                            }
                            KeyCode::Char(character)
                                if (character.is_ascii_hexdigit()
                                    || matches!(character, '.' | ':'))
                                    && host.len() < 45 =>
                            {
                                host.push(character);
                            }
                            KeyCode::Enter => match host.trim().parse::<IpAddr>() {
                                Ok(value) => {
                                    let value = value.to_string();
                                    host.clone_from(&value);
                                    *editing_host = false;
                                    if value != proxy_host {
                                        self.pending_effect = Some(Effect::SetProxyHost(value));
                                        self.loading = true;
                                    }
                                }
                                Err(_) => self.notice = Some("@validation_proxy_address".into()),
                            },
                            _ => {}
                        }
                    } else if *editing_port {
                        match key.code {
                            KeyCode::Esc => {
                                *port = proxy_port.to_string();
                                *editing_port = false;
                            }
                            KeyCode::Backspace => {
                                port.pop();
                            }
                            KeyCode::Char(character)
                                if character.is_ascii_digit() && port.len() < 5 =>
                            {
                                port.push(character);
                            }
                            KeyCode::Enter => match port.parse::<u16>() {
                                Ok(value) if value >= 1024 => {
                                    *editing_port = false;
                                    if value != proxy_port {
                                        self.pending_effect = Some(Effect::SetProxyPort(value));
                                        self.loading = true;
                                    }
                                }
                                _ => self.notice = Some("@validation_proxy_port".into()),
                            },
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                                screen.page = SettingsPage::Root;
                            }
                            KeyCode::Up | KeyCode::Char('i') => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('k') => {
                                *selected = (*selected + 1).min(2);
                            }
                            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l' | ' ') => {
                                match *selected {
                                    0 => {
                                        self.pending_effect =
                                            Some(Effect::SetProxyEnabled(!proxy_enabled));
                                        self.loading = true;
                                    }
                                    1 => {
                                        *editing_host = true;
                                        host.clear();
                                    }
                                    _ => {
                                        *editing_port = true;
                                        port.clear();
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                SettingsPage::Language { selected } => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        screen.page = SettingsPage::Root;
                    }
                    KeyCode::Up | KeyCode::Char('i') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('k') => {
                        *selected = (*selected + 1).min(2);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        self.pending_effect = Some(Effect::SetLanguage(
                            match *selected {
                                0 => LANGUAGE_SYSTEM,
                                1 => LANGUAGE_EN_US,
                                _ => LANGUAGE_ZH_CN,
                            }
                            .into(),
                        ));
                        self.loading = true;
                        screen.page = SettingsPage::Root;
                    }
                    _ => {}
                },
                SettingsPage::Clients { selected } => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        screen.page = SettingsPage::Root;
                    }
                    KeyCode::Up | KeyCode::Char('i') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('k') => {
                        *selected = (*selected + 1).min(3);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        screen.page = match *selected {
                            0 => SettingsPage::ClientConfig {
                                client: ClientKind::Codex,
                                selected: 0,
                            },
                            1 => SettingsPage::ClientConfig {
                                client: ClientKind::Claude,
                                selected: 0,
                            },
                            2 => SettingsPage::ClientVisibility { selected: 0 },
                            _ => SettingsPage::ClientOrder {
                                selected: 0,
                                order: client_settings.order.clone(),
                                moving: false,
                            },
                        };
                    }
                    _ => {}
                },
                SettingsPage::ClientConfig { client, selected } => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        screen.page = SettingsPage::Clients {
                            selected: match client {
                                ClientKind::Codex => 0,
                                ClientKind::Claude => 1,
                            },
                        };
                    }
                    KeyCode::Up | KeyCode::Char('i') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('k') => {
                        let last = if *client == ClientKind::Claude { 2 } else { 1 };
                        *selected = (*selected + 1).min(last);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l' | ' ') => {
                        self.pending_effect = Some(if *selected == 0 {
                            Effect::SetClientAuth {
                                client: *client,
                                disable_custom_auth: !client_auth.disable_custom_auth(*client),
                            }
                        } else if *client == ClientKind::Claude && *selected == 1 {
                            Effect::SetClaudeModelNames(!claude_model_names_enabled)
                        } else {
                            Effect::ImportCurrent(*client)
                        });
                        self.loading = true;
                    }
                    _ => {}
                },
                SettingsPage::ClientVisibility { selected } => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        screen.page = SettingsPage::Clients { selected: 2 };
                    }
                    KeyCode::Up | KeyCode::Char('i') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('k') => {
                        *selected = (*selected + 1).min(ClientKind::ALL.len() - 1);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l' | ' ') => {
                        let client = client_settings.order[*selected];
                        let mut updated = client_settings.clone();
                        if updated.visible.contains(&client) {
                            if updated.visible.len() == 1 {
                                self.notice = Some("@client_visibility_minimum".into());
                            } else {
                                updated.visible.retain(|candidate| *candidate != client);
                                self.pending_effect = Some(Effect::SetClients(updated));
                                self.loading = true;
                            }
                        } else {
                            updated.visible.push(client);
                            updated.visible = updated.visible_in_order();
                            self.pending_effect = Some(Effect::SetClients(updated));
                            self.loading = true;
                        }
                    }
                    _ => {}
                },
                SettingsPage::ClientOrder {
                    selected,
                    order,
                    moving,
                } => {
                    if *moving {
                        match key.code {
                            KeyCode::Esc => {
                                order.clone_from(&client_settings.order);
                                *moving = false;
                            }
                            KeyCode::Up | KeyCode::Char('i') if *selected > 0 => {
                                order.swap(*selected, *selected - 1);
                                *selected -= 1;
                            }
                            KeyCode::Down | KeyCode::Char('k') if *selected + 1 < order.len() => {
                                order.swap(*selected, *selected + 1);
                                *selected += 1;
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                *moving = false;
                                if *order != client_settings.order {
                                    let mut updated = client_settings.clone();
                                    updated.order.clone_from(order);
                                    updated.visible = updated.visible_in_order();
                                    self.pending_effect = Some(Effect::SetClients(updated));
                                    self.loading = true;
                                }
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                                screen.page = SettingsPage::Clients { selected: 3 };
                            }
                            KeyCode::Up | KeyCode::Char('i') => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('k') => {
                                *selected = (*selected + 1).min(order.len().saturating_sub(1));
                            }
                            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l' | ' ') => {
                                *moving = true;
                            }
                            _ => {}
                        }
                    }
                }
            },
            InputMode::Normal => match key.code {
                KeyCode::Char('q') => return Transition::Quit,
                KeyCode::Esc => {
                    if self.search.is_empty() {
                        return Transition::Quit;
                    }
                    self.search.clear();
                    self.selected = 0;
                }
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.switch_client(self.previous_visible_client());
                }
                KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                    self.switch_client(self.next_visible_client());
                }
                KeyCode::BackTab | KeyCode::Left | KeyCode::Char('j') => {
                    self.switch_client(self.previous_visible_client());
                }
                KeyCode::Down | KeyCode::Char('k') => {
                    let len = self.visible_providers().len();
                    if len > 0 {
                        self.selected = (self.selected + 1).min(len - 1);
                    }
                }
                KeyCode::Up | KeyCode::Char('i') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Char('r') => self.queue(Effect::Refresh),
                KeyCode::Char('p') => {
                    if self.active_id().is_none() {
                        self.notice = Some("@proxy_requires_provider".into());
                    } else {
                        self.queue(Effect::SetMode {
                            client: self.client,
                            mode: if current_mode == ConnectionMode::Proxy {
                                ConnectionMode::Direct
                            } else {
                                ConnectionMode::Proxy
                            },
                        });
                    }
                }
                KeyCode::Char('o') => {
                    self.input = InputMode::Settings(SettingsScreen {
                        selected: 0,
                        page: SettingsPage::Root,
                    });
                }
                KeyCode::Char('/') => {
                    self.input = InputMode::Search {
                        cursor: caret_end(&self.search),
                        query: self.search.clone(),
                    };
                }
                KeyCode::Char('a') => {
                    self.input = InputMode::Form(ProviderForm {
                        id: None,
                        revision: None,
                        client: self.client,
                        name: String::new(),
                        description: String::new(),
                        base_url: String::new(),
                        auth_scheme: match self.client {
                            ClientKind::Codex => AuthScheme::Bearer,
                            ClientKind::Claude => AuthScheme::XApiKey,
                        },
                        secret: Zeroizing::new(String::new()),
                        copied_secret: None,
                        field: 0,
                        error: None,
                        secret_visible: false,
                        discovering_models: false,
                        cursor: 0,
                        claude_model_mapping: None,
                    });
                }
                KeyCode::Char('e') => {
                    if let Some(provider) = self.selected_provider().cloned() {
                        if provider.official {
                            self.notice = Some("@official_read_only".into());
                            return Transition::Continue;
                        }
                        let name =
                            normalize_generated_provider_name(&provider.name, &provider.base_url);
                        let cursor = caret_end(&provider.base_url);
                        self.input = InputMode::Form(ProviderForm {
                            id: Some(provider.id),
                            revision: Some(provider.revision),
                            client: provider.client,
                            name,
                            description: provider.description,
                            base_url: provider.base_url,
                            auth_scheme: provider.auth_scheme,
                            secret: Zeroizing::new(String::new()),
                            copied_secret: None,
                            field: 0,
                            error: None,
                            secret_visible: false,
                            discovering_models: false,
                            cursor,
                            claude_model_mapping: provider.claude_model_mapping,
                        });
                    }
                }
                KeyCode::Char('c') => {
                    let Some(provider) = self.selected_provider().cloned() else {
                        self.notice = Some("@copy_provider_required".into());
                        return Transition::Continue;
                    };
                    if provider.official {
                        self.notice = Some("@copy_official_unsupported".into());
                    } else if !provider.credential_configured {
                        self.notice = Some("@copy_credential_missing".into());
                    } else {
                        self.queue(Effect::CopyProvider(provider));
                    }
                }
                KeyCode::Char('v') => {
                    let Some(clipboard) = &self.clipboard else {
                        self.notice = Some("@provider_clipboard_empty".into());
                        return Transition::Continue;
                    };
                    let source = &clipboard.provider;
                    let base_url =
                        convert_provider_base_url(&source.base_url, source.client, self.client);
                    self.input = InputMode::Form(ProviderForm {
                        id: None,
                        revision: None,
                        client: self.client,
                        name: copied_provider_name(&self.providers, self.client, &source.name),
                        description: source.description.clone(),
                        cursor: caret_end(&base_url),
                        base_url,
                        auth_scheme: if source.client == self.client {
                            source.auth_scheme
                        } else {
                            match self.client {
                                ClientKind::Codex => AuthScheme::Bearer,
                                ClientKind::Claude => AuthScheme::XApiKey,
                            }
                        },
                        secret: Zeroizing::new(String::new()),
                        copied_secret: Some(clipboard.secret.clone()),
                        field: 0,
                        error: None,
                        secret_visible: false,
                        discovering_models: false,
                        // A mapping only makes sense for the client it was written for.
                        claude_model_mapping: (source.client == self.client)
                            .then(|| source.claude_model_mapping.clone())
                            .flatten(),
                    });
                }
                KeyCode::Char('d') => {
                    if let Some(provider) = self.selected_provider() {
                        if provider.official {
                            self.notice = Some("@official_read_only".into());
                            return Transition::Continue;
                        }
                        // The prompt lives in the footer now, so a leftover notice sitting in that
                        // same slot would hide it.
                        let armed = InputMode::DeleteConfirm {
                            id: provider.id.clone(),
                            revision: provider.revision,
                            expires_at: Instant::now() + DELETE_CONFIRM_WINDOW,
                        };
                        self.notice = None;
                        self.input = armed;
                    }
                }
                KeyCode::Enter => {
                    if let Some(provider) = self.selected_provider() {
                        self.queue(Effect::Switch {
                            client: self.client,
                            id: provider.id.clone(),
                        });
                    }
                }
                _ => {}
            },
        }
        Transition::Continue
    }

    fn queue(&mut self, effect: Effect) {
        self.pending_effect = Some(effect);
        self.loading = true;
        self.notice = None;
    }

    pub(super) fn take_effect(&mut self) -> Option<Effect> {
        self.pending_effect.take()
    }

    /// The filter currently shaping the provider list: the in-progress draft while the search bar
    /// has focus, otherwise the query committed with enter.
    pub(super) fn active_query(&self) -> &str {
        match &self.input {
            InputMode::Search { query, .. } => query,
            _ => &self.search,
        }
    }

    pub(super) fn visible_providers(&self) -> Vec<&Provider> {
        let query = match self.active_query() {
            "" => None,
            query => Some(query.to_ascii_lowercase()),
        };
        self.providers
            .iter()
            .filter(|provider| provider.client == self.client)
            .filter(|provider| {
                query.as_ref().is_none_or(|query| {
                    provider.name.to_ascii_lowercase().contains(query)
                        || provider.base_url.to_ascii_lowercase().contains(query)
                })
            })
            .collect()
    }

    pub(super) fn selected_provider(&self) -> Option<&Provider> {
        self.visible_providers().get(self.selected).copied()
    }

    pub(super) fn visible_clients(&self) -> Vec<ClientKind> {
        self.client_settings.visible_in_order()
    }

    fn next_visible_client(&self) -> ClientKind {
        let clients = self.visible_clients();
        if clients.is_empty() {
            return self.client;
        }
        let index = clients
            .iter()
            .position(|client| *client == self.client)
            .unwrap_or(0);
        clients
            .get((index + 1) % clients.len())
            .copied()
            .unwrap_or(self.client)
    }

    fn previous_visible_client(&self) -> ClientKind {
        let clients = self.visible_clients();
        if clients.is_empty() {
            return self.client;
        }
        let index = clients
            .iter()
            .position(|client| *client == self.client)
            .unwrap_or(0);
        clients
            .get((index + clients.len() - 1) % clients.len())
            .copied()
            .unwrap_or(self.client)
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_providers().len().saturating_sub(1));
    }

    /// Move to `client`, resuming the cursor where it was left there. A client not visited yet
    /// starts on its active provider, which is more useful than the top of the list.
    fn switch_client(&mut self, client: ClientKind) {
        if client == self.client {
            return;
        }
        self.parked.insert(self.client, self.selected);
        self.client = client;
        let resumed = self.parked.get(&client).copied();
        let selected = resumed.or_else(|| self.active_index()).unwrap_or(0);
        self.selected = selected;
        self.clamp_selection();
    }

    /// Where the current client's active provider sits in the visible list.
    fn active_index(&self) -> Option<usize> {
        let active = self.active_id()?;
        self.visible_providers()
            .iter()
            .position(|provider| provider.id == active)
    }

    pub(super) fn mode(&self) -> ConnectionMode {
        match self.client {
            ClientKind::Codex => self.status.codex_mode,
            ClientKind::Claude => self.status.claude_mode,
        }
    }

    pub(super) fn active_id(&self) -> Option<&str> {
        match self.client {
            ClientKind::Codex => self.status.codex_active_provider.as_deref(),
            ClientKind::Claude => self.status.claude_active_provider.as_deref(),
        }
    }
}

fn clear_form_field(form: &mut ProviderForm) {
    match form.field {
        0 => form.base_url.clear(),
        1 => form.secret.clear(),
        2 => form.name.clear(),
        3 => form.description.clear(),
        _ => {}
    }
}

pub(super) fn take_form_submission(
    form: &mut ProviderForm,
) -> std::result::Result<FormSubmission, &'static str> {
    let base_url = form.base_url.trim();
    if base_url.is_empty() {
        return Err("validation_base_url_required");
    }
    let url = url::Url::parse(base_url).map_err(|_| "validation_base_url_invalid")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("validation_base_url_invalid");
    }
    let name = if form.name.trim().is_empty() {
        provider_name_from_url(base_url).ok_or("validation_name_required")?
    } else {
        form.name.trim().to_owned()
    };
    if name.chars().count() > 128 {
        return Err("validation_name_too_long");
    }
    if form.description.chars().count() > 1024 {
        return Err("validation_description_too_long");
    }
    if form.id.is_none() && form.secret.trim().is_empty() && form.copied_secret.is_none() {
        return Err("validation_api_key_required");
    }
    let secret = if form.secret.trim().is_empty() {
        form.copied_secret.take().unwrap_or_default()
    } else {
        std::mem::take(&mut form.secret)
    };
    Ok(FormSubmission {
        id: form.id.clone(),
        revision: form.revision,
        client: form.client,
        name,
        description: form.description.clone(),
        base_url: base_url.trim_end_matches('/').to_owned(),
        auth_scheme: form.auth_scheme,
        secret,
        model: ModelUpdate::Preserve,
        claude_model_mapping: ClaudeModelMappingUpdate::Preserve,
    })
}

fn copied_provider_name(providers: &[Provider], client: ClientKind, source_name: &str) -> String {
    let base = format!("{} copy", source_name.trim());
    let mut candidate = base.clone();
    let mut suffix = 2;
    while providers
        .iter()
        .any(|provider| provider.client == client && provider.name == candidate)
    {
        candidate = format!("{base} {suffix}");
        suffix += 1;
    }
    candidate
}

/// Caret-aware editing shared by every single-line text box. Positions are character indices, so a
/// caret can never land inside a multi-byte character.
///
/// Returns whether `key` was an editing key; callers handle their own keys first and fall through
/// to this for the rest.
fn edit_text(text: &mut String, cursor: &mut usize, key: KeyEvent) -> bool {
    *cursor = (*cursor).min(text.chars().count());
    match key.code {
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(text.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = text.chars().count(),
        KeyCode::Backspace => {
            if let Some(previous) = cursor.checked_sub(1) {
                text.remove(character_offset(text, previous));
                *cursor = previous;
            }
        }
        KeyCode::Delete => {
            if *cursor < text.chars().count() {
                text.remove(character_offset(text, *cursor));
            }
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let offset = character_offset(text, *cursor);
            text.insert(offset, character);
            *cursor += 1;
        }
        _ => return false,
    }
    true
}

/// The byte offset of character `index`, or the end of `text` when it is past the last character.
fn character_offset(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(offset, _)| offset)
}

fn caret_end(text: &str) -> usize {
    text.chars().count()
}

/// The caret position at the end of the mapping row with focus; the master toggle has no text.
fn mapping_row_caret_end(mapping: &ModelMappingForm) -> usize {
    match mapping.field {
        0 => 0,
        1 => caret_end(&mapping.default_model),
        field => caret_end(&mapping.rows[field - 2].model),
    }
}

/// The text of the form field with focus; the auth scheme field carries none.
fn form_field_text(form: &ProviderForm) -> &str {
    match form.field {
        0 => &form.base_url,
        1 => &form.secret,
        2 => &form.name,
        3 => &form.description,
        _ => "",
    }
}

fn take_submission(form: &mut FormSubmission) -> FormSubmission {
    FormSubmission {
        id: form.id.take(),
        revision: form.revision,
        client: form.client,
        name: std::mem::take(&mut form.name),
        description: std::mem::take(&mut form.description),
        base_url: std::mem::take(&mut form.base_url),
        auth_scheme: form.auth_scheme,
        secret: std::mem::take(&mut form.secret),
        model: std::mem::take(&mut form.model),
        claude_model_mapping: std::mem::take(&mut form.claude_model_mapping),
    }
}

pub(super) fn visible_models(picker: &ModelPicker) -> Vec<&str> {
    let query = picker.query.to_ascii_lowercase();
    picker
        .models
        .iter()
        .filter(|model| query.is_empty() || model.to_ascii_lowercase().contains(&query))
        .map(String::as_str)
        .collect()
}

fn visible_model_count(picker: &ModelPicker) -> usize {
    visible_models(picker).len()
}

fn selected_model(picker: &ModelPicker) -> Option<&str> {
    picker
        .selected
        .checked_sub(1)
        .and_then(|index| visible_models(picker).get(index).copied())
}
