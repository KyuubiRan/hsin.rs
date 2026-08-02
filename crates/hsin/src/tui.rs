use std::{io, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        Event, EventStream, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::{i18n::I18n, rpc::DaemonClient};

mod effects;
mod screens;
mod state;
mod theme;
mod widgets;

use effects::{Effect, worker};
use screens::draw;
use state::{Action, State, Transition};

pub async fn run(client: DaemonClient, i18n: &mut I18n, follow_saved_language: bool) -> Result<()> {
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
                    Some(Ok(event)) => {
                        if let Some(action) = key_action(&event) {
                            if matches!(reduce_action(&mut state, i18n, follow_saved_language, action), Transition::Quit) {
                                break;
                            }
                            if let Some(effect) = state.take_effect() {
                                effect_tx.send(effect).await?;
                            }
                        }
                    }
                    Some(Err(error)) => return Err(error).context("read terminal event"),
                    None => break,
                }
            }
            action = action_rx.recv() => {
                let Some(action) = action else { break };
                reduce_action(&mut state, i18n, follow_saved_language, action);
            }
            _ = tick.tick() => {
                reduce_action(&mut state, i18n, follow_saved_language, Action::Tick);
            }
        }
    }
    Ok(())
}

/// Held-down keys. `REPORT_EVENT_TYPES` makes terminals that speak the kitty keyboard protocol
/// split auto-repeat out as its own kind, so ignoring it froze backspace and the arrow keys under
/// a long press while letters — which still arrive as plain text presses — kept repeating.
fn key_action(event: &Event) -> Option<Action> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(Action::Key(*key))
        }
        _ => None,
    }
}

fn reduce_action(
    state: &mut State,
    i18n: &mut I18n,
    follow_saved_language: bool,
    action: Action,
) -> Transition {
    let previous_language = state.language.clone();
    let transition = state.reduce(action);
    if follow_saved_language && state.language != previous_language {
        i18n.set_language(&state.language);
    }
    transition
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        )
    );
    Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen
        );
    }
}

#[cfg(test)]
mod tests;
