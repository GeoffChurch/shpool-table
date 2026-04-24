use crate::session::Session;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    CreateInput(String),
    ConfirmKill(String),
    ConfirmForce(String),
}

pub struct Model {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub mode: Mode,
    /// Transient error message displayed in the bottom bar until the
    /// next keypress. Set by failed shell-outs and pre-flight checks.
    pub error: Option<String>,
}

impl Model {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self { sessions, selected: 0, mode: Mode::Normal, error: None }
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }

    pub fn select_next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.sessions.len();
    }

    pub fn select_prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.sessions.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.sessions.get(self.selected).map(|s| s.name.as_str())
    }

    /// Replace the session list, preserving the selection by name when
    /// possible and otherwise clamping the previous index into the new
    /// list. Sessions are sorted with the most recently active first,
    /// so whichever session you last touched floats to the top.
    pub fn refresh(&mut self, mut new_sessions: Vec<Session>) {
        new_sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active_unix_ms()));
        let prev_name = self.selected_name().map(str::to_string);
        let prev_idx = self.selected;
        self.sessions = new_sessions;
        self.selected = prev_name
            .and_then(|name| self.sessions.iter().position(|s| s.name == name))
            .unwrap_or_else(|| prev_idx.min(self.sessions.len().saturating_sub(1)));
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

    #[test]
    fn select_next_wraps() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        assert_eq!(m.selected, 0);
        m.select_next();
        assert_eq!(m.selected, 1);
        m.select_next();
        assert_eq!(m.selected, 2);
        m.select_next();
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn select_prev_wraps() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.select_prev();
        assert_eq!(m.selected, 2);
        m.select_prev();
        assert_eq!(m.selected, 1);
    }

    #[test]
    fn empty_model_is_noop() {
        let mut m = Model::new(vec![]);
        m.select_next();
        m.select_prev();
        assert_eq!(m.selected, 0);
        assert_eq!(m.selected_name(), None);
    }
}
