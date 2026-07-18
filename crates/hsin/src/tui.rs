use std::{io, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use hsin_core::{
    AuthScheme, ClientKind, ConnectionMode, ModeSetParams, Provider, ProviderAddParams,
    ProviderDraft, ProviderEditParams, ProviderPatch, ProviderRemoveParams, ProviderSwitchParams,
    SecretInput, Settings, SettingsPatch,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use serde_json::Value;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::{
    i18n::I18n,
    rpc::{DaemonClient, StatusSnapshot},
};

const RED: Color = Color::Rgb(205, 58, 69);
const WHITE: Color = Color::Rgb(235, 235, 235);
const MUTED: Color = Color::Rgb(130, 130, 140);

pub async fn run(client: DaemonClient, i18n: &I18n) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let _restore = RestoreTerminal;
    let (effect_tx, effect_rx) = mpsc::channel(16);
    let (action_tx, mut action_rx) = mpsc::channel(16);
    tokio::spawn(worker(client, effect_rx, action_tx));

    let mut state = State::default();
    effect_tx.send(Effect::Refresh).await?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    loop {
        terminal.draw(|frame| draw(frame, &mut state, i18n))?;
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if matches!(state.reduce(Action::Key(key)), Transition::Quit) {
                            break;
                        }
                        if let Some(effect) = state.take_effect() {
                            effect_tx.send(effect).await?;
                        }
                    }
                    Some(Err(error)) => return Err(error).context("read terminal event"),
                    None => break,
                    _ => {}
                }
            }
            action = action_rx.recv() => {
                let Some(action) = action else { break };
                state.reduce(action);
            }
            _ = tick.tick() => {}
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Debug)]
enum Effect {
    Refresh,
    Switch {
        client: ClientKind,
        id: String,
    },
    SetMode {
        client: ClientKind,
        mode: ConnectionMode,
    },
    Add(FormSubmission),
    Edit(FormSubmission),
    Remove {
        id: String,
        expected_revision: u64,
    },
    SetLanguage(String),
}

#[derive(Debug)]
enum Action {
    Key(KeyEvent),
    Loaded {
        providers: Vec<Provider>,
        status: StatusSnapshot,
        settings: Settings,
    },
    Notice(&'static str),
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Transition {
    Continue,
    Quit,
}

#[derive(Debug)]
struct State {
    client: ClientKind,
    providers: Vec<Provider>,
    selected: usize,
    status: StatusSnapshot,
    language: String,
    loading: bool,
    notice: Option<String>,
    input: InputMode,
    pending_effect: Option<Effect>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            client: ClientKind::Codex,
            providers: Vec::new(),
            selected: 0,
            status: StatusSnapshot::default(),
            language: String::from("en-US"),
            loading: true,
            notice: None,
            input: InputMode::Normal,
            pending_effect: None,
        }
    }
}

#[derive(Debug, Default)]
enum InputMode {
    #[default]
    Normal,
    Search(String),
    Form(ProviderForm),
    DeleteConfirm {
        id: String,
        revision: u64,
    },
}

#[derive(Debug)]
struct ProviderForm {
    id: Option<String>,
    revision: Option<u64>,
    client: ClientKind,
    name: String,
    base_url: String,
    auth_scheme: AuthScheme,
    secret: Zeroizing<String>,
    field: usize,
}

#[derive(Debug)]
struct FormSubmission {
    id: Option<String>,
    revision: Option<u64>,
    client: ClientKind,
    name: String,
    base_url: String,
    auth_scheme: AuthScheme,
    secret: Zeroizing<String>,
}

impl State {
    fn reduce(&mut self, action: Action) -> Transition {
        match action {
            Action::Loaded {
                providers,
                status,
                settings,
            } => {
                self.providers = providers;
                self.status = status;
                self.language = settings.language;
                self.loading = false;
                self.clamp_selection();
            }
            Action::Notice(key) => {
                self.notice = Some(format!("@{key}"));
                self.loading = false;
            }
            Action::Failed(message) => {
                self.notice = Some(message);
                self.loading = false;
            }
            Action::Key(key) => return self.reduce_key(key),
        }
        Transition::Continue
    }

