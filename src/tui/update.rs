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
use super::keymap::{Key, NormalAction, normal_action};
use super::model::{Mode, Model, Selection};

/// Fold one event into the model. Returns `Some(Command)` if the
/// event triggers a side effect (attach / kill / create / quit /
/// refresh).
pub fn update(model: &mut Model, event: Event) -> Option<Command> {
    // Any keystroke clears the transient error — the user has seen it
    // now. Async events SET errors but don't clear them, so messages
    // about failed background actions don't self-dismiss.
    //
    // Exception: while the selection is stale, the error rides through
    // this first keystroke. handle_key_normal consumes that keystroke
    // as the acknowledgment and clears both together — so the "is gone"
    // message is guaranteed at least one frame in front of the user.
    let was_keystroke = matches!(event, Event::Key(_));
    if was_keystroke && !model.is_stale() {
        model.error = None;
    }

    let cmd = match event {
        Event::Key(k) => handle_key(model, k),
        // Focus-gained is pure redundancy while subscribed — the event
        // stream already keeps the list current.
        Event::FocusGained => refresh_unless_subscribed(model),
        // An event from the subscription always means "re-list".
        Event::EventsArrived => Some(Command::Refresh),
        Event::SessionsRefreshed(sessions) => {
            model.refresh(sessions);
            cancel_modal_if_target_gone(model);
            Option::None
        }
        Event::RefreshFailed(msg) => {
            model.set_error(format!("shpool list: {msg}"));
            Option::None
        }
        Event::AttachExited { ok, name } => {
            // A completed action supersedes any stale-selection state
            // left over from before it (e.g. a session vanished while
            // the user sat inside the attach). Clear it ahead of the
            // refresh below, which re-raises stale only if something
            // fresh disappeared mid-action.
            clear_pre_action_alert(model);
            if !ok {
                model.set_error(format!("shpool attach {name} failed"));
            }
            // Reselect the session we just attached to, so when the
            // user returns they're looking at what they just left.
            if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
                model.selection = Selection::At(i);
            }
            // Always refresh after attach: state may have changed
            // while we were suspended (other clients detached,
            // sessions expired, etc.).
            Some(Command::Refresh)
        }
        Event::KillFinished { ok, name, err } => {
            clear_pre_action_alert(model);
            if !ok {
                let msg = err.unwrap_or_else(|| format!("kill {name} failed"));
                model.set_error(msg);
            }
            Some(Command::Refresh)
        }
    };

    // Auto-refresh: a Normal-mode keystroke that produced no Command of
    // its own requests a refresh so the list tracks daemon-side changes
    // (sessions created/killed/detached by other clients) without
    // explicit user action. Skipped in modal modes so typing "foo" into
    // CreateInput isn't three shell-outs, and skipped while subscribed —
    // the event stream already keeps us current, so this fallback would
    // only double the list calls.
    if was_keystroke && cmd.is_none() && matches!(model.mode, Mode::Normal) && !model.events_active
    {
        return Some(Command::Refresh);
    }
    cmd
}

/// Refresh unless an events subscription is already keeping the list
/// current. The focus-gained path is pure redundancy while subscribed.
fn refresh_unless_subscribed(model: &Model) -> Option<Command> {
    if model.events_active {
        Option::None
    } else {
        Some(Command::Refresh)
    }
}

/// Clear the transient error and demote a stale selection back to a
/// clean slate, ahead of a completed action's own refresh. A valid
/// `At` selection is left untouched; only the stale/cleared alert goes.
fn clear_pre_action_alert(model: &mut Model) {
    if model.is_stale() {
        model.selection = Selection::None;
    }
    model.error = None;
}

/// Drop a kill / force-attach modal whose target session has vanished
/// from under it — any refresh (event-driven, focus, the keystroke
/// fallback) can race ahead of the user mid-prompt. CreateInput is
/// safe: its buffer is a name being typed, not a session reference.
fn cancel_modal_if_target_gone(model: &mut Model) {
    let target = match &model.mode {
        Mode::ConfirmKill(name) | Mode::ConfirmForce(name) => name.clone(),
        _ => return,
    };
    if !model.sessions.iter().any(|s| s.name == target) {
        model.mode = Mode::Normal;
        model.set_error(format!("session '{target}' is gone"));
    }
}

fn handle_key(model: &mut Model, key: Key) -> Option<Command> {
    match &model.mode {
        Mode::Normal => handle_key_normal(model, key),
        Mode::CreateInput(_) => handle_key_create(model, key),
        Mode::ConfirmKill(_) | Mode::ConfirmForce(_) => handle_key_confirm(model, key),
    }
}

