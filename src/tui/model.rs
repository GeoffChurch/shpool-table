use crate::session::Session;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    CreateInput(String),
    ConfirmKill(String),
    ConfirmForce(String),
}

/// Where the cursor is, as a three-state value rather than a bare
/// index — so "nothing is validly selected" can never be confused with
/// "row 0", which is the bug a clamped index invites: when the selected
/// session vanishes from a refresh, clamping silently lands the cursor
/// on whatever shifted into that slot, and the next attach/kill hits the
/// wrong session.
#[derive(Debug, PartialEq)]
pub enum Selection {
    /// Cursor on a valid row.
    At(usize),
    /// Deliberately nothing selected: an empty list, or the user just
    /// killed the last/only session. Attaching/killing is a no-op and
    /// no acknowledgment is required.
    None,
    /// The selected session disappeared from a refresh the user didn't
    /// initiate (another client killed it, an event-driven race).
    /// Carries the lost name for the "is gone" error. The highlight is
    /// suppressed and the next keystroke is consumed as acknowledgment
    /// (see update.rs) before any attach/kill can land — so the action
    /// never strikes whatever moved into that row.
    Stale(String),
}

pub struct Model {
    pub sessions: Vec<Session>,
    pub selection: Selection,
    pub mode: Mode,
    /// Transient error message displayed in the bottom bar until the
    /// next keypress. Set by failed shell-outs and pre-flight checks.
    pub error: Option<String>,
    /// Set by Command::Quit's executor. The main loop checks this
    /// after each render and exits if true. A flag rather than a
    /// loop-break return so the cascade can produce other commands
    /// around a Quit without losing them.
    pub quit: bool,
    /// True while a `shpool events` subscription is feeding push-driven
    /// refreshes. The subscriber child + its pipe live in the main loop
    /// (src/main.rs); this is the model's mirror of that state so the
    /// pure core can skip the keystroke/focus auto-refresh the event
    /// stream makes redundant, and fall back to it when the stream is
    /// unavailable.
    pub events_active: bool,
}

impl Model {
    pub fn new(sessions: Vec<Session>) -> Self {
        let selection =
            if sessions.is_empty() { Selection::None } else { Selection::At(0) };
        Self {
            sessions,
            selection,
            mode: Mode::Normal,
            error: None,
            quit: false,
            events_active: false,
        }
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }

    /// The highlighted row index, or `None` in the empty / cleared /
    /// stale states. The view uses this so nothing lights up when
    /// there's no valid selection.
    pub fn selected_index(&self) -> Option<usize> {
        match self.selection {
            Selection::At(i) => Some(i),
            _ => Option::None,
        }
    }

    /// The name of the highlighted session, or `None` when there's no
    /// valid selection. Returns `None` while stale, so attach/kill
    /// short-circuit instead of acting on the wrong session.
    pub fn selected_name(&self) -> Option<&str> {
        match self.selection {
            Selection::At(i) => self.sessions.get(i).map(|s| s.name.as_str()),
            _ => Option::None,
        }
    }

    /// True while the selection is in the unexpected-disappearance
    /// state, where the next keystroke is consumed as acknowledgment.
    pub fn is_stale(&self) -> bool {
        matches!(self.selection, Selection::Stale(_))
    }

    pub fn select_next(&mut self) {
        if self.sessions.is_empty() {
            self.selection = Selection::None;
            return;
        }
        let next = match self.selection {
            Selection::At(i) => (i + 1) % self.sessions.len(),
            // From no-valid-selection (cleared or stale), land on the
            // first row rather than tracking a remembered index.
            _ => 0,
        };
        self.selection = Selection::At(next);
    }

    pub fn select_prev(&mut self) {
        if self.sessions.is_empty() {
            self.selection = Selection::None;
            return;
        }
        let last = self.sessions.len() - 1;
        let prev = match self.selection {
            Selection::At(0) => last,
            Selection::At(i) => i - 1,
            _ => last,
        };
        self.selection = Selection::At(prev);
    }

    /// Move the cursor off `name` to a neighbor (vim `dd` semantics),
    /// or clear the selection if it was the only session. Called before
    /// issuing a kill of the *highlighted* session so the post-kill
    /// refresh re-selects the neighbor by name instead of raising a
    /// spurious stale alert for a disappearance the user caused.
    pub fn advance_off(&mut self, name: &str) {
        let Selection::At(i) = self.selection else { return };
        if self.sessions.get(i).map(|s| s.name.as_str()) != Some(name) {
            return;
        }
        let last = self.sessions.len() - 1;
        self.selection = if self.sessions.len() == 1 {
            Selection::None
        } else if i == last {
            Selection::At(i - 1)
        } else {
            Selection::At(i + 1)
        };
    }