    #[allow(clippy::too_many_lines)]
    fn reduce_key(&mut self, key: KeyEvent) -> Transition {
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
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % 4,
                KeyCode::BackTab | KeyCode::Up => form.field = (form.field + 3) % 4,
                KeyCode::Backspace => match form.field {
                    0 => {
                        form.name.pop();
                    }
                    1 => {
                        form.base_url.pop();
                    }
                    2 => {
                        form.secret.pop();
                    }
                    _ => {}
                },
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 3 => {
                    form.auth_scheme = match form.auth_scheme {
                        AuthScheme::Bearer => AuthScheme::XApiKey,
                        AuthScheme::XApiKey => AuthScheme::Bearer,
                    };
                }
                KeyCode::Char(character) => match form.field {
                    0 => form.name.push(character),
                    1 => form.base_url.push(character),
                    2 => form.secret.push(character),
                    _ => {}
                },
                KeyCode::Enter if form.field < 3 => form.field += 1,
                KeyCode::Enter if !form.name.is_empty() && !form.base_url.is_empty() => {
                    let submission = FormSubmission {
                        id: form.id.clone(),
                        revision: form.revision,
                        client: form.client,
                        name: std::mem::take(&mut form.name),
                        base_url: std::mem::take(&mut form.base_url),
                        auth_scheme: form.auth_scheme,
                        secret: std::mem::take(&mut form.secret),
                    };
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
            InputMode::Normal => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Transition::Quit,
                KeyCode::Tab => {
                    self.client = match self.client {
                        ClientKind::Codex => ClientKind::Claude,
                        ClientKind::Claude => ClientKind::Codex,
                    };
                    self.selected = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let len = self.visible_providers().len();
                    if len > 0 {
                        self.selected = (self.selected + 1).min(len - 1);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Char('r') => self.queue(Effect::Refresh),
                KeyCode::Char('/') => self.input = InputMode::Search(String::new()),
                KeyCode::Char('a') => {
                    self.input = InputMode::Form(ProviderForm {
                        id: None,
                        revision: None,
                        client: self.client,
                        name: String::new(),
                        base_url: String::new(),
                        auth_scheme: match self.client {
                            ClientKind::Codex => AuthScheme::Bearer,
                            ClientKind::Claude => AuthScheme::XApiKey,
                        },
                        secret: Zeroizing::new(String::new()),
                        field: 0,
                    });
                }
                KeyCode::Char('e') => {
                    if let Some(provider) = self.selected_provider().cloned() {
                        self.input = InputMode::Form(ProviderForm {
                            id: Some(provider.id),
                            revision: Some(provider.revision),
                            client: provider.client,
                            name: provider.name,
                            base_url: provider.base_url,
                            auth_scheme: provider.auth_scheme,
                            secret: Zeroizing::new(String::new()),
                            field: 0,
                        });
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(provider) = self.selected_provider() {
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
                KeyCode::Char('m') => {
                    let mode = match self.mode() {
                        ConnectionMode::Direct => ConnectionMode::Proxy,
                        ConnectionMode::Proxy => ConnectionMode::Direct,
                    };
                    self.queue(Effect::SetMode {
                        client: self.client,
                        mode,
                    });
                }
                KeyCode::Char('l') => {
                    let language = if self.language.eq_ignore_ascii_case("zh-CN") {
                        String::from("en-US")
                    } else {
                        String::from("zh-CN")
                    };
                    self.queue(Effect::SetLanguage(language));
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

    fn take_effect(&mut self) -> Option<Effect> {
        self.pending_effect.take()
    }

    fn visible_providers(&self) -> Vec<&Provider> {
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

    fn selected_provider(&self) -> Option<&Provider> {
        self.visible_providers().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_providers().len().saturating_sub(1));
    }

    fn mode(&self) -> ConnectionMode {
        match self.client {
            ClientKind::Codex => self.status.codex_mode,
            ClientKind::Claude => self.status.claude_mode,
        }
    }

    fn active_id(&self) -> Option<&str> {
        match self.client {
            ClientKind::Codex => self.status.codex_active_provider.as_deref(),
            ClientKind::Claude => self.status.claude_active_provider.as_deref(),
        }
    }
}

async fn worker(
    client: DaemonClient,
    mut effects: mpsc::Receiver<Effect>,
    actions: mpsc::Sender<Action>,
) {
    while let Some(effect) = effects.recv().await {
        let result = execute_effect(&client, effect).await;
        match result {
            Ok(notice) => {
                if let Some(notice) = notice {
                    let _ = actions.send(Action::Notice(notice)).await;
                }
                match load(&client).await {
                    Ok((providers, status, settings)) => {
                        let _ = actions
                            .send(Action::Loaded {
                                providers,
                                status,
                                settings,
                            })
                            .await;
                    }
                    Err(error) => {
                        let _ = actions.send(Action::Failed(format!("{error:#}"))).await;
                    }
                }
            }
            Err(error) => {
                let _ = actions.send(Action::Failed(format!("{error:#}"))).await;
            }
        }
    }
}

async fn execute_effect(client: &DaemonClient, effect: Effect) -> Result<Option<&'static str>> {
    match effect {
        Effect::Refresh => Ok(None),
        Effect::Switch { client: kind, id } => {
            let _: Value = client
                .call(
                    "provider.switch",
                    &ProviderSwitchParams {
                        client: kind,
                        provider_id: id,
                    },
                )
                .await?;
            Ok(Some("switched"))
        }
        Effect::SetMode { client: kind, mode } => {
            let _: Value = client
                .call("mode.set", &ModeSetParams { client: kind, mode })
                .await?;
            Ok(Some("mode_changed"))
        }
        Effect::Add(form) => {
            let request = ProviderAddParams {
                provider: ProviderDraft {
                    client: form.client,
                    name: form.name,
                    base_url: form.base_url,
                    auth_scheme: form.auth_scheme,
                },
                secret: if form.secret.is_empty() {
                    SecretInput::Clear
                } else {
                    SecretInput::Replace(form.secret.to_string())
                },
            };
            let _: Value = client.call("provider.add", &request).await?;
            Ok(Some("provider_added"))
        }
        Effect::Edit(form) => {
            let id = form.id.context("edit form is missing provider ID")?;
            let expected_revision = form
                .revision
                .context("edit form is missing provider revision")?;
            let request = ProviderEditParams {
                id,
                expected_revision,
                patch: ProviderPatch {
                    name: Some(form.name),
                    base_url: Some(form.base_url),
                    auth_scheme: Some(form.auth_scheme),
                },
                secret: if form.secret.is_empty() {
                    SecretInput::Preserve
                } else {
                    SecretInput::Replace(form.secret.to_string())
                },
            };
            let _: Value = client.call("provider.edit", &request).await?;
            Ok(Some("provider_updated"))
        }
        Effect::Remove {
            id,
            expected_revision,
        } => {
            let _: Value = client
                .call(
                    "provider.remove",
                    &ProviderRemoveParams {
                        id,
                        expected_revision,
                    },
                )
                .await?;
            Ok(Some("provider_removed"))
        }
        Effect::SetLanguage(language) => {
            let _: Value = client
                .call(
                    "settings.set",
                    &SettingsPatch {
                        language: Some(language),
                        proxy_port: None,
                    },
                )
                .await?;
            Ok(Some("language_changed"))
        }
    }
}

async fn load(client: &DaemonClient) -> Result<(Vec<Provider>, StatusSnapshot, Settings)> {
    let providers = client.provider_list(None).await?;
    let status = client.status().await?;
    let settings = client.call("settings.get", &serde_json::json!({})).await?;
    Ok((providers, status, settings))
}

fn draw(frame: &mut Frame<'_>, state: &mut State, i18n: &I18n) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(3),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            i18n.text("title"),
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ◆  ", Style::default().fg(RED)),
        Span::styled(i18n.text("subtitle"), Style::default().fg(MUTED)),
    ]))
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(RED)),
    );
    frame.render_widget(title, rows[0]);

    let selected_tab = usize::from(state.client == ClientKind::Claude);
    let tabs = Tabs::new(vec![i18n.text("codex"), i18n.text("claude")])
        .select(selected_tab)
        .highlight_style(Style::default().fg(RED).add_modifier(Modifier::BOLD))
        .divider(" │ ")
        .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(tabs, rows[1]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(rows[2]);
    draw_provider_list(frame, columns[0], state, i18n);
    draw_details(frame, columns[1], state, i18n);

    let footer = state.notice.as_deref().map_or_else(
        || i18n.text("help"),
        |notice| {
            notice
                .strip_prefix('@')
                .map_or(notice, |key| i18n.text(key))
        },
    );
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(if state.notice.is_some() { RED } else { MUTED }))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::TOP)),
        rows[3],
    );

    match &state.input {
        InputMode::Search(query) => draw_search(frame, area, query),
        InputMode::Form(form) => draw_form(frame, area, form),
        InputMode::DeleteConfirm { .. } => draw_confirm(frame, area, i18n),
        InputMode::Normal => {}
    }
}

