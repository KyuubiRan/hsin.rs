use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{layout::Rect, widgets::ListState};

use super::state::HomeSection;

/// What a mouse click lands on. Every region maps back onto the keyboard model: clicks move the
/// same cursors and press the same keys, so the reducer has one source of truth for behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Hit {
    /// Covers everything drawn before a modal, so a click beside a dialog cannot reach the screen
    /// underneath it.
    Barrier,
    Key(KeyEvent),
    Section(HomeSection),
    /// A row of a list or form navigated with ↑/↓. `selected` is the cursor when the frame was
    /// drawn; clicking the selected row again presses `activate`, which is `None` where a second
    /// click must not act, such as a form field that would otherwise submit the form.
    Row {
        index: usize,
        selected: usize,
        activate: Option<KeyEvent>,
    },
    /// A quick range chip on the stats screen; the value indexes the time popup.
    StatsRange(usize),
    /// A heatmap cell; clicking or hovering shows that day.
    HeatDay(chrono::NaiveDate),
}

/// The clickable regions of the last frame, in draw order. Later regions sit on top.
#[derive(Debug, Default)]
pub(super) struct HitMap {
    regions: Vec<(Rect, Hit)>,
}

impl HitMap {
    pub(super) fn push(&mut self, area: Rect, hit: Hit) {
        if area.width > 0 && area.height > 0 {
            self.regions.push((area, hit));
        }
    }

    pub(super) fn key(&mut self, area: Rect, key: KeyEvent) {
        self.push(area, Hit::Key(key));
    }

    pub(super) fn barrier(&mut self, area: Rect) {
        self.push(area, Hit::Barrier);
    }

    /// Registers the rows a `List` actually drew, reading the scroll offset ratatui settled on.
    pub(super) fn list(
        &mut self,
        area: Rect,
        state: &ListState,
        heights: impl IntoIterator<Item = usize>,
        activate: Option<KeyEvent>,
    ) {
        let Some(selected) = state.selected() else {
            return;
        };
        let mut y = area.y;
        for (index, height) in heights.into_iter().enumerate().skip(state.offset()) {
            if y >= area.bottom() {
                break;
            }
            let height = u16::try_from(height)
                .unwrap_or(u16::MAX)
                .min(area.bottom() - y);
            self.push(
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                },
                Hit::Row {
                    index,
                    selected,
                    activate,
                },
            );
            y = y.saturating_add(height);
        }
    }

    /// Registers rows laid out by `scrolling_rows`.
    pub(super) fn rows(
        &mut self,
        rows: &[(usize, Rect)],
        selected: usize,
        activate: Option<KeyEvent>,
    ) {
        for &(index, area) in rows {
            self.push(
                area,
                Hit::Row {
                    index,
                    selected,
                    activate,
                },
            );
        }
    }

    pub(super) fn at(&self, column: u16, row: u16) -> Option<&Hit> {
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| {
                column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
            })
            .map(|(_, hit)| hit)
    }

    #[cfg(test)]
    pub(super) fn find(&self, predicate: impl Fn(&Hit) -> bool) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, hit)| predicate(hit))
            .map(|(area, _)| *area)
    }
}

pub(super) const fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) const ENTER: Option<KeyEvent> = Some(plain(KeyCode::Enter));

/// The key a footer hint such as `enter apply`, `s/esc back` or `ctrl+u clear` advertises.
/// Hints that name a pair of arrows, or describe typing rather than a key, are not clickable.
pub(super) fn hint_key(token: &str) -> Option<KeyEvent> {
    if token == "/" {
        return Some(plain(KeyCode::Char('/')));
    }
    let mut alternatives = token.split('/');
    let first = alternatives.next()?;
    if token.contains('/') && token.split('/').all(|part| arrow(part).is_some()) {
        return None;
    }
    if let Some(code) = arrow(first) {
        return Some(plain(code));
    }
    let code = match first {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        _ => {
            if let Some(letter) = first.strip_prefix("ctrl+")
                && let [letter] = letter.chars().collect::<Vec<_>>()[..]
                && letter.is_ascii_alphabetic()
            {
                return Some(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL));
            }
            let mut characters = first.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) if character.is_ascii_graphic() => KeyCode::Char(character),
                _ => return None,
            }
        }
    };
    Some(plain(code))
}

fn arrow(token: &str) -> Option<KeyCode> {
    match token {
        "↑" => Some(KeyCode::Up),
        "↓" => Some(KeyCode::Down),
        "←" => Some(KeyCode::Left),
        "→" => Some(KeyCode::Right),
        _ => None,
    }
}