    /// Replace the session list, most-recently-touched first, preserving
    /// the selection by name. If the previously-selected session is gone
    /// from a refresh the user didn't initiate, enter the Stale state —
    /// don't silently move the cursor onto whatever shifted into its
    /// place — and raise an error.
    pub fn refresh(&mut self, mut new_sessions: Vec<Session>) {
        new_sessions.sort_by_key(|s| std::cmp::Reverse(s.last_touched_unix_ms()));

        // Capture the prior selection's identity before swapping the
        // list out from under the index it points into.
        let prev_name = self.selected_name().map(str::to_string);
        let stale_name = match &self.selection {
            Selection::Stale(n) => Some(n.clone()),
            _ => Option::None,
        };
        self.sessions = new_sessions;

        // Had a valid selection: re-seat by name, or go Stale.
        if let Some(name) = prev_name {
            match self.sessions.iter().position(|s| s.name == name) {
                Some(i) => self.selection = Selection::At(i),
                Option::None => {
                    self.set_error(format!("session '{name}' is gone"));
                    self.selection = Selection::Stale(name);
                }
            }
            return;
        }

        // Already stale: keep waiting for the ack, unless the session
        // came back (recreated same-named) — then re-seat on it.
        if let Some(name) = stale_name {
            if let Some(i) = self.sessions.iter().position(|s| s.name == name) {
                self.selection = Selection::At(i);
            }
            return;
        }

        // Cleared or empty: land on the freshest row if any appeared,
        // else stay empty.
        self.selection = if self.sessions.is_empty() {
            Selection::None
        } else {
            Selection::At(0)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(name: &str) -> Session {
        Session {
            name: name.to_string(),
            attached: false,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
        }
    }

    /// A session with an explicit last-touched time, so refresh's sort
    /// order is deterministic in tests that care about it.
    fn mk_at(name: &str, touched: u64) -> Session {
        Session {
            name: name.to_string(),
            attached: false,
            started_at_unix_ms: touched,
            last_connected_at_unix_ms: touched,
            last_disconnected_at_unix_ms: None,
        }
    }

    #[test]
    fn select_next_wraps() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        assert_eq!(m.selection, Selection::At(0));
        m.select_next();
        assert_eq!(m.selection, Selection::At(1));
        m.select_next();
        assert_eq!(m.selection, Selection::At(2));
        m.select_next();
        assert_eq!(m.selection, Selection::At(0));
    }

    #[test]
    fn select_prev_wraps() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.select_prev();
        assert_eq!(m.selection, Selection::At(2));
        m.select_prev();
        assert_eq!(m.selection, Selection::At(1));
    }

    #[test]
    fn empty_model_has_no_selection() {
        let mut m = Model::new(vec![]);
        assert_eq!(m.selection, Selection::None);
        m.select_next();
        m.select_prev();
        assert_eq!(m.selection, Selection::None);
        assert_eq!(m.selected_name(), None);
        assert_eq!(m.selected_index(), None);
    }

    #[test]
    fn nav_from_stale_lands_on_an_edge() {
        // j from "nowhere" goes to the top, k to the bottom.
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::Stale("gone".into());
        m.select_next();
        assert_eq!(m.selection, Selection::At(0));
        m.selection = Selection::Stale("gone".into());
        m.select_prev();
        assert_eq!(m.selection, Selection::At(2));
    }

    #[test]
    fn refresh_preserves_selection_by_name() {
        let mut m = Model::new(vec![mk_at("a", 3), mk_at("b", 2), mk_at("c", 1)]);
        m.selection = Selection::At(2); // "c"
        // New list reorders; selection should track "c" by name.
        m.refresh(vec![mk_at("c", 9), mk_at("a", 3), mk_at("b", 2)]);
        assert_eq!(m.selected_name(), Some("c"));
        assert!(m.error.is_none());
    }

    #[test]
    fn refresh_marks_stale_when_selected_disappears() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::At(1); // "b"
        m.refresh(vec![mk("a"), mk("c")]);
        assert_eq!(m.selection, Selection::Stale("b".into()));
        assert_eq!(m.selected_name(), None); // no wrong-session action
        assert!(m.is_stale());
        assert!(m.error.as_deref().unwrap_or("").contains("'b' is gone"));
    }

    #[test]
    fn refresh_stale_clears_when_session_reappears() {
        let mut m = Model::new(vec![mk("a")]);
        m.selection = Selection::Stale("b".into());
        m.refresh(vec![mk("a"), mk("b")]);
        assert_eq!(m.selected_name(), Some("b"));
    }

    #[test]
    fn refresh_stale_persists_while_still_gone() {
        let mut m = Model::new(vec![mk("a")]);
        m.selection = Selection::Stale("b".into());
        m.refresh(vec![mk("a"), mk("c")]);
        assert_eq!(m.selection, Selection::Stale("b".into()));
    }

    #[test]
    fn advance_off_moves_to_next_neighbor() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::At(1); // "b"
        m.advance_off("b");
        assert_eq!(m.selected_name(), Some("c"));
    }

    #[test]
    fn advance_off_last_moves_to_previous() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::At(2); // "c"
        m.advance_off("c");
        assert_eq!(m.selected_name(), Some("b"));
    }

    #[test]
    fn advance_off_only_session_clears() {
        let mut m = Model::new(vec![mk("solo")]);
        m.advance_off("solo");
        assert_eq!(m.selection, Selection::None);
    }

    #[test]
    fn advance_off_then_kill_refresh_is_not_stale() {
        // The point of advance_off: after moving off "b" and refreshing
        // with "b" removed, the neighbor is re-selected by name and no
        // stale alert fires for the user's own kill.
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::At(1); // "b"
        m.advance_off("b");
        m.refresh(vec![mk("a"), mk("c")]);
        assert_eq!(m.selected_name(), Some("c"));
        assert!(!m.is_stale());
        assert!(m.error.is_none());
    }

    #[test]
    fn refresh_onto_empty_then_repopulated() {
        let mut m = Model::new(vec![]);
        assert_eq!(m.selection, Selection::None);
        m.refresh(vec![mk("a")]);
        assert_eq!(m.selected_name(), Some("a"));
    }
}
