//! The pure update function: `(&mut Model, Event) -> Option<Command>`.
//!
//! Free of I/O — no stdin reads, no shell-outs, no ANSI writes. All
//! the dispatch policy (which key does what in which mode, when to
//! clear the transient error, when to enter or leave a modal mode)
//! lives here and is covered by unit tests.
//!
//! Side effects are expressed as `Command` return values; the main
//! loop (src/main.rs) is responsible for executing them.

use super::command::Command;
use super::event::Event;
use super::keymap::{normal_action, Key, NormalAction};
use super::model::{Mode, Model};

/// Fold one event into the model. Returns `Some(Command)` if the
/// event triggers a side effect (attach / kill / create / quit).
pub fn update(model: &mut Model, event: Event) -> Option<Command> {
    // Any keystroke clears the transient error — the user has seen
    // it now. Async events (added in a later commit) will *set* an
    // error but won't clear one, so that messages about failed
    // background actions don't self-dismiss.
    let was_keystroke = matches!(event, Event::Key(_));
    if was_keystroke {
        model.error = None;
    }

    match event {
        Event::Key(k) => handle_key(model, k),
    }
}

fn handle_key(model: &mut Model, key: Key) -> Option<Command> {
    match &model.mode {
        Mode::Normal => handle_key_normal(model, key),
        Mode::CreateInput(_) => handle_key_create(model, key),
        Mode::ConfirmKill(_) => handle_key_confirm_kill(model, key),
        Mode::ConfirmForce(_) => handle_key_confirm_force(model, key),
    }
}

fn handle_key_normal(model: &mut Model, key: Key) -> Option<Command> {
    let action = normal_action(key)?;
    match action {
        NormalAction::SelectPrev => {
            model.select_prev();
            None
        }
        NormalAction::SelectNext => {
            model.select_next();
            None
        }
        NormalAction::AttachSelected => {
            let name = model.selected_name()?.to_string();
            // Emit Command::Attach with force=false. The executor
            // pre-flights fresh data and may pop a ConfirmForce
            // prompt if the session turns out to be attached
            // elsewhere — no reason to guess from (possibly stale)
            // model state here.
            Some(Command::Attach { name, force: false })
        }
        NormalAction::NewSession => {
            model.mode = Mode::CreateInput(String::new());
            None
        }
        NormalAction::KillSelected => {
            let name = model.selected_name()?.to_string();
            model.mode = Mode::ConfirmKill(name);
            None
        }
        NormalAction::Quit => Some(Command::Quit),
    }
}

fn handle_key_create(model: &mut Model, key: Key) -> Option<Command> {
    let Mode::CreateInput(buf) = &mut model.mode else {
        return None;
    };
    match key {
        // Esc and Ctrl-C both cancel — Ctrl-C is a reflexive "get me
        // out of here" in interactive tools, and mid-typing is a
        // common place to want to bail.
        Key::Esc | Key::Ctrl(0x03) => {
            model.mode = Mode::Normal;
            None
        }
        Key::Enter => {
            let name = std::mem::take(buf);
            model.mode = Mode::Normal;
            if name.is_empty() {
                None
            } else {
                Some(Command::Create(name))
            }
        }
        Key::Backspace => {
            buf.pop();
            None
        }
        // Printable non-space ASCII. shpool rejects whitespace in
        // names (it ends up in env vars / prompt prefixes where
        // spaces cause pain downstream). The decoder already
        // filtered non-ASCII bytes to Key::Other.
        Key::Char(b) if b != b' ' => {
            buf.push(b as char);
            None
        }
        _ => None,
    }
}

fn handle_key_confirm_kill(model: &mut Model, key: Key) -> Option<Command> {
    let Mode::ConfirmKill(name) = &mut model.mode else {
        return None;
    };
    match key {
        Key::Char(b'y') | Key::Char(b'Y') => {
            let name = std::mem::take(name);
            model.mode = Mode::Normal;
            Some(Command::Kill(name))
        }
        _ => {
            // Any non-y keystroke cancels. Intentional — matches
            // existing shpool-table behavior before the refactor;
            // stricter "only n/Enter/Esc cancel" would be a UX
            // change, not an architectural one.
            model.mode = Mode::Normal;
            None
        }
    }
}