fn draw_provider_list(frame: &mut Frame<'_>, area: Rect, state: &mut State, i18n: &I18n) {
    let active = state.active_id().map(str::to_owned);
    let providers = state.visible_providers();
    let items = if providers.is_empty() {
        vec![ListItem::new(if state.loading {
            i18n.text("loading")
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
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} "), Style::default().fg(RED)),
                    Span::styled(&provider.name, Style::default().fg(WHITE)),
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

fn draw_details(frame: &mut Frame<'_>, area: Rect, state: &State, i18n: &I18n) {
    let mode = match state.mode() {
        ConnectionMode::Direct => i18n.text("direct"),
        ConnectionMode::Proxy => i18n.text("proxy"),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{}: ", i18n.text("mode")),
                Style::default().fg(MUTED),
            ),
            Span::styled(mode, Style::default().fg(RED)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("{}: ", i18n.text("language")),
                Style::default().fg(MUTED),
            ),
            Span::styled(&state.language, Style::default().fg(WHITE)),
        ]),
        Line::from(""),
    ];
    if let Some(provider) = state.selected_provider() {
        lines.push(Line::from(Span::styled(
            &provider.name,
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(provider.base_url.as_str()));
        lines.push(Line::from(format!("auth: {:?}", provider.auth_scheme)));
        lines.push(Line::from(format!("revision: {}", provider.revision)));
    }
    if state.status.security_locked {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "LOCKED",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )));
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

fn draw_search(frame: &mut Frame<'_>, area: Rect, query: &str) {
    let popup = centered(area, 60, 3);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("/ {query}")).block(
            Block::default()
                .title("Search")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(RED)),
        ),
        popup,
    );
}

