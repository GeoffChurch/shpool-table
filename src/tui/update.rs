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
use super::model::{
    EditState, Mode, Model, Selection, VarPromptState, VarsState, merge_unset_vars,
    remerge_preserving_cursor, resolution_map,
};
use super::template::{candidate_values, filter_rank};

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
            // In the vars view the displayed list depends on the sessions
            // just refreshed (the unset rows derive from their templates),
            // so re-merge against the new list, keeping the cursor on its
            // variable by name. This also self-corrects the post-`var set`
            // re-dial: the Refresh that VarSetFinished issues lands here
            // and re-merges against fresh sessions. Disjoint borrow:
            // &model.sessions vs &mut model.mode.
            let sessions = &model.sessions;
            if let Mode::Vars(vs) = &mut model.mode {
                remerge_preserving_cursor(vs, sessions);
            }
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
        Event::CreateNeedsVars {
            name,
            vars,
            set_vars,
        } => {
            // The Create detect step found unknown vars (no teardown ran;
            // we're still in the alt-screen). Open the per-var prompt,
            // carrying the set-var snapshot so the live preview can resolve
            // known co-vars. No Command — the prompt drives the next step.
            model.mode = Mode::CreateVarPrompt(VarPromptState {
                name,
                vars,
                idx: 0,
                input: String::new(),
                collected: Vec::new(),
                set_vars,
            });
            Option::None
        }
        Event::CreateVarsFailed { var, err } => {
            // A `var set` in the apply step failed; the create was aborted
            // with no attach. The prompt already dropped to Normal at
            // apply-emit (B.4), so this parked error is visible. Partial
            // sets linger (no rollback) — the next detect sees them as set.
            // Message mirrors the vars view's `var set <var>: <stderr>`.
            let msg = match err {
                Some(e) => format!("var set {var}: {e}"),
                Option::None => format!("var set {var} failed"),
            };
            model.set_error(msg);
            Option::None
        }
        Event::VarsFetched(vars) => {
            // Open the vars view on the fresh snapshot, cursor at the top.
            // Surface the unset rows the current sessions reference; the
            // cursor resets to 0 so no by-name preservation is needed here.
            let vars = merge_unset_vars(&vars, &model.sessions);
            model.mode = Mode::Vars(VarsState {
                vars,
                selected: 0,
                edit: Option::None,
            });
            Option::None
        }
        Event::VarsFetchFailed(e) => {
            // List failed: stay in Normal mode with the error parked,
            // rather than opening an empty view.
            model.set_error(format!("shpool var list: {e}"));
            Option::None
        }
        Event::VarSetFinished {
            name,
            ok,
            err,
            vars,
        } => {
            // Clear the edit line regardless of outcome; on success swap
            // in the refetched (set-only) list, point the cursor at the
            // var we just set by name, then re-merge the unset rows the
            // current sessions reference (carrying the cursor across the
            // resort). The set may have promoted an unset row to a set one
            // or added a brand-new var. The Refresh below also re-merges
            // against fresh sessions in the SessionsRefreshed arm; doing it
            // here too keeps the pre-refresh frame consistent. Disjoint
            // borrow: &model.sessions vs &mut model.mode.
            let sessions = &model.sessions;
            if let Mode::Vars(vs) = &mut model.mode {
                vs.edit = Option::None;
                if ok {
                    if let Some(v) = vars {
                        vs.selected = v.iter().position(|x| x.name == name).unwrap_or(0);
                        vs.vars = v;
                        remerge_preserving_cursor(vs, sessions);
                    }
                }
            }
            if ok {
                // Refresh sessions so the preview reflects any re-dial
                // the set triggered. Mirrors shperl's var_set branch.
                Some(Command::Refresh)
            } else {
                let msg = match err {
                    Some(e) => format!("var set {name}: {e}"),
                    Option::None => format!("var set {name} failed"),
                };
                model.set_error(msg);
                Option::None
            }
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
/// fallback) can race ahead of the user mid-prompt. CreateInput and
/// CreateVarPrompt are safe: their state is a name/value being typed,
/// not a reference to an existing session.
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
        Mode::CreateVarPrompt(_) => handle_key_create_vars(model, key),
        Mode::ConfirmKill(_) | Mode::ConfirmForce(_) => handle_key_confirm(model, key),
        Mode::Vars(_) => handle_key_vars(model, key),
    }
}

