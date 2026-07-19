use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hsin_core::{
    AuthScheme, ClientKind, ClientSettings, ConnectionMode, LANGUAGE_EN_US, LANGUAGE_SYSTEM,
    LANGUAGE_ZH_CN, ModelDiscovery, ModelUpdate, Provider, Settings,
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
}

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
    pub(super) loading: bool,
    pub(super) notice: Option<String>,
    pub(super) input: InputMode,
    pub(super) pending_effect: Option<Effect>,
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
            loading: true,
            notice: None,
            input: InputMode::Normal,
            pending_effect: None,
        }
    }
}

#[derive(Default)]
pub(super) enum InputMode {
    #[default]
    Normal,
    Search(String),
    Form(ProviderForm),
    Models(ModelPicker),
    DeleteConfirm {
        id: String,
        revision: u64,
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
        port: String,
        editing_port: bool,
    },
    Language {
        selected: usize,
    },
    Clients {
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
    Import {
        selected: usize,
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
    pub(super) field: usize,
    pub(super) error: Option<&'static str>,
    pub(super) secret_visible: bool,
    pub(super) discovering_models: bool,
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
}

pub(super) struct ModelPicker {
    pub(super) form: FormSubmission,
    pub(super) models: Vec<String>,
    pub(super) selected: usize,
    pub(super) query: String,
    pub(super) mode: ModelPickerMode,
    pub(super) warning: Option<String>,
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
            } => {
                self.providers = providers;
                self.status = status;
                self.language = settings.language;
                self.proxy_enabled = settings.proxy_enabled;
                self.proxy_host = settings.proxy_host;
                self.proxy_port = settings.proxy_port;
                self.client_settings = settings.clients;
                if !self.client_settings.visible.contains(&self.client)
                    && let Some(client) = self.client_settings.visible_in_order().first().copied()
                {
                    self.client = client;
                }
                if let InputMode::Settings(SettingsScreen {
                    page:
                        SettingsPage::Proxy {
                            port,
                            editing_port: false,
                            ..
                        },
                    ..
                }) = &mut self.input
                {
                    *port = self.proxy_port.to_string();
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
                if let Some(active) = self.active_id()
                    && let Some(index) = self
                        .visible_providers()
                        .iter()
                        .position(|provider| provider.id == active)
                {
                    self.selected = index;
                }
            }
            Action::Notice(key) => {
                self.notice = Some(format!("@{key}"));
                self.loading = false;
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
                    warning: Some(message),
                });
                self.loading = false;
                self.notice = None;
            }
            Action::Key(key) => return self.reduce_key(key),
        }
        Transition::Continue
    }

    #[allow(clippy::too_many_lines)]
    fn reduce_key(&mut self, key: KeyEvent) -> Transition {
        if matches!(&self.input, InputMode::Form(form) if form.discovering_models) {
            return Transition::Continue;
        }
        let current_mode = self.mode();
        let proxy_enabled = self.proxy_enabled;
        let proxy_port = self.proxy_port;
        let client_settings = self.client_settings.clone();
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
            InputMode::Search(query) => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.input = InputMode::Normal,
                KeyCode::Backspace => {
                    query.pop();
                    self.selected = 0;
                }
                KeyCode::Char(character) => {
                    query.push(character);
                    self.selected = 0;
                }
                _ => {}
            },
            InputMode::Form(form) => match key.code {
                KeyCode::Char('h' | 'H') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.secret_visible = !form.secret_visible;
                }
                KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_form_field(form);
                }
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Tab | KeyCode::Down => {
                    form.field = (form.field + 1) % 5;
                    form.error = None;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.field = (form.field + 4) % 5;
                    form.error = None;
                }
                KeyCode::Backspace => match form.field {
                    0 => {
                        form.base_url.pop();
                    }
                    1 => {
                        form.secret.pop();
                    }
                    2 => {
                        form.name.pop();
                    }
                    3 => {
                        form.description.pop();
                    }
                    _ => {}
                },
                KeyCode::Left | KeyCode::Right | KeyCode::Char('j' | 'l' | ' ')
                    if form.field == 4 =>
                {
                    form.auth_scheme = match form.auth_scheme {
                        AuthScheme::Bearer => AuthScheme::XApiKey,
                        AuthScheme::XApiKey | AuthScheme::OAuth => AuthScheme::Bearer,
                    };
                }
                KeyCode::Char(character) => match form.field {
                    0 => form.base_url.push(character),
                    1 => form.secret.push(character),
                    2 => form.name.push(character),
                    3 => form.description.push(character),
                    _ => {}
                },
                KeyCode::Enter => match take_form_submission(form) {
                    Ok(submission) => {
                        let discovering_models = submission.client == ClientKind::Codex;
                        self.pending_effect = Some(if discovering_models {
                            Effect::DiscoverModels(submission)
                        } else if submission.id.is_some() {
                            Effect::Edit(submission)
                        } else {
                            Effect::Add(submission)
                        });
                        self.loading = true;
                        if discovering_models {
                            form.discovering_models = true;
                            self.notice = Some("@fetching_models".into());
                        } else {
                            self.notice = None;
                            self.input = InputMode::Normal;
                        }
                    }
                    Err(error) => form.error = Some(error),
                },
                _ => {}
            },
            InputMode::Models(picker) => match &mut picker.mode {
                ModelPickerMode::Browse => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        self.input = InputMode::Normal;
                    }
                    KeyCode::Char('s') => picker.mode = ModelPickerMode::Search,
                    KeyCode::Char('m') => {
                        picker.mode = ModelPickerMode::Manual(String::new());
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
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.selected = 0;
                    }
                    KeyCode::Char(character) => {
                        picker.query.push(character);
                        picker.selected = 0;
                    }
                    _ => {}
                },
                ModelPickerMode::Manual(value) => match key.code {
                    KeyCode::Esc => picker.mode = ModelPickerMode::Browse,
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char(character) => value.push(character),
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
                    _ => {}
                },
            },
            InputMode::DeleteConfirm { id, revision } => {
                if key.code == KeyCode::Char('d') {
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
                        screen.selected = (screen.selected + 1).min(3);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match screen.selected {
                        0 => {
                            screen.page = SettingsPage::Proxy {
                                selected: 0,
                                port: proxy_port.to_string(),
                                editing_port: false,
                            };
                        }
                        1 => {
                            screen.page = SettingsPage::Clients { selected: 0 };
                        }
                        2 => {
                            screen.page = SettingsPage::Language {
                                selected: language_selected,
                            };
                        }
                        _ => {
                            screen.page = SettingsPage::Import { selected: 0 };
                        }
                    },
                    _ => {}
                },
                SettingsPage::Proxy {
                    selected,
                    port,
                    editing_port,
                } => {
                    if *editing_port {
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
                                        self.notice = Some("@proxy_address_read_only".into());
                                    }
                                    _ if proxy_enabled => {
                                        self.notice = Some("@proxy_port_disable_first".into());
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
                        *selected = (*selected + 1).min(1);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        screen.page = if *selected == 0 {
                            SettingsPage::ClientVisibility { selected: 0 }
                        } else {
                            SettingsPage::ClientOrder {
                                selected: 0,
                                order: client_settings.order.clone(),
                                moving: false,
                            }
                        };
                    }
                    _ => {}
                },
                SettingsPage::ClientVisibility { selected } => match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('j') => {
                        screen.page = SettingsPage::Clients { selected: 0 };
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
                                screen.page = SettingsPage::Clients { selected: 1 };
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
                SettingsPage::Import { selected } => match key.code {
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
                        self.pending_effect = Some(match *selected {
                            0 => Effect::ImportCurrent(ClientKind::Codex),
                            1 => Effect::ImportCurrent(ClientKind::Claude),
                            _ => Effect::ImportAll,
                        });
                        self.loading = true;
                        screen.page = SettingsPage::Root;
                    }
                    _ => {}
                },
            },
            InputMode::Normal => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Transition::Quit,
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.client = self.previous_visible_client();
                    self.selected = 0;
                }
                KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                    self.client = self.next_visible_client();
                    self.selected = 0;
                }
                KeyCode::BackTab | KeyCode::Left | KeyCode::Char('j') => {
                    self.client = self.previous_visible_client();
                    self.selected = 0;
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
                KeyCode::Char('/') => self.input = InputMode::Search(String::new()),
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
                        field: 0,
                        error: None,
                        secret_visible: false,
                        discovering_models: false,
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
                        self.input = InputMode::Form(ProviderForm {
                            id: Some(provider.id),
                            revision: Some(provider.revision),
                            client: provider.client,
                            name,
                            description: provider.description,
                            base_url: provider.base_url,
                            auth_scheme: provider.auth_scheme,
                            secret: Zeroizing::new(String::new()),
                            field: 0,
                            error: None,
                            secret_visible: false,
                            discovering_models: false,
                        });
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(provider) = self.selected_provider() {
                        if provider.official {
                            self.notice = Some("@official_read_only".into());
                            return Transition::Continue;
                        }
                        self.input = InputMode::DeleteConfirm {
                            id: provider.id.clone(),
                            revision: provider.revision,
                        };
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

    pub(super) fn visible_providers(&self) -> Vec<&Provider> {
        let query = match &self.input {
            InputMode::Search(query) if !query.is_empty() => Some(query.to_ascii_lowercase()),
            _ => None,
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
    if form.id.is_none() && form.secret.trim().is_empty() {
        return Err("validation_api_key_required");
    }
    Ok(FormSubmission {
        id: form.id.clone(),
        revision: form.revision,
        client: form.client,
        name,
        description: form.description.clone(),
        base_url: base_url.trim_end_matches('/').to_owned(),
        auth_scheme: form.auth_scheme,
        secret: std::mem::take(&mut form.secret),
        model: ModelUpdate::Preserve,
    })
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