fn handle_key_confirm_force(model: &mut Model, key: Key) -> Option<Command> {
    let Mode::ConfirmForce(name) = &mut model.mode else {
        return None;
    };
    match key {
        Key::Char(b'y') | Key::Char(b'Y') => {
            let name = std::mem::take(name);
            model.mode = Mode::Normal;
            Some(Command::Attach { name, force: true })
        }
        _ => {
            model.mode = Mode::Normal;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn mk(name: &str) -> Session {
        Session {
            name: name.to_string(),
            attached: false,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
        }
    }

    fn key(k: Key) -> Event {
        Event::Key(k)
    }

    #[test]
    fn down_then_enter_attaches_second_session() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        assert_eq!(update(&mut m, key(Key::Down)), None);
        assert_eq!(
            update(&mut m, key(Key::Enter)),
            Some(Command::Attach { name: "b".into(), force: false }),
        );
    }

    #[test]
    fn up_wraps_and_attaches_last() {
        let mut m = Model::new(vec![mk("x"), mk("y"), mk("z")]);
        assert_eq!(update(&mut m, key(Key::Up)), None);
        assert_eq!(
            update(&mut m, key(Key::Enter)),
            Some(Command::Attach { name: "z".into(), force: false }),
        );
    }

    #[test]
    fn q_quits() {
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(update(&mut m, key(Key::Char(b'q'))), Some(Command::Quit));
    }

    #[test]
    fn ctrl_c_quits_in_normal_mode() {
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(update(&mut m, key(Key::Ctrl(0x03))), Some(Command::Quit));
    }

    #[test]
    fn enter_on_empty_list_noops() {
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, key(Key::Enter)), None);
    }

    #[test]
    fn keystroke_clears_error() {
        let mut m = Model::new(vec![mk("a")]);
        m.set_error("session 'a' is gone");
        assert!(m.error.is_some());
        update(&mut m, key(Key::Char(b'j')));
        assert!(m.error.is_none());
    }

    #[test]
    fn unbound_keys_in_normal_mode_are_noop() {
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), None);
        assert_eq!(update(&mut m, key(Key::Char(b'y'))), None);
        assert_eq!(update(&mut m, key(Key::Char(b'z'))), None);
    }

    #[test]
    fn create_flow_enter_submits() {
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(update(&mut m, key(Key::Char(b'n'))), None);
        assert_eq!(m.mode, Mode::CreateInput(String::new()));
        update(&mut m, key(Key::Char(b'f')));
        update(&mut m, key(Key::Char(b'o')));
        update(&mut m, key(Key::Char(b'o')));
        assert_eq!(
            update(&mut m, key(Key::Enter)),
            Some(Command::Create("foo".into()))
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_typing_accumulates_into_buffer() {
        // Guards against a regression where keystrokes during
        // CreateInput would reset the buffer or escape the modal
        // early.
        let mut m = Model::new(vec![]);
        m.mode = Mode::CreateInput(String::new());
        update(&mut m, key(Key::Char(b'f')));
        update(&mut m, key(Key::Char(b'o')));
        update(&mut m, key(Key::Char(b'o')));
        assert_eq!(m.mode, Mode::CreateInput("foo".into()));
    }

    #[test]
    fn create_esc_cancels() {
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::CreateInput("partial".into());
        assert_eq!(update(&mut m, key(Key::Esc)), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_backspace_pops() {
        let mut m = Model::new(vec![]);
        m.mode = Mode::CreateInput("ab".into());
        update(&mut m, key(Key::Backspace));
        assert_eq!(m.mode, Mode::CreateInput("a".into()));
    }

    #[test]
    fn create_rejects_empty_on_enter() {
        // Empty name on Enter: return to Normal with no Command,
        // rather than emitting Command::Create("") which shpool
        // would reject.
        let mut m = Model::new(vec![]);
        m.mode = Mode::CreateInput(String::new());
        assert_eq!(update(&mut m, key(Key::Enter)), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_drops_spaces_in_name() {
        let mut m = Model::new(vec![]);
        m.mode = Mode::CreateInput(String::new());
        update(&mut m, key(Key::Char(b'a')));
        update(&mut m, key(Key::Char(b' ')));
        update(&mut m, key(Key::Char(b'b')));
        assert_eq!(m.mode, Mode::CreateInput("ab".into()));
    }

    #[test]
    fn confirm_kill_y_fires() {
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        assert_eq!(update(&mut m, key(Key::Char(b'd'))), None);
        assert_eq!(m.mode, Mode::ConfirmKill("a".into()));
        assert_eq!(
            update(&mut m, key(Key::Char(b'y'))),
            Some(Command::Kill("a".into()))
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn confirm_kill_any_non_y_cancels() {
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmKill("a".into());
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn kill_on_empty_list_is_noop() {
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, key(Key::Char(b'd'))), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn confirm_force_y_force_attaches() {
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmForce("a".into());
        assert_eq!(
            update(&mut m, key(Key::Char(b'y'))),
            Some(Command::Attach { name: "a".into(), force: true }),
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn confirm_force_n_cancels() {
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmForce("a".into());
        assert_eq!(update(&mut m, key(Key::Char(b'n'))), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn vim_jj_then_enter_attaches_third() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        update(&mut m, key(Key::Char(b'j')));
        update(&mut m, key(Key::Char(b'j')));
        assert_eq!(
            update(&mut m, key(Key::Enter)),
            Some(Command::Attach { name: "c".into(), force: false }),
        );
    }
}
