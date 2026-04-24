//! The pure update function: `(&mut Model, Event) -> Option<Command>`.
//!
//! Free of I/O — no stdin reads, no shell-outs, no ANSI writes. All
//! the dispatch policy (which key does what in which mode, when to
//! clear the transient error, when to enter or leave a modal mode,
//! when to refresh) lives here and is covered by unit tests.
//!
//! Side effects are expressed as `Command` return values; the main
//! loop (src/main.rs) is responsible for executing them.

use super::command::Command;
use super::event::Event;
use super::keymap::{normal_action, Key, NormalAction};
use super::model::{Mode, Model};

/// Fold one event into the model. Returns `Some(Command)` if the
/// event triggers a side effect (attach / kill / create / quit /
/// refresh).
pub fn update(model: &mut Model, event: Event) -> Option<Command> {
    // Any keystroke clears the transient error — the user has seen
    // it now. Async events SET errors but don't clear them, so that
    // messages about failed background actions don't self-dismiss.
    let was_keystroke = matches!(event, Event::Key(_));
    if was_keystroke {
        model.error = None;
    }

    let cmd = match event {
        Event::Key(k) => handle_key(model, k),
        Event::FocusGained => Some(Command::Refresh),
        Event::SessionsRefreshed(sessions) => {
            model.refresh(sessions);
            None
        }
        Event::RefreshFailed(msg) => {
            model.set_error(format!("shpool list: {msg}"));
            None
        }
        Event::AttachExited { ok, name } => {
            if !ok {
                model.set_error(format!("shpool attach {name} failed"));
            }
            // Reselect the session we just attached to, so when the
            // user returns they're looking at what they just left.
            if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
                model.selected = i;
            }
            // Always refresh after attach: state may have changed
            // while we were suspended (other clients detached,
            // sessions expired, etc.).
            Some(Command::Refresh)
        }
        Event::KillFinished { ok, name, err } => {
            if !ok {
                let msg = err.unwrap_or_else(|| format!("kill {name} failed"));
                model.set_error(msg);
            }
            Some(Command::Refresh)
        }
    };

    // Auto-refresh: a Normal-mode keystroke that produced no Command
    // of its own requests a refresh so the list tracks daemon-side
    // changes (sessions created/killed/detached by other clients)
    // without needing explicit user action. Skipped in modal modes
    // so typing "foo" into CreateInput isn't three shell-outs.
    if was_keystroke && cmd.is_none() && matches!(model.mode, Mode::Normal) {
        return Some(Command::Refresh);
    }
    cmd
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
                return None;
            }
            // Reject duplicates here rather than making the executor
            // do a refresh-then-check dance. The model may be a touch
            // stale (we skip auto-refresh while CreateInput is open),
            // but in the rare "daemon created a same-named session
            // while I was typing" race, the user sees `already exists`
            // on one attempt and a fresh list on the next.
            if model.sessions.iter().any(|s| s.name == name) {
                model.set_error(format!("session '{name}' already exists"));
                return None;
            }
            Some(Command::Create(name))
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
        // Down is a Normal-mode keystroke with no session-binding
        // command, so it triggers auto-refresh — the list stays
        // current without the user having to do anything explicit.
        assert_eq!(update(&mut m, key(Key::Down)), Some(Command::Refresh));
        assert_eq!(
            update(&mut m, key(Key::Enter)),
            Some(Command::Attach { name: "b".into(), force: false }),
        );
    }

    #[test]
    fn up_wraps_and_attaches_last() {
        let mut m = Model::new(vec![mk("x"), mk("y"), mk("z")]);
        assert_eq!(update(&mut m, key(Key::Up)), Some(Command::Refresh));
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
    fn enter_on_empty_list_triggers_auto_refresh_only() {
        // AttachSelected short-circuits when the list is empty, so
        // handle_key_normal produces no Command. The Normal-mode
        // auto-refresh still fires.
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, key(Key::Enter)), Some(Command::Refresh));
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
    fn unbound_keys_in_normal_mode_trigger_auto_refresh() {
        // No action binding for x/y/z in Normal mode, but each is
        // still a keystroke — the auto-refresh rule treats them
        // like any other Normal-mode keypress.
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), Some(Command::Refresh));
        assert_eq!(update(&mut m, key(Key::Char(b'y'))), Some(Command::Refresh));
        assert_eq!(update(&mut m, key(Key::Char(b'z'))), Some(Command::Refresh));
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
        // Esc transitions out of CreateInput. Auto-refresh then
        // fires since we've landed back in Normal mode.
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::CreateInput("partial".into());
        assert_eq!(update(&mut m, key(Key::Esc)), Some(Command::Refresh));
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
        // Empty name on Enter: return to Normal without emitting
        // Command::Create. Auto-refresh fires because we're back
        // in Normal mode.
        let mut m = Model::new(vec![]);
        m.mode = Mode::CreateInput(String::new());
        assert_eq!(update(&mut m, key(Key::Enter)), Some(Command::Refresh));
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_rejects_duplicate_name() {
        let mut m = Model::new(vec![mk("main")]);
        m.mode = Mode::CreateInput("main".into());
        // Duplicate check runs in update before emitting Create, so
        // the executor never sees the doomed command. Auto-refresh
        // fires after the mode transitions to Normal.
        assert_eq!(update(&mut m, key(Key::Enter)), Some(Command::Refresh));
        assert!(m.error.as_deref().unwrap_or("").contains("already exists"));
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
        // Non-y cancels back to Normal, which then auto-refreshes.
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmKill("a".into());
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), Some(Command::Refresh));
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn kill_on_empty_list_is_noop() {
        // 'd' with no sessions: handle_key_normal short-circuits (no
        // mode change, no command). Auto-refresh still fires because
        // we're in Normal mode.
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, key(Key::Char(b'd'))), Some(Command::Refresh));
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
        assert_eq!(update(&mut m, key(Key::Char(b'n'))), Some(Command::Refresh));
        assert_eq!(m.mode, Mode::Normal);
    }

    // -- async event tests --

    #[test]
    fn sessions_refreshed_applies_and_does_not_re_refresh() {
        // Guards against an infinite-refresh loop: the event that
        // comes back from the executor after Command::Refresh must
        // NOT itself emit another Command::Refresh. The
        // `was_keystroke` gate in update() is what prevents this.
        let mut m = Model::new(vec![]);
        let cmd = update(&mut m, Event::SessionsRefreshed(vec![mk("a"), mk("b")]));
        assert_eq!(cmd, None);
        assert_eq!(m.sessions.len(), 2);
    }

    #[test]
    fn refresh_failed_sets_error_no_re_refresh() {
        // Same invariant for the failure path — otherwise a down
        // daemon would tight-loop refresh-failures.
        let mut m = Model::new(vec![]);
        let cmd = update(&mut m, Event::RefreshFailed("boom".into()));
        assert_eq!(cmd, None);
        assert!(m.error.as_deref().unwrap_or("").contains("shpool list"));
    }

    #[test]
    fn attach_exited_ok_reselects_and_refreshes() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selected = 0;
        let cmd = update(
            &mut m,
            Event::AttachExited { ok: true, name: "c".into() },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert_eq!(m.selected, 2);
        assert!(m.error.is_none());
    }

    #[test]
    fn attach_exited_fail_sets_error_and_refreshes() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(
            &mut m,
            Event::AttachExited { ok: false, name: "a".into() },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert!(m.error.as_deref().unwrap_or("").contains("shpool attach"));
    }

    #[test]
    fn kill_finished_ok_refreshes() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(
            &mut m,
            Event::KillFinished { ok: true, name: "a".into(), err: None },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert!(m.error.is_none());
    }

    #[test]
    fn kill_finished_fail_sets_error_and_refreshes() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(
            &mut m,
            Event::KillFinished {
                ok: false,
                name: "a".into(),
                err: Some("kill a: not found".into()),
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert_eq!(m.error.as_deref(), Some("kill a: not found"));
    }

    #[test]
    fn typing_in_create_mode_does_not_auto_refresh() {
        // In modal modes we skip auto-refresh — typing "foo" into
        // the create prompt shouldn't fire three shell-outs.
        let mut m = Model::new(vec![]);
        m.mode = Mode::CreateInput(String::new());
        assert_eq!(update(&mut m, key(Key::Char(b'f'))), None);
    }

    #[test]
    fn async_events_do_not_clear_transient_error() {
        // Only keystrokes clear the error — a background event like
        // a refresh completion shouldn't dismiss a message the user
        // hasn't acknowledged.
        let mut m = Model::new(vec![]);
        m.set_error("sticky");
        update(&mut m, Event::SessionsRefreshed(vec![]));
        assert_eq!(m.error.as_deref(), Some("sticky"));
    }

    #[test]
    fn focus_gained_triggers_refresh() {
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, Event::FocusGained), Some(Command::Refresh));
    }

    #[test]
    fn focus_gained_does_not_clear_error() {
        let mut m = Model::new(vec![]);
        m.set_error("sticky");
        update(&mut m, Event::FocusGained);
        assert_eq!(m.error.as_deref(), Some("sticky"));
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