fn draw_form(frame: &mut Frame<'_>, area: Rect, form: &ProviderForm) {
    let popup = centered(area, 72, 13);
    frame.render_widget(Clear, popup);
    let hidden = "•".repeat(form.secret.chars().count());
    let auth = match form.auth_scheme {
        AuthScheme::Bearer => "Bearer",
        AuthScheme::XApiKey => "X-API-Key",
    };
    let fields = [
        ("Name", form.name.as_str()),
        ("Base URL", form.base_url.as_str()),
        ("API key", hidden.as_str()),
        ("Auth (Left/Right)", auth),
    ];
    let lines = fields
        .into_iter()
        .enumerate()
        .flat_map(|(index, (label, value))| {
            let style = if form.field == index {
                Style::default().fg(RED).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(WHITE)
            };
            [Line::from(Span::styled(label, style)), Line::from(value)]
        })
        .collect::<Vec<_>>();
    let title = if form.id.is_some() {
        "Edit provider"
    } else {
        "Add provider"
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(RED)),
        ),
        popup,
    );
}

fn draw_confirm(frame: &mut Frame<'_>, area: Rect, i18n: &I18n) {
    let popup = centered(area, 66, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(i18n.text("confirm_delete"))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title("Confirm")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(RED)),
            ),
        popup,
    );
}

fn centered(area: Rect, width_percent: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Min(0),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Action {
        Action::Key(KeyEvent::new(code, crossterm::event::KeyModifiers::NONE))
    }

    #[test]
    fn reducer_switches_client_and_moves_selection() {
        let mut state = State::default();
        state.reduce(key(KeyCode::Tab));
        assert_eq!(state.client, ClientKind::Claude);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn reducer_queues_mode_change() {
        let mut state = State::default();
        state.reduce(key(KeyCode::Char('m')));
        assert!(matches!(
            state.take_effect(),
            Some(Effect::SetMode {
                mode: ConnectionMode::Proxy,
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
    fn renders_provider_and_compact_windows() {
        let mut state = State {
            loading: false,
            ..State::default()
        };
        state.providers.push(Provider {
            id: String::from("provider-1"),
            client: ClientKind::Codex,
            name: String::from("Example"),
            base_url: String::from("https://api.example.test/v1"),
            auth_scheme: AuthScheme::Bearer,
            revision: 1,
        });
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

        let mut compact = Terminal::new(TestBackend::new(30, 8)).expect("compact terminal");
        compact
            .draw(|frame| draw(frame, &mut state, &locale))
            .expect("draw compact terminal");
    }
}