fn handle_key_normal(model: &mut Model, key: Key) -> Option<Command> {
    let action = normal_action(key);

    // Acknowledge a stale selection on the first keystroke — whatever
    // it is. Clear the flag and its error; for the act-on-selection
    // keys, also swallow the keystroke so the action can't strike
    // whatever shifted into the vanished session's row. Navigation (and
    // everything else) falls through and re-seats naturally.
    if model.is_stale() {
        model.selection = Selection::None;
        model.error = None;
        if matches!(
            action,
            Some(NormalAction::AttachSelected | NormalAction::KillSelected)
        ) {
            return Option::None;
        }
    }

    match action? {
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
        NormalAction::EnsureDaemon => Some(Command::EnsureDaemon),
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

/// Handle a key in either confirm modal (kill or force-attach). y/Y
/// confirms; n/N/Esc/Ctrl-C cancels; every other key — arrows, stray
/// letters, a fat-fingered space — is ignored so the prompt stays put.
/// The old "any non-y key cancels" made it far too easy to dismiss a
/// prompt by accident, which matters more now that background refreshes
/// keep the modal up while state churns underneath it. The two modes
/// differ only in the action confirmed, so they share this handler.
fn handle_key_confirm(model: &mut Model, key: Key) -> Option<Command> {
    match key {
        Key::Char(b'y') | Key::Char(b'Y') => {
            match std::mem::replace(&mut model.mode, Mode::Normal) {
                Mode::ConfirmKill(name) => {
                    // Step the cursor onto a neighbor first so the
                    // post-kill refresh reads the user's own kill as an
                    // expected change, not a stale disappearance.
                    model.advance_off(&name);
                    Some(Command::Kill(name))
                }
                Mode::ConfirmForce(name) => Some(Command::Attach { name, force: true }),
                // handle_key only routes the two confirm modes here.
                _ => Option::None,
            }
        }
        Key::Char(b'n') | Key::Char(b'N') | Key::Esc | Key::Ctrl(0x03) => {
            model.mode = Mode::Normal;
            Option::None
        }
        // Any other key: stay in the modal rather than dismissing it.
        _ => Option::None,
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
            Some(Command::Attach {
                name: "b".into(),
                force: false
            }),
        );
    }

    #[test]
    fn up_wraps_and_attaches_last() {
        let mut m = Model::new(vec![mk("x"), mk("y"), mk("z")]);
        assert_eq!(update(&mut m, key(Key::Up)), Some(Command::Refresh));
        assert_eq!(
            update(&mut m, key(Key::Enter)),
            Some(Command::Attach {
                name: "z".into(),
                force: false
            }),
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
    fn confirm_kill_stray_key_is_ignored() {
        // Strict modals: a stray key (arrow, space, random letter) no
        // longer dismisses the prompt — it stays put with no command.
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmKill("a".into());
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), None);
        assert_eq!(m.mode, Mode::ConfirmKill("a".into()));
        assert_eq!(update(&mut m, key(Key::Down)), None);
        assert_eq!(m.mode, Mode::ConfirmKill("a".into()));
    }

    #[test]
    fn confirm_kill_n_and_esc_cancel() {
        for cancel in [Key::Char(b'n'), Key::Char(b'N'), Key::Esc, Key::Ctrl(0x03)] {
            let mut m = Model::new(vec![mk("a")]);
            m.mode = Mode::ConfirmKill("a".into());
            // Cancel returns to Normal, which then auto-refreshes.
            assert_eq!(update(&mut m, key(cancel)), Some(Command::Refresh));
            assert_eq!(m.mode, Mode::Normal);
        }
    }

    #[test]
    fn confirm_kill_y_advances_cursor_off_target() {
        // 'y' steps the cursor onto a neighbor before issuing the kill,
        // so the post-kill refresh won't read it as a stale loss.
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        m.selection = Selection::At(0); // "a"
        m.mode = Mode::ConfirmKill("a".into());
        assert_eq!(
            update(&mut m, key(Key::Char(b'y'))),
            Some(Command::Kill("a".into()))
        );
        assert_eq!(m.selected_name(), Some("b"));
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
            Some(Command::Attach {
                name: "a".into(),
                force: true
            }),
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
        m.selection = Selection::At(0);
        let cmd = update(
            &mut m,
            Event::AttachExited {
                ok: true,
                name: "c".into(),
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert_eq!(m.selected_index(), Some(2));
        assert!(m.error.is_none());
    }

    #[test]
    fn attach_exited_fail_sets_error_and_refreshes() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(
            &mut m,
            Event::AttachExited {
                ok: false,
                name: "a".into(),
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert!(m.error.as_deref().unwrap_or("").contains("shpool attach"));
    }

    #[test]
    fn kill_finished_ok_refreshes() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(
            &mut m,
            Event::KillFinished {
                ok: true,
                name: "a".into(),
                err: None,
            },
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
    fn uppercase_d_fires_ensure_daemon() {
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(
            update(&mut m, key(Key::Char(b'D'))),
            Some(Command::EnsureDaemon),
        );
    }

    #[test]
    fn lowercase_d_still_enters_confirm_kill() {
        // Guard against a regression where the case-split between
        // d (kill) and D (daemon) gets re-fused.
        let mut m = Model::new(vec![mk("a")]);
        update(&mut m, key(Key::Char(b'd')));
        assert_eq!(m.mode, Mode::ConfirmKill("a".into()));
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
            Some(Command::Attach {
                name: "c".into(),
                force: false
            }),
        );
    }

    // -- events subscription gating --

    #[test]
    fn events_arrived_refreshes() {
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, Event::EventsArrived), Some(Command::Refresh));
    }

    #[test]
    fn subscribed_keystroke_skips_auto_refresh() {
        // While subscribed the event stream keeps the list current, so a
        // no-op Normal keystroke must NOT also fire a list call.
        let mut m = Model::new(vec![mk("a")]);
        m.events_active = true;
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), None);
    }

    #[test]
    fn subscribed_focus_gained_skips_refresh() {
        let mut m = Model::new(vec![]);
        m.events_active = true;
        assert_eq!(update(&mut m, Event::FocusGained), None);
    }

    #[test]
    fn unsubscribed_focus_gained_refreshes() {
        let mut m = Model::new(vec![]);
        assert_eq!(update(&mut m, Event::FocusGained), Some(Command::Refresh));
    }

    // -- stale-selection acknowledgment --

    #[test]
    fn stale_selection_consumes_enter_without_attaching() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::At(1); // "b"
        // A racy refresh drops "b": selection goes stale, error set.
        update(&mut m, Event::SessionsRefreshed(vec![mk("a"), mk("c")]));
        assert!(m.is_stale());
        assert!(m.error.is_some());
        // First Enter is the acknowledgment: no Attach, error cleared,
        // selection no longer stale.
        let cmd = update(&mut m, key(Key::Enter));
        assert!(!matches!(cmd, Some(Command::Attach { .. })));
        assert!(!m.is_stale());
        assert!(m.error.is_none());
    }

    #[test]
    fn stale_selection_consumes_kill_keystroke() {
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        m.selection = Selection::At(1);
        update(&mut m, Event::SessionsRefreshed(vec![mk("a")]));
        assert!(m.is_stale());
        // d is consumed as ack — no ConfirmKill, no command that could
        // act on the wrong row.
        let cmd = update(&mut m, key(Key::Char(b'd')));
        assert!(!matches!(cmd, Some(Command::Kill(_))));
        assert_eq!(m.mode, Mode::Normal);
        assert!(!m.is_stale());
    }

    #[test]
    fn stale_selection_navigation_clears_and_moves() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        m.selection = Selection::At(1);
        update(&mut m, Event::SessionsRefreshed(vec![mk("a"), mk("c")]));
        assert!(m.is_stale());
        // j acknowledges AND re-seats onto a real row.
        update(&mut m, key(Key::Char(b'j')));
        assert!(!m.is_stale());
        assert!(m.selected_name().is_some());
    }

    // -- modal cancelled when its target vanishes --

    #[test]
    fn confirm_kill_cancelled_when_target_vanishes() {
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        m.selection = Selection::At(0);
        m.mode = Mode::ConfirmKill("a".into());
        // A background refresh removes "a" out from under the prompt.
        update(&mut m, Event::SessionsRefreshed(vec![mk("b")]));
        assert_eq!(m.mode, Mode::Normal);
        assert!(m.error.as_deref().unwrap_or("").contains("'a' is gone"));
    }

    #[test]
    fn confirm_force_cancelled_when_target_vanishes() {
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        m.mode = Mode::ConfirmForce("a".into());
        update(&mut m, Event::SessionsRefreshed(vec![mk("b")]));
        assert_eq!(m.mode, Mode::Normal);
        assert!(m.error.as_deref().unwrap_or("").contains("'a' is gone"));
    }

    #[test]
    fn create_modal_survives_session_churn() {
        // CreateInput holds a name being typed, not a session ref, so a
        // refresh that changes the list must not cancel it.
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::CreateInput("foo".into());
        update(&mut m, Event::SessionsRefreshed(vec![mk("b")]));
        assert_eq!(m.mode, Mode::CreateInput("foo".into()));
    }

    #[test]
    fn attach_return_clears_prior_stale() {
        // The user was stale (a session vanished), then completed an
        // attach. Returning clears the now-irrelevant stale alert.
        let mut m = Model::new(vec![mk("a")]);
        m.selection = Selection::Stale("old".into());
        m.set_error("session 'old' is gone");
        let cmd = update(
            &mut m,
            Event::AttachExited {
                ok: true,
                name: "a".into(),
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert!(!m.is_stale());
        assert!(m.error.is_none());
    }
}