fn handle_key_normal(model: &mut Model, key: Key) -> Option<Command> {
    let action = normal_action(key);

    // Acknowledge a stale selection on the first keystroke — whatever it
    // is. Clear the error; for the act-on-selection keys, also swallow
    // the keystroke so the action can't strike whatever shifted into the
    // vanished session's row.
    if model.is_stale() {
        model.error = None;
        let swallow = matches!(
            action,
            Some(NormalAction::AttachSelected | NormalAction::KillSelected)
        );
        // Re-seat the cursor onto the freshest row for keys that would
        // otherwise strand it on nothing: a swallowed act-on-selection
        // key, or any unbound key. Navigation re-seats itself
        // (None -> first/last) and the mode-changing keys resolve their
        // own state, so those just fall through with the selection
        // cleared. This keeps the subscribed path consistent with the
        // fallback one, where the post-ack auto-refresh re-seats anyway.
        model.selection = if (swallow || action.is_none()) && !model.sessions.is_empty() {
            Selection::At(0)
        } else {
            Selection::None
        };
        if swallow {
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
        NormalAction::Variables => Some(Command::FetchVars),
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

/// Handle a key in the create-time variable prompt. Walks the unknown
/// vars one at a time: printable bytes (incl. space — values may hold
/// spaces, unlike session names) / Backspace edit the current `input`;
/// Enter pushes `(vars[idx], input)` to `collected` when non-empty (an
/// empty entry skips that var — it stays unset, surfaced later by the
/// vars view) and advances `idx`. Once every var has been visited, drop
/// to Normal (so a failed apply's error isn't hidden behind this
/// bottom-bar label — the modal-over-error rule) and emit
/// Command::CreateWithVars with the collected pairs. Esc/Ctrl-C cancel
/// the whole prompt (nothing set, no session created). Other keys
/// (arrows, Tab, unmapped CSI -> Key::Other) are consumed.
fn handle_key_create_vars(model: &mut Model, key: Key) -> Option<Command> {
    let Mode::CreateVarPrompt(vp) = &mut model.mode else {
        return Option::None;
    };
    match key {
        Key::Esc | Key::Ctrl(0x03) => {
            model.mode = Mode::Normal;
            Option::None
        }
        Key::Enter => {
            if !vp.input.is_empty() {
                let var = vp.vars[vp.idx].clone();
                let value = std::mem::take(&mut vp.input);
                vp.collected.push((var, value));
            }
            vp.idx += 1;
            if vp.idx == vp.vars.len() {
                // Drop to Normal before emitting so an apply failure's
                // parked error isn't outranked by this prompt's label.
                let name = vp.name.clone();
                let set_vars = std::mem::take(&mut vp.collected);
                model.mode = Mode::Normal;
                Some(Command::CreateWithVars { name, set_vars })
            } else {
                vp.input.clear();
                Option::None
            }
        }
        Key::Backspace => {
            vp.input.pop();
            Option::None
        }
        // Printable ASCII, space included (the decoder already filtered
        // non-ASCII bytes to Key::Other).
        Key::Char(b) => {
            vp.input.push(b as char);
            Option::None
        }
        // Other keys (arrows, Tab, Key::Other): consumed, input untouched.
        _ => Option::None,
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

/// Handle a key in the template-variable view. Two sub-states keyed off
/// `edit`:
///   browsing (`edit == None`) — j/k/arrows move the cursor (wrapping);
///     e/Enter open the value selector (field empty, current value just
///     the highlighted row); Esc/q/Ctrl-C return to the session list.
///     `Q` and everything else are ignored (matching shperl — `Q` is not
///     a leave key here).
///   editing (`edit == Some(_)`) — the value selector. Up/Down move the
///     highlight within the candidate list (copying the highlighted value
///     into `field`, leaving `filter` frozen so the list stays put);
///     printable bytes (incl. space) / Backspace edit `field`, refresh
///     `filter` from it, and reset the highlight to the top; Enter commits
///     a Command::SetVar — applying `field`, or the highlighted candidate
///     when `field` is empty; Esc/Ctrl-C cancel the edit. Other
///     non-printables are ignored.
fn handle_key_vars(model: &mut Model, key: Key) -> Option<Command> {
    // Split the borrow: the harvest path reads `sessions` while mutating
    // `mode`, and the two fields are disjoint.
    let sessions = &model.sessions;
    let Mode::Vars(vs) = &mut model.mode else {
        return Option::None;
    };

    // Editing sub-state (the value selector).
    if let Some(edit) = &mut vs.edit {
        match key {
            Key::Esc | Key::Ctrl(0x03) => {
                vs.edit = Option::None;
                Option::None
            }
            // Up/Down move the highlight within the shown list and copy
            // the highlighted candidate into the field; the filter is left
            // frozen so the list stays stable while arrowing.
            Key::Up | Key::Down => {
                let shown = filter_rank(&edit.candidates, &edit.filter);
                if !shown.is_empty() {
                    let last = shown.len() - 1;
                    edit.highlight = match key {
                        Key::Down => (edit.highlight + 1).min(last),
                        // Up: clamp at 0, saturating so it never wraps.
                        _ => edit.highlight.saturating_sub(1),
                    };
                    edit.field = shown[edit.highlight].clone();
                }
                Option::None
            }
            Key::Enter => {
                // Empty field => apply the highlighted row (= the current
                // value, since an empty filter shows it first); otherwise
                // apply the literal field (no dead zone — `xm` is applied
                // as `xm` even while `xmr` is shown). The edit sub-state is
                // cleared when VarSetFinished lands.
                let value = if edit.field.is_empty() {
                    let shown = filter_rank(&edit.candidates, &edit.filter);
                    shown.get(edit.highlight).cloned().unwrap_or_default()
                } else {
                    std::mem::take(&mut edit.field)
                };
                vs.vars.get(vs.selected).map(|v| Command::SetVar {
                    name: v.name.clone(),
                    value,
                })
            }
            Key::Backspace => {
                edit.field.pop();
                edit.filter = edit.field.clone();
                edit.highlight = 0;
                Option::None
            }
            // Values may hold spaces (nothing rejects them here), so space
            // is accepted unlike in the create-name prompt. A typing
            // keystroke snaps the highlight back to the top.
            Key::Char(b) => {
                edit.field.push(b as char);
                edit.filter = edit.field.clone();
                edit.highlight = 0;
                Option::None
            }
            // Other non-printables (Tab, unmapped CSI -> Key::Other): the
            // sequence is consumed, the field untouched.
            _ => Option::None,
        }
    } else {
        // Browsing sub-state. Leave keys transition out — compute first,
        // then reassign model.mode (can't hold &mut into vs across it).
        match key {
            Key::Down | Key::Char(b'j') | Key::Char(b'J') => {
                vars_select(vs, 1);
                Option::None
            }
            Key::Up | Key::Char(b'k') | Key::Char(b'K') => {
                vars_select(vs, -1);
                Option::None
            }
            Key::Enter | Key::Char(b'e') => {
                // Open the value selector: harvest candidates from existing
                // session names, start with an empty field/filter (the
                // current value is just the highlighted row, not prefilled
                // text), highlight the first row. No-op on an empty list.
                if vs.vars.get(vs.selected).is_some() {
                    // Map from real (!unset) rows only, so a synthetic unset
                    // co-var can't collapse a literal {name} to empty in the
                    // harvest. An unset target itself is fine: its current
                    // value reads as "" with no panic.
                    let vmap = resolution_map(&vs.vars);
                    let name = vs.vars[vs.selected].name.as_str();
                    let candidates = candidate_values(sessions, &vmap, name);
                    vs.edit = Some(EditState {
                        field: String::new(),
                        filter: String::new(),
                        candidates,
                        highlight: 0,
                    });
                }
                Option::None
            }
            Key::Esc | Key::Char(b'q') | Key::Ctrl(0x03) => {
                model.mode = Mode::Normal;
                Option::None
            }
            // Everything else (incl. `Q`): ignored — stay in the view.
            _ => Option::None,
        }
    }
}

/// Move the vars cursor by `dir`, wrapping at both ends. No-op on an
/// empty list.
fn vars_select(vs: &mut VarsState, dir: isize) {
    let n = vs.vars.len();
    if n == 0 {
        return;
    }
    let next = (vs.selected as isize + dir).rem_euclid(n as isize) as usize;
    vs.selected = next;
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
            attachments: Vec::new(),
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

    #[test]
    fn stale_ack_reseats_onto_a_row_even_when_subscribed() {
        // Subscribed: there's no fallback auto-refresh to re-seat the
        // cursor, so acking must land it on a real row itself rather than
        // strand it on None. Covers an unbound key and a swallowed
        // act-on-selection key — the two paths that don't navigate.
        for ack in [Key::Char(b'x'), Key::Enter] {
            let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
            m.events_active = true;
            m.selection = Selection::At(1); // "b"
            update(&mut m, Event::SessionsRefreshed(vec![mk("a"), mk("c")]));
            assert!(m.is_stale());
            update(&mut m, key(ack));
            assert!(!m.is_stale(), "ack with {ack:?} should clear stale");
            assert!(
                m.selected_index().is_some(),
                "ack with {ack:?} should re-seat onto a row, not strand at None",
            );
        }
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

    // -- vars view state machine --

    use super::super::model::Var;

    fn var(name: &str, value: &str) -> Var {
        Var {
            name: name.to_string(),
            value: value.to_string(),
            unset: false,
        }
    }

    /// Sessions dialed in with `{workspace}-...` / `{editor}-notes`
    /// templates so `workspace` governs two attachments and `editor`
    /// harvests `vim`. Mirrors shperl's vars_sessions fixture.
    fn vars_sessions() -> Vec<Session> {
        vec![
            attached_sess("myproj-edit", "{workspace}-edit", 111),
            attached_sess("myproj-term", "{workspace}-term", 222),
            attached_sess("vim-notes", "{editor}-notes", 333),
        ]
    }

    fn attached_sess(name: &str, template: &str, pid: u64) -> Session {
        use crate::session::Attachment;
        Session {
            name: name.to_string(),
            attached: true,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
            attachments: vec![Attachment {
                session_name_template: template.to_string(),
                pid,
            }],
        }
    }

    /// A model parked in the vars view with two variables, cursor at 0,
    /// over the templated sessions. Mirrors shperl's make_vars_model.
    fn vars_model() -> Model {
        let mut m = Model::new(vars_sessions());
        m.mode = Mode::Vars(VarsState {
            vars: vec![var("editor", "vim"), var("workspace", "myproj")],
            selected: 0,
            edit: Option::None,
        });
        m
    }

    /// Destructure the vars state, panicking if the model isn't in the
    /// vars view — keeps the tests asserting on fields, not on whole-Vec
    /// equality.
    fn vars(m: &Model) -> &VarsState {
        let Mode::Vars(vs) = &m.mode else {
            panic!("expected Mode::Vars, got {:?}", m.mode);
        };
        vs
    }

    /// The value-selector edit state, panicking if not editing.
    fn edit(m: &Model) -> &EditState {
        vars(m)
            .edit
            .as_ref()
            .unwrap_or_else(|| panic!("expected an active edit"))
    }

    #[test]
    fn v_in_normal_mode_fetches_vars() {
        let mut m = Model::new(vec![mk("a")]);
        assert_eq!(
            update(&mut m, key(Key::Char(b'v'))),
            Some(Command::FetchVars)
        );
        // Still Normal until the fetched event arrives.
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn vars_fetched_opens_view() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(
            &mut m,
            Event::VarsFetched(vec![var("editor", "vim"), var("workspace", "myproj")]),
        );
        assert_eq!(cmd, None);
        let vs = vars(&m);
        assert_eq!(vs.vars.len(), 2);
        assert_eq!(vs.selected, 0);
        assert_eq!(vs.edit, None);
    }

    #[test]
    fn vars_fetch_failed_stays_normal_with_error() {
        let mut m = Model::new(vec![mk("a")]);
        let cmd = update(&mut m, Event::VarsFetchFailed("boom".into()));
        assert_eq!(cmd, None);
        assert_eq!(m.mode, Mode::Normal);
        assert!(m.error.as_deref().unwrap_or("").contains("shpool var list"));
    }

    #[test]
    fn vars_browse_jk_move_and_wrap() {
        // Ports shperl's "vars browse: j/k move the cursor and wrap".
        let mut m = vars_model();
        update(&mut m, key(Key::Char(b'j')));
        assert_eq!(vars(&m).selected, 1, "j moves down");
        update(&mut m, key(Key::Char(b'j')));
        assert_eq!(vars(&m).selected, 0, "j wraps to top");
        update(&mut m, key(Key::Char(b'k')));
        assert_eq!(vars(&m).selected, 1, "k wraps to bottom");
    }

    #[test]
    fn vars_edit_e_opens_empty_field_then_enter_commits() {
        // `e` opens the value selector with an empty field (the current
        // value is just the highlighted row, not prefilled text); typing +
        // Enter commit the typed value. Asserts the emitted Command::SetVar,
        // not a shell-out.
        let mut m = vars_model(); // selected 0 = editor=vim
        update(&mut m, key(Key::Char(b'e')));
        let e = edit(&m);
        assert_eq!(e.field, "", "field starts empty (no prefill)");
        assert_eq!(e.filter, "", "filter starts empty");
        assert_eq!(e.highlight, 0, "highlight starts at the top");
        assert_eq!(
            e.candidates,
            vec!["vim".to_string()],
            "candidates harvested ({{editor}}-notes -> vim)"
        );
        // Type "nano".
        for b in b"nano" {
            update(&mut m, key(Key::Char(*b)));
        }
        assert_eq!(edit(&m).field, "nano", "typed new value into the field");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::SetVar {
                name: "editor".into(),
                value: "nano".into(),
            }),
        );
    }

    #[test]
    fn vars_edit_accepts_spaces_in_value() {
        // Values may hold spaces (unlike session names) — the create
        // prompt drops them, the vars edit field keeps them. The field
        // starts empty now, so no prefill to clear first.
        let mut m = vars_model();
        update(&mut m, key(Key::Char(b'e')));
        for b in b"key bugfix" {
            update(&mut m, key(Key::Char(*b)));
        }
        assert_eq!(edit(&m).field, "key bugfix");
    }

    #[test]
    fn vars_edit_esc_cancels_but_stays_in_view() {
        // Ports "vars edit: Esc cancels the edit but stays in the view".
        let mut m = vars_model();
        update(&mut m, key(Key::Char(b'e')));
        update(&mut m, key(Key::Char(b'x')));
        assert_eq!(edit(&m).field, "x", "typed into the field");
        update(&mut m, key(Key::Esc));
        assert_eq!(vars(&m).edit, None, "edit cancelled");
        assert!(matches!(m.mode, Mode::Vars(_)), "still in the vars view");
    }

    // -- value-selector state machine --

    /// A model mid-edit on a single variable `v` (current `djt`), with
    /// sessions shaped so `{v}-edit` harvests the given captured values
    /// (current value first via candidate_values). Mirrors shperl's
    /// vars_edit_model.
    fn vars_edit_model(cand_names: &[&str]) -> Model {
        let mut sessions = vec![attached_sess("djt-edit", "{v}-edit", 1)];
        for name in cand_names {
            sessions.push(detached_sess(&format!("{name}-edit")));
        }
        let mut m = Model::new(sessions);
        m.mode = Mode::Vars(VarsState {
            vars: vec![var("v", "djt")],
            selected: 0,
            edit: Option::None,
        });
        // Open the selector via the real key path so the harvest runs.
        update(&mut m, key(Key::Char(b'e')));
        m
    }

    fn detached_sess(name: &str) -> Session {
        Session {
            name: name.to_string(),
            attached: false,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
            attachments: Vec::new(),
        }
    }

    /// `shown` = the filtered/ranked candidate list for the current filter.
    fn shown(m: &Model) -> Vec<String> {
        let e = edit(m);
        filter_rank(&e.candidates, &e.filter)
    }

    #[test]
    fn selector_arrows_fill_field_and_do_not_refilter() {
        // Ports shperl's "value selector: arrows fill the field and do not
        // re-filter". Down copies the highlighted candidate into the field
        // while the filter stays frozen; Down clamps at the last row; Up
        // walks back.
        let mut m = vars_edit_model(&["xmr", "xrp"]); // cands: djt, xmr, xrp
        assert_eq!(
            edit(&m).candidates,
            vec!["djt".to_string(), "xmr".to_string(), "xrp".to_string()],
            "candidates harvested",
        );
        update(&mut m, key(Key::Down));
        assert_eq!(edit(&m).highlight, 1, "Down moves the highlight");
        assert_eq!(
            edit(&m).field,
            "xmr",
            "highlighted candidate copied into field"
        );
        assert_eq!(edit(&m).filter, "", "filter stays frozen while arrowing");
        update(&mut m, key(Key::Down));
        assert_eq!(edit(&m).field, "xrp", "Down again -> next candidate");
        update(&mut m, key(Key::Down));
        assert_eq!(edit(&m).highlight, 2, "Down clamps at the last row");
        update(&mut m, key(Key::Up));
        assert_eq!(edit(&m).field, "xmr", "Up walks back up the list");
    }

    #[test]
    fn selector_typing_filters_and_resets_highlight() {
        // Ports "value selector: typing filters and resets the highlight".
        let mut m = vars_edit_model(&["xmr", "xrp"]); // cands: djt, xmr, xrp
        update(&mut m, key(Key::Char(b'x')));
        assert_eq!(edit(&m).field, "x", "char appended to the field");
        assert_eq!(edit(&m).filter, "x", "filter follows the field when typing");
        assert_eq!(
            edit(&m).highlight,
            0,
            "highlight at the top after first char"
        );
        assert_eq!(shown(&m), vec!["xmr", "xrp"], "shown narrowed to x-matches");
        update(&mut m, key(Key::Char(b'm')));
        assert_eq!(edit(&m).filter, "xm", "filter keeps following the field");
        assert_eq!(shown(&m), vec!["xmr"], "filter narrowed further to xm");

        // A typing keystroke snaps the highlight back to the top even after
        // an arrow moved it.
        let mut m = vars_edit_model(&["xmr", "xrp"]);
        update(&mut m, key(Key::Char(b'x'))); // shown = [xmr, xrp]
        update(&mut m, key(Key::Down)); // highlight 1
        assert_eq!(
            edit(&m).highlight,
            1,
            "arrowed down within the filtered list"
        );
        update(&mut m, key(Key::Char(b'q'))); // any printable
        assert_eq!(
            edit(&m).highlight,
            0,
            "highlight reset to the top by typing"
        );
    }

    #[test]
    fn selector_arrow_then_type_extends_field_and_filters() {
        // Ports "value selector: arrow-then-type (field becomes prefix,
        // filter follows)".
        let mut m = vars_edit_model(&["xmr", "xrp"]); // cands: djt, xmr, xrp
        update(&mut m, key(Key::Down)); // highlight xmr, field=xmr
        assert_eq!(edit(&m).field, "xmr", "arrow copied xmr into the field");
        update(&mut m, key(Key::Char(b'z'))); // type onto the copied text
        assert_eq!(
            edit(&m).field,
            "xmrz",
            "typed char extends the arrowed-in value"
        );
        assert_eq!(edit(&m).filter, "xmrz", "filter now equals the field");
        assert_eq!(
            edit(&m).highlight,
            0,
            "highlight reset by the typing keystroke"
        );
        assert!(shown(&m).is_empty(), "nothing matches xmrz");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::SetVar {
                name: "v".into(),
                value: "xmrz".into(),
            }),
            "Enter applies the free-text field",
        );
    }

    #[test]
    fn selector_non_arrow_key_is_consumed_without_touching_field() {
        // Ports shperl's "a non-arrow CSI final is consumed, not typed":
        // an unmapped key (Key::Other) or Tab is swallowed by the edit
        // branch — field/filter/highlight unchanged, no command emitted.
        let mut m = vars_edit_model(&["xmr", "xrp"]); // cands: djt, xmr, xrp
        update(&mut m, key(Key::Down)); // highlight 1, field=xmr, filter frozen
        assert_eq!(
            update(&mut m, key(Key::Other)),
            None,
            "Key::Other emits no command"
        );
        assert_eq!(
            edit(&m).field,
            "xmr",
            "Key::Other leaves the field untouched"
        );
        assert_eq!(
            edit(&m).filter,
            "",
            "Key::Other leaves the filter untouched"
        );
        assert_eq!(
            edit(&m).highlight,
            1,
            "Key::Other leaves the highlight untouched"
        );
        assert_eq!(update(&mut m, key(Key::Tab)), None, "Tab emits no command");
        assert_eq!(edit(&m).field, "xmr", "Tab leaves the field untouched");
        assert_eq!(edit(&m).highlight, 1, "Tab leaves the highlight untouched");
    }

    #[test]
    fn selector_empty_field_enter_keeps_current_value() {
        // Ports "value selector: empty field => Enter keeps the current
        // value" — the highlighted row is the current value, shown first.
        let mut m = vars_edit_model(&["xmr", "xrp"]); // cands: djt(current), xmr, xrp
        assert_eq!(edit(&m).field, "", "field empty on entry");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::SetVar {
                name: "v".into(),
                value: "djt".into(),
            }),
            "empty field -> highlighted row (the current value) is applied",
        );
    }

    #[test]
    fn selector_empty_field_after_backspacing_keeps_current() {
        // Ports "value selector: empty field after backspacing also keeps
        // current".
        let mut m = vars_edit_model(&["xmr", "xrp"]);
        update(&mut m, key(Key::Char(b'x'))); // field=x
        update(&mut m, key(Key::Backspace)); // -> empty
        assert_eq!(edit(&m).field, "", "backspaced to empty");
        assert_eq!(edit(&m).highlight, 0, "highlight back at the top");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::SetVar {
                name: "v".into(),
                value: "djt".into(),
            }),
            "still applies the current value",
        );
    }

    #[test]
    fn selector_literal_field_is_not_a_dead_zone() {
        // Ports "value selector: literal field is not a dead zone (xm vs
        // xmr)" — Enter applies the literal `xm` even while `xmr` is the
        // shown suggestion.
        let mut m = vars_edit_model(&["xmr"]); // cands: djt, xmr
        for b in b"xm" {
            update(&mut m, key(Key::Char(*b)));
        }
        assert_eq!(edit(&m).field, "xm", "field holds the literal typed text");
        assert_eq!(shown(&m), vec!["xmr"], "xmr shown as the suggestion");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::SetVar {
                name: "v".into(),
                value: "xm".into(),
            }),
            "Enter applies the literal xm, not the shown xmr",
        );
    }

    #[test]
    fn selector_arrow_to_suggestion_then_enter_applies_it() {
        // Ports "value selector: arrowing to a suggestion then Enter
        // applies it".
        let mut m = vars_edit_model(&["xmr"]); // cands: djt, xmr
        for b in b"xm" {
            update(&mut m, key(Key::Char(*b)));
        }
        update(&mut m, key(Key::Down)); // highlight xmr, field=xmr
        assert_eq!(
            edit(&m).field,
            "xmr",
            "arrow filled the field with the suggestion"
        );
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::SetVar {
                name: "v".into(),
                value: "xmr".into(),
            }),
            "now xmr is applied",
        );
    }

    #[test]
    fn selector_esc_cancels_and_clears_edit_state() {
        // Ports "value selector: Esc cancels and clears the edit state".
        let mut m = vars_edit_model(&["xmr", "xrp"]);
        update(&mut m, key(Key::Down)); // field=xmr
        update(&mut m, key(Key::Esc));
        assert_eq!(vars(&m).edit, None, "no longer editing");
        assert!(matches!(m.mode, Mode::Vars(_)), "still in the vars view");
    }

    #[test]
    fn vars_browse_esc_and_q_both_leave() {
        // Ports "vars browse: Esc and q both leave the view".
        let mut m = vars_model();
        update(&mut m, key(Key::Esc));
        assert_eq!(m.mode, Mode::Normal, "Esc returns to the session list");
        let mut m2 = vars_model();
        update(&mut m2, key(Key::Char(b'q')));
        assert_eq!(m2.mode, Mode::Normal, "q returns to the session list");
    }

    #[test]
    fn vars_browse_uppercase_q_does_not_leave() {
        // `Q` is not a leave key in the vars view (matches shperl) —
        // it's ignored, and the view stays up.
        let mut m = vars_model();
        update(&mut m, key(Key::Char(b'Q')));
        assert!(matches!(m.mode, Mode::Vars(_)));
    }

    #[test]
    fn vars_set_finished_ok_applies_list_and_refreshes() {
        // On success the edit line clears, the refetched list is applied
        // (cursor preserved), and a Refresh is emitted to re-dial the
        // preview.
        let mut m = vars_model();
        update(&mut m, key(Key::Char(b'e'))); // open edit on editor
        let cmd = update(
            &mut m,
            Event::VarSetFinished {
                name: "editor".into(),
                ok: true,
                err: Option::None,
                vars: Some(vec![var("editor", "nano"), var("workspace", "myproj")]),
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        let vs = vars(&m);
        assert_eq!(vs.edit, None, "edit line cleared");
        assert_eq!(vs.vars[0].value, "nano", "new list applied");
        assert_eq!(vs.selected, 0, "cursor preserved");
    }

    #[test]
    fn vars_set_finished_ok_with_no_refetch_keeps_old_list() {
        // A successful set whose refetch failed (vars: None) keeps the
        // prior list silently — still refreshes sessions.
        let mut m = vars_model();
        let cmd = update(
            &mut m,
            Event::VarSetFinished {
                name: "editor".into(),
                ok: true,
                err: Option::None,
                vars: Option::None,
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        let vs = vars(&m);
        assert_eq!(vs.vars[0].value, "vim", "old list kept");
    }

    #[test]
    fn vars_set_finished_fail_sets_error_no_refresh() {
        let mut m = vars_model();
        let cmd = update(
            &mut m,
            Event::VarSetFinished {
                name: "editor".into(),
                ok: false,
                err: Some("not allowed".into()),
                vars: Option::None,
            },
        );
        assert_eq!(cmd, None, "no refresh on failure");
        assert_eq!(m.error.as_deref(), Some("var set editor: not allowed"));
        // Edit line still cleared; still in the view.
        assert_eq!(vars(&m).edit, None);
    }

    #[test]
    fn vars_set_finished_fail_without_stderr_uses_generic() {
        let mut m = vars_model();
        update(
            &mut m,
            Event::VarSetFinished {
                name: "editor".into(),
                ok: false,
                err: Option::None,
                vars: Option::None,
            },
        );
        assert_eq!(m.error.as_deref(), Some("var set editor failed"));
    }

    #[test]
    fn vars_keystroke_does_not_auto_refresh() {
        // The auto-refresh fallback is gated on Mode::Normal, so a
        // browse keystroke in the vars view never storms shpool with
        // list calls.
        let mut m = vars_model();
        assert_eq!(update(&mut m, key(Key::Char(b'j'))), None);
    }

    #[test]
    fn vars_e_on_empty_list_is_noop() {
        let mut m = Model::new(vec![]);
        m.mode = Mode::Vars(VarsState {
            vars: vec![],
            selected: 0,
            edit: Option::None,
        });
        assert_eq!(update(&mut m, key(Key::Char(b'e'))), None);
        assert_eq!(vars(&m).edit, None, "no edit opened on empty list");
    }

    // -- Feature A: union of unset rows (the update wiring) --

    use super::super::template::resolve_template;
    use crate::session::Attachment;

    /// A session whose single attachment carries `tmpl`.
    fn tmpl_session(name: &str, tmpl: &str) -> Session {
        Session {
            name: name.to_string(),
            attached: true,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
            attachments: vec![Attachment {
                session_name_template: tmpl.to_string(),
                pid: 1,
            }],
        }
    }

    /// Flatten a vars list to `name=value` / `name(unset)` tokens.
    fn merged_repr(vs: &VarsState) -> Vec<String> {
        vs.vars
            .iter()
            .map(|v| {
                if v.unset {
                    format!("{}(unset)", v.name)
                } else {
                    format!("{}={}", v.name, v.value)
                }
            })
            .collect()
    }

    #[test]
    fn vars_fetched_merges_unset_rows_against_sessions() {
        // A.3a: opening the view surfaces the unset rows the current
        // sessions reference, interleaved with the set rows by name.
        let mut m = Model::new(vec![tmpl_session("B-x", "{b}-x")]);
        let cmd = update(&mut m, Event::VarsFetched(vec![var("a", "1")]));
        assert_eq!(cmd, None);
        assert_eq!(merged_repr(vars(&m)), ["a=1", "b(unset)"]);
        assert_eq!(vars(&m).selected, 0, "cursor resets to the top");
    }

    #[test]
    fn vars_set_finished_keeps_unset_sibling_and_cursor_by_name() {
        // A.3b refetch regression: two unset siblings b, c; the cursor is
        // on b. A successful set of b refetches the set rows; b is promoted
        // and c must survive as unset, with the cursor still on b by name
        // after the resort.
        let mut m = Model::new(vec![
            tmpl_session("B-x", "{b}-x"),
            tmpl_session("C-x", "{c}-x"),
        ]);
        update(&mut m, Event::VarsFetched(vec![]));
        assert_eq!(merged_repr(vars(&m)), ["b(unset)", "c(unset)"]);
        // Cursor is at 0 (b). Open the selector so the edit line is live,
        // then deliver the set result with a refetched set list.
        update(&mut m, key(Key::Char(b'e')));
        let cmd = update(
            &mut m,
            Event::VarSetFinished {
                name: "b".into(),
                ok: true,
                err: Option::None,
                vars: Some(vec![var("b", "foo")]),
            },
        );
        assert_eq!(cmd, Some(Command::Refresh));
        assert_eq!(
            merged_repr(vars(&m)),
            ["b=foo", "c(unset)"],
            "b promoted; c survives as unset"
        );
        let vs = vars(&m);
        assert_eq!(
            vs.vars[vs.selected].name, "b",
            "cursor stays on the promoted var by name"
        );
        assert_eq!(vs.edit, None, "edit line cleared");
    }

    #[test]
    fn sessions_refreshed_in_vars_mode_adds_unset_row_and_keeps_cursor() {
        // A.3c session-refresh regression (add): a refresh introducing a
        // session that references a new var surfaces it as an unset row,
        // cursor held by name.
        let mut m = Model::new(vec![tmpl_session("B-x", "{b}-x")]);
        update(&mut m, Event::VarsFetched(vec![var("a", "1")]));
        assert_eq!(merged_repr(vars(&m)), ["a=1", "b(unset)"]);
        // Move the cursor onto b.
        update(&mut m, key(Key::Char(b'j')));
        assert_eq!(vars(&m).vars[vars(&m).selected].name, "b");

        update(
            &mut m,
            Event::SessionsRefreshed(vec![
                tmpl_session("B-x", "{b}-x"),
                tmpl_session("C-x", "{c}-x"),
            ]),
        );
        assert_eq!(merged_repr(vars(&m)), ["a=1", "b(unset)", "c(unset)"]);
        let vs = vars(&m);
        assert_eq!(vs.vars[vs.selected].name, "b", "cursor held on b by name");
    }

    #[test]
    fn sessions_refreshed_in_vars_mode_removes_unset_row_and_clamps_cursor() {
        // A.3c (remove + clamp): a refresh that drops the only session
        // referencing the selected unset var removes its row; the cursor,
        // whose var vanished, clamps to the last row.
        let mut m = Model::new(vec![
            tmpl_session("B-x", "{b}-x"),
            tmpl_session("C-x", "{c}-x"),
        ]);
        update(&mut m, Event::VarsFetched(vec![]));
        assert_eq!(merged_repr(vars(&m)), ["b(unset)", "c(unset)"]);
        update(&mut m, key(Key::Char(b'j'))); // cursor on c
        assert_eq!(vars(&m).vars[vars(&m).selected].name, "c");

        update(
            &mut m,
            Event::SessionsRefreshed(vec![tmpl_session("B-x", "{b}-x")]),
        );
        assert_eq!(merged_repr(vars(&m)), ["b(unset)"]);
        assert_eq!(vars(&m).selected, 0, "cursor clamped into bounds");
    }

    #[test]
    fn map_unchanged_candidate_harvest_ignores_an_unset_co_var() {
        // A.1 map-unchanged (candidate_values site): workspace is set;
        // editor is an unset co-var in the template. Opening the value
        // selector on workspace must harvest the same candidates as if the
        // unset row were absent — {editor} stays literal, so the
        // "{editor}-" prefix never spuriously matches and collapses.
        let sessions = vec![
            tmpl_session("{editor}-myproj", "{editor}-{workspace}"),
            // detached target carrying the literal-prefix name.
            Session {
                name: "{editor}-demo".into(),
                attached: false,
                started_at_unix_ms: 0,
                last_connected_at_unix_ms: 0,
                last_disconnected_at_unix_ms: None,
                attachments: Vec::new(),
            },
        ];
        // Pre-union baseline: harvest with only the real row in the map.
        // Computed before `sessions` moves into the model.
        let real = [var("workspace", "myproj")];
        let map = resolution_map(&real);
        let pre_union = candidate_values(&sessions, &map, "workspace");

        let mut m = Model::new(sessions);
        update(&mut m, Event::VarsFetched(vec![var("workspace", "myproj")]));
        // editor surfaced as an unset co-var row.
        assert!(
            vars(&m).vars.iter().any(|v| v.name == "editor" && v.unset),
            "editor surfaced as an unset row"
        );
        // Point the cursor at the set var and open the selector.
        let wi = vars(&m)
            .vars
            .iter()
            .position(|v| v.name == "workspace")
            .unwrap();
        if let Mode::Vars(vs) = &mut m.mode {
            vs.selected = wi;
        }
        update(&mut m, key(Key::Char(b'e')));
        let with_union = edit(&m).candidates.clone();

        assert_eq!(
            with_union, pre_union,
            "candidate harvest is identical with vs. without the unset row"
        );
        assert_eq!(
            with_union,
            vec!["myproj".to_string(), "demo".to_string()],
            "captures are the remainders after the literal {{editor}}- prefix"
        );
    }

    #[test]
    fn map_unchanged_preview_keeps_unset_co_var_literal() {
        // A.1 map-unchanged (preview site): the re-dial preview for a set
        // var whose template also mentions an unset var leaves the unset
        // token literal, not collapsed to empty. Resolve against the same
        // map the preview uses (real rows only).
        let sessions = vec![tmpl_session("{editor}-myproj", "{editor}-{workspace}")];
        let mut m = Model::new(sessions);
        update(&mut m, Event::VarsFetched(vec![var("workspace", "myproj")]));
        let vs = vars(&m);
        let map = resolution_map(&vs.vars);
        assert_eq!(
            resolve_template("{editor}-{workspace}", &map),
            "{editor}-myproj",
            "unknown {{editor}} stays literal, not collapsed to -myproj"
        );
    }

    #[test]
    fn set_but_empty_var_stays_a_set_row_through_fetch() {
        // A var with an empty value is a real (set) row, never flagged
        // unset, even when its name is referenced by a template.
        let mut m = Model::new(vec![tmpl_session("-x", "{a}-x")]);
        update(&mut m, Event::VarsFetched(vec![var("a", "")]));
        let vs = vars(&m);
        assert_eq!(vs.vars.len(), 1);
        assert!(!vs.vars[0].unset, "set-but-empty var is not flagged unset");
        assert_eq!(vs.vars[0].value, "");
    }

    // -- Feature B: create-time variable prompt --

    /// Borrow the VarPromptState out of a model parked in the prompt,
    /// panicking otherwise — keeps tests asserting on fields.
    fn vp(m: &Model) -> &VarPromptState {
        let Mode::CreateVarPrompt(vp) = &m.mode else {
            panic!("expected Mode::CreateVarPrompt, got {:?}", m.mode);
        };
        vp
    }

    /// A model parked in the create-var prompt over `vars` (with no
    /// pre-set co-vars), as the CreateNeedsVars handler builds it.
    fn prompt_model(name: &str, names: &[&str]) -> Model {
        let mut m = Model::new(vec![]);
        update(
            &mut m,
            Event::CreateNeedsVars {
                name: name.to_string(),
                vars: names.iter().map(|s| s.to_string()).collect(),
                set_vars: Vec::new(),
            },
        );
        m
    }

    /// Feed a run of printable chars as individual keystrokes.
    fn type_str(m: &mut Model, s: &str) {
        for b in s.bytes() {
            update(m, key(Key::Char(b)));
        }
    }

    #[test]
    fn create_needs_vars_opens_prompt() {
        // The detect-step event opens the prompt at idx 0 with empty input
        // and carries the set-var snapshot for the preview.
        let mut m = Model::new(vec![]);
        let cmd = update(
            &mut m,
            Event::CreateNeedsVars {
                name: "{a}-{b}".into(),
                vars: vec!["a".into(), "b".into()],
                set_vars: vec![("c".into(), "see".into())],
            },
        );
        assert_eq!(cmd, None, "opening the prompt emits no command");
        let p = vp(&m);
        assert_eq!(p.name, "{a}-{b}");
        assert_eq!(p.vars, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(p.idx, 0);
        assert_eq!(p.input, "");
        assert!(p.collected.is_empty());
        assert_eq!(p.set_vars, vec![("c".to_string(), "see".to_string())]);
    }

    #[test]
    fn create_prompt_single_var_collects_then_emits() {
        // A single unknown var: type a value, Enter emits CreateWithVars
        // with the one pair and drops to Normal.
        let mut m = prompt_model("{a}-x", &["a"]);
        type_str(&mut m, "hello");
        assert_eq!(vp(&m).input, "hello", "input accumulates printable bytes");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::CreateWithVars {
                name: "{a}-x".into(),
                set_vars: vec![("a".into(), "hello".into())],
            }),
            "Enter on the last var emits the apply command with the pair"
        );
        assert_eq!(
            m.mode,
            Mode::Normal,
            "dropped to Normal at apply-emit (B.4)"
        );
    }

    #[test]
    fn create_prompt_multiple_vars_collected_in_order() {
        let mut m = prompt_model("{a}-{b}", &["a", "b"]);
        type_str(&mut m, "one");
        update(&mut m, key(Key::Enter));
        assert!(
            matches!(m.mode, Mode::CreateVarPrompt(_)),
            "still prompting after the first var"
        );
        assert_eq!(vp(&m).idx, 1, "advanced to the second var");
        assert_eq!(vp(&m).input, "", "input reset for the next var");
        assert_eq!(
            vp(&m).collected,
            vec![("a".to_string(), "one".to_string())],
            "first pair stored"
        );
        type_str(&mut m, "two");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::CreateWithVars {
                name: "{a}-{b}".into(),
                set_vars: vec![("a".into(), "one".into()), ("b".into(), "two".into())],
            }),
            "both pairs emitted in template order"
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_prompt_empty_entry_skips_that_var() {
        let mut m = prompt_model("{a}-{b}", &["a", "b"]);
        // Enter on empty input for 'a' -> skip it.
        update(&mut m, key(Key::Enter));
        assert_eq!(vp(&m).idx, 1, "advanced past the skipped var");
        assert!(
            vp(&m).collected.is_empty(),
            "nothing collected for the empty entry"
        );
        type_str(&mut m, "bee");
        let cmd = update(&mut m, key(Key::Enter));
        assert_eq!(
            cmd,
            Some(Command::CreateWithVars {
                name: "{a}-{b}".into(),
                set_vars: vec![("b".into(), "bee".into())],
            }),
            "only the non-empty var is in the emitted pairs"
        );
    }

    #[test]
    fn create_prompt_all_entries_skipped_emits_empty_pair_list() {
        // All-skip -> CreateWithVars with set_vars: [] (attach with the
        // name resolving empty — intentionally today's behavior).
        let mut m = prompt_model("{a}-{b}", &["a", "b"]);
        update(&mut m, key(Key::Enter)); // skip a
        let cmd = update(&mut m, key(Key::Enter)); // skip b
        assert_eq!(
            cmd,
            Some(Command::CreateWithVars {
                name: "{a}-{b}".into(),
                set_vars: vec![],
            }),
            "all-skip -> CreateWithVars with an empty set_vars list"
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_prompt_backspace_edits_input() {
        let mut m = prompt_model("{a}", &["a"]);
        type_str(&mut m, "abc");
        update(&mut m, key(Key::Backspace));
        assert_eq!(vp(&m).input, "ab", "Backspace removes the last char");
    }

    #[test]
    fn create_prompt_accepts_spaces() {
        // Values may hold spaces (unlike session names) — the prompt keeps
        // them, mirroring shperl's 0x20..=0x7e printable rule.
        let mut m = prompt_model("{a}", &["a"]);
        type_str(&mut m, "key bugfix");
        assert_eq!(vp(&m).input, "key bugfix");
    }

    #[test]
    fn create_prompt_esc_cancels_nothing_collected() {
        let mut m = prompt_model("{a}-{b}", &["a", "b"]);
        type_str(&mut m, "partial");
        // Esc cancels: returns to Normal (which then auto-refreshes, like
        // the CreateInput Esc path), prompt torn down, nothing set.
        let cmd = update(&mut m, key(Key::Esc));
        assert_eq!(cmd, Some(Command::Refresh));
        assert_eq!(m.mode, Mode::Normal, "Esc returns to Normal mode");
    }

    #[test]
    fn create_prompt_ctrl_c_cancels_like_esc() {
        let mut m = prompt_model("{a}", &["a"]);
        let cmd = update(&mut m, key(Key::Ctrl(0x03)));
        assert_eq!(cmd, Some(Command::Refresh), "Ctrl-C cancels to Normal");
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn create_prompt_arrow_keys_ignored() {
        let mut m = prompt_model("{a}", &["a"]);
        update(&mut m, key(Key::Down));
        update(&mut m, key(Key::Up));
        assert_eq!(vp(&m).input, "", "CSI/arrow keys are not typed into input");
        assert_eq!(vp(&m).idx, 0, "still on the first var");
    }

    #[test]
    fn create_prompt_keystroke_does_not_auto_refresh() {
        // The Normal-only auto-refresh must not fire mid-prompt: typing a
        // value isn't a list-storm trigger.
        let mut m = prompt_model("{a}", &["a"]);
        assert_eq!(update(&mut m, key(Key::Char(b'x'))), None);
        // A non-final Enter (skip) also produces no command.
        let mut m2 = prompt_model("{a}-{b}", &["a", "b"]);
        assert_eq!(update(&mut m2, key(Key::Enter)), None);
    }

    #[test]
    fn create_vars_failed_parks_error_in_normal_mode() {
        // The apply step failed on a var; the prompt already dropped to
        // Normal (B.4), so the parked error is visible (not hidden behind
        // a modal). Message names the failed var.
        let mut m = Model::new(vec![]);
        // Mode is Normal by the time CreateVarsFailed lands (B.4 dropped to
        // it at apply-emit).
        let cmd = update(
            &mut m,
            Event::CreateVarsFailed {
                var: "b".into(),
                err: Some("not allowed".into()),
            },
        );
        assert_eq!(cmd, None);
        assert_eq!(m.mode, Mode::Normal, "error surfaces in Normal mode");
        assert_eq!(m.error.as_deref(), Some("var set b: not allowed"));
    }

    #[test]
    fn create_vars_failed_without_stderr_uses_generic() {
        let mut m = Model::new(vec![]);
        update(
            &mut m,
            Event::CreateVarsFailed {
                var: "b".into(),
                err: Option::None,
            },
        );
        assert_eq!(m.error.as_deref(), Some("var set b failed"));
    }

    #[test]
    fn mid_prompt_session_refresh_leaves_prompt_intact() {
        // A background refresh (events/focus) during the prompt only
        // touches the session list — mode and VarPromptState survive.
        let mut m = prompt_model("{a}-x", &["a"]);
        type_str(&mut m, "half");
        update(&mut m, Event::SessionsRefreshed(vec![mk("other")]));
        assert!(
            matches!(m.mode, Mode::CreateVarPrompt(_)),
            "still in the prompt after a refresh"
        );
        assert_eq!(vp(&m).input, "half", "the in-progress input survives");
        assert_eq!(vp(&m).idx, 0, "prompt position unchanged");
    }

    #[test]
    fn create_prompt_first_keystroke_clears_parked_error() {
        // The existing keystroke-clears-error rule covers the prompt — a
        // parked error is dismissed on the first prompt keystroke, and no
        // second clear is needed in the handler.
        let mut m = prompt_model("{a}", &["a"]);
        m.set_error("stale background error");
        update(&mut m, key(Key::Char(b'x')));
        assert!(m.error.is_none(), "first keystroke cleared the error");
    }

    #[test]
    fn create_all_known_never_reaches_prompt() {
        // When every referenced var is set, the detect step (in the
        // executor) finds no unknowns and never emits CreateNeedsVars, so
        // update never opens the prompt. This asserts the pure helper the
        // detect step relies on returns empty for an all-known name; the
        // executor's branch is exercised by the detect logic in main.rs.
        use super::super::template::unknown_template_vars;
        let known: std::collections::HashSet<&str> = ["a", "b"].into_iter().collect();
        assert!(
            unknown_template_vars("{a}-{b}", &known).is_empty(),
            "all-known name -> no unknowns -> detect falls through to attach"
        );
    }
}
