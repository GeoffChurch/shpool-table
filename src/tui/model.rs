use std::collections::HashMap;

use super::template::template_vars;
use crate::session::Session;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    CreateInput(String),
    ConfirmKill(String),
    ConfirmForce(String),
    /// The create-time variable prompt: a new session whose name
    /// references vars not yet set walks each unknown var (in
    /// `template_vars` order), collecting a value, then sets the
    /// non-empty ones and attaches. A bottom-bar mode like CreateInput —
    /// the session list still renders behind it.
    CreateVarPrompt(VarPromptState),
    /// The template-variable view. Carries its own scoped state so no
    /// stale vars data lingers in the other modes.
    Vars(VarsState),
}

/// Per-var prompt state for a new session whose name references vars not
/// yet set. `vars` is the ordered unknowns to walk; `idx` is the current
/// one; `input` is the value typed so far; `collected` accumulates the
/// non-empty `(var, value)` pairs to apply; `set_vars` is the snapshot of
/// already-set vars from the detect-time `var list`, kept only to feed the
/// live name preview (future-unknown vars are deliberately omitted from
/// the preview map so they render literal).
#[derive(Debug, Clone, PartialEq)]
pub struct VarPromptState {
    pub name: String,
    pub vars: Vec<String>,
    pub idx: usize,
    pub input: String,
    pub collected: Vec<(String, String)>,
    pub set_vars: Vec<(String, String)>,
}

/// One of the daemon's template variables. `unset` flags a synthetic
/// row — a variable an attached template references but `var list`
/// doesn't carry — surfaced by the vars view as a dimmed `(unset)` row.
/// Real (set) rows have `unset: false`, including a legitimately set
/// but empty-valued var: emptiness is never inferred as unset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Var {
    pub name: String,
    pub value: String,
    pub unset: bool,
}

/// The `name -> value` map fed to `resolve_template` / `candidate_values`.
/// Built from real (`!unset`) rows only: a synthetic unset row carries no
/// value, and slipping an `editor -> ""` in here would collapse a literal
/// `{editor}` in a preview to empty. The single source of that map, used
/// wherever the vars view resolves a template.
pub fn resolution_map(vars: &[Var]) -> HashMap<&str, &str> {
    vars.iter()
        .filter(|v| !v.unset)
        .map(|v| (v.name.as_str(), v.value.as_str()))
        .collect()
}

/// Surface variables a template references but `var list` doesn't carry:
/// union the template vars across every attachment of every session, drop
/// the ones already in `var_list`, and append a synthetic
/// `Var { unset: true }` row for each remainder. The result is re-sorted
/// by name so set and unset rows interleave alphabetically, matching the
/// list view's ordering.
///
/// Templates live only on attached sessions (`attachments`), so a var
/// referenced solely by a detached session contributes nothing — same
/// scope as `attachments_for_var`. A var that is both set and referenced
/// stays a single (set) row; a var referenced by several templates yields
/// one unset row.
pub fn merge_unset_vars(var_list: &[Var], sessions: &[Session]) -> Vec<Var> {
    let set: std::collections::HashSet<&str> = var_list.iter().map(|v| v.name.as_str()).collect();
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in sessions {
        for a in &s.attachments {
            for name in template_vars(&a.session_name_template) {
                referenced.insert(name);
            }
        }
    }
    let mut merged = var_list.to_vec();
    for name in referenced {
        if !set.contains(name.as_str()) {
            merged.push(Var {
                name,
                value: String::new(),
                unset: true,
            });
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

/// Re-derive the displayed variable list — set rows ∪ the unset rows the
/// current sessions reference — preserving the cursor on the same variable
/// by name. The list is a function of (real vars, sessions), so anything
/// that changes either input while in the view (a `var set`, a session
/// refresh) runs through here. The cursor moves by name because the merge
/// re-sorts and a set can both add and remove rows (promoting an unset row
/// to a set one, shifting indices); on no match (only a concurrent external
/// `var unset` can drop the selected name) it clamps to the last row. Prior
/// synthetic rows are filtered out first so they never feed back into the
/// union as if they were set.
pub fn remerge_preserving_cursor(vs: &mut VarsState, sessions: &[Session]) {
    let prev_name = vs.vars.get(vs.selected).map(|v| v.name.clone());
    let real: Vec<Var> = vs.vars.iter().filter(|v| !v.unset).cloned().collect();
    vs.vars = merge_unset_vars(&real, sessions);
    vs.selected = prev_name
        .and_then(|name| vs.vars.iter().position(|v| v.name == name))
        .unwrap_or_else(|| vs.vars.len().saturating_sub(1));
}

/// State of the template-variable view: the snapshot of variables, the
/// cursor into them, and the value selector. `edit` is `None` while
/// browsing and `Some(EditState)` while picking/typing a value. The
/// governed-attachment preview is derived from `model.sessions` on
/// demand, never stored here.
#[derive(Debug, Clone, PartialEq)]
pub struct VarsState {
    pub vars: Vec<Var>,
    pub selected: usize,
    pub edit: Option<EditState>,
}

/// The value selector's two-slot state, held while editing a variable.
/// `field` is the text Enter applies; `filter` is `field` as of the last
/// typing keystroke (frozen while arrowing) and is what `candidates`
/// filters by; `highlight` indexes the filtered (shown) list.
#[derive(Debug, Clone, PartialEq)]
pub struct EditState {
    /// Editable text — the value Enter applies. Starts empty.
    pub field: String,
    /// The list filter: `field` as of the last typing keystroke.
    pub filter: String,
    /// All harvested candidate values, fixed for the duration of the edit.
    pub candidates: Vec<String>,
    /// Index into the filtered (shown) list.
    pub highlight: usize,
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
        let selection = if sessions.is_empty() {
            Selection::None
        } else {
            Selection::At(0)
        };
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
        let Selection::At(i) = self.selection else {
            return;
        };
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
        let was_stale = matches!(self.selection, Selection::Stale(_));
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

        // Already stale: stay stale until the user acks, even if a
        // same-named session reappears. A reappearance is a recreated,
        // different instance; silently adopting it would hide that the
        // original is gone — and could land an attach/kill on the new
        // session while the user believes it's the original.
        if was_stale {
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
            attachments: Vec::new(),
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
            attachments: Vec::new(),
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
    fn refresh_stale_persists_when_same_named_session_reappears() {
        // A reappeared same-named session is a recreated, different
        // instance — stay stale so the user is told the original is gone
        // rather than silently adopting the new one.
        let mut m = Model::new(vec![mk("a")]);
        m.selection = Selection::Stale("b".into());
        m.refresh(vec![mk("a"), mk("b")]);
        assert_eq!(m.selection, Selection::Stale("b".into()));
        assert_eq!(m.selected_name(), None);
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

    // -- Feature A: union of unset rows --

    use crate::session::Attachment;

    /// A session whose single attachment carries `tmpl`. Bare-bones: the
    /// unset derivation only reads `attachments[].session_name_template`.
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

    /// A detached session: exists by name but carries no attachments, so
    /// its (notional) template is invisible to the unset derivation.
    fn detached_session(name: &str) -> Session {
        mk(name)
    }

    /// Reconstruct a session list (Session isn't Clone, but Attachment is)
    /// so a test can both seed a model and keep a copy to re-merge against.
    fn clone_sessions(sessions: &[Session]) -> Vec<Session> {
        sessions
            .iter()
            .map(|s| Session {
                name: s.name.clone(),
                attached: s.attached,
                started_at_unix_ms: s.started_at_unix_ms,
                last_connected_at_unix_ms: s.last_connected_at_unix_ms,
                last_disconnected_at_unix_ms: s.last_disconnected_at_unix_ms,
                attachments: s.attachments.clone(),
            })
            .collect()
    }

    fn set_var(name: &str, value: &str) -> Var {
        Var {
            name: name.to_string(),
            value: value.to_string(),
            unset: false,
        }
    }

    /// Flatten a merged list to `name=value` / `name(unset)` tokens, in
    /// list order, for compact structural assertions (mirrors shperl's
    /// `merged_repr`).
    fn merged_repr(vars: &[Var]) -> Vec<String> {
        vars.iter()
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
    fn merge_unset_vars_referenced_but_absent_become_unset_rows_sorted() {
        let sessions = vec![
            tmpl_session("A-x", "{a}-x"),
            tmpl_session("B-x", "{b}-x"),
            tmpl_session("C-x", "{c}-x"),
        ];
        let merged = merge_unset_vars(&[set_var("a", "1")], &sessions);
        assert_eq!(merged_repr(&merged), ["a=1", "b(unset)", "c(unset)"]);
    }

    #[test]
    fn merge_unset_vars_set_and_referenced_stays_a_single_set_row() {
        let sessions = vec![tmpl_session("1-x", "{a}-x")];
        let merged = merge_unset_vars(&[set_var("a", "1")], &sessions);
        assert_eq!(merged_repr(&merged), ["a=1"]);
    }

    #[test]
    fn merge_unset_vars_var_referenced_by_several_templates_yields_one_row() {
        let sessions = vec![
            tmpl_session("w-edit", "{w}-edit"),
            tmpl_session("w-term", "{w}-term"),
            tmpl_session("w-logs", "{w}-logs"),
        ];
        let merged = merge_unset_vars(&[], &sessions);
        assert_eq!(merged_repr(&merged), ["w(unset)"]);
    }

    #[test]
    fn merge_unset_vars_repeated_token_within_one_template_dedups() {
        let sessions = vec![tmpl_session("a-a", "{a}-{a}")];
        let merged = merge_unset_vars(&[], &sessions);
        assert_eq!(merged_repr(&merged), ["a(unset)"]);
    }

    #[test]
    fn merge_unset_vars_detached_session_surfaces_nothing() {
        let sessions = vec![
            detached_session("detached-edit"), // template invisible
            tmpl_session("plain", "plain"),    // no tokens
        ];
        let merged = merge_unset_vars(&[set_var("a", "1")], &sessions);
        assert_eq!(merged_repr(&merged), ["a=1"]);
    }

    #[test]
    fn merge_unset_vars_set_but_empty_var_stays_a_set_row() {
        // The value is "", but it is a real (set) row: emptiness must not
        // be confused with unset.
        let sessions = vec![tmpl_session("-x", "{a}-x")];
        let merged = merge_unset_vars(&[set_var("a", "")], &sessions);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].unset, "set-but-empty var is not flagged unset");
        assert_eq!(merged[0].value, "", "value stays the empty string");
    }

    /// A vars-mode model carrying a real (set) list plus sessions; the
    /// merge runs so the displayed list mirrors what VarsFetched produces.
    fn merged_vars_model(real: Vec<Var>, sessions: Vec<Session>) -> Model {
        let mut m = Model::new(sessions);
        let vars = merge_unset_vars(&real, &m.sessions);
        m.mode = Mode::Vars(VarsState {
            vars,
            selected: 0,
            edit: Option::None,
        });
        m
    }

    /// Borrow the VarsState out of a model parked in the vars view.
    fn vs_of(m: &mut Model) -> &mut VarsState {
        match &mut m.mode {
            Mode::Vars(vs) => vs,
            _ => panic!("expected Mode::Vars"),
        }
    }

    #[test]
    fn remerge_set_one_of_two_unset_siblings_other_survives_cursor_by_name() {
        // b and c are both unset siblings; the cursor sits on b. Replay the
        // successful-set sequence: fresh set rows (b now set), cursor
        // pointed at the promoted var, then a re-merge against the
        // (unchanged) sessions.
        let sessions = vec![tmpl_session("B-x", "{b}-x"), tmpl_session("C-x", "{c}-x")];
        let mut m = merged_vars_model(vec![], clone_sessions(&sessions));
        assert_eq!(merged_repr(&vs_of(&mut m).vars), ["b(unset)", "c(unset)"]);
        vs_of(&mut m).selected = 0; // cursor on b

        // Post-set: fresh `var list` (b promoted), cursor at the set var,
        // then a re-merge against the fresh sessions.
        let vs = vs_of(&mut m);
        vs.vars = vec![set_var("b", "foo")];
        vs.selected = vs.vars.iter().position(|v| v.name == "b").unwrap();
        remerge_preserving_cursor(vs, &sessions);

        assert_eq!(merged_repr(&vs_of(&mut m).vars), ["b=foo", "c(unset)"]);
        let vs = vs_of(&mut m);
        assert_eq!(
            vs.vars[vs.selected].name, "b",
            "cursor stays on the promoted var by name across the resort"
        );
    }

    #[test]
    fn remerge_session_refresh_introducing_a_referencing_session_adds_the_unset_row() {
        let mut sessions = vec![tmpl_session("B-x", "{b}-x")];
        let mut m = merged_vars_model(vec![set_var("a", "1")], clone_sessions(&sessions));
        assert_eq!(merged_repr(&vs_of(&mut m).vars), ["a=1", "b(unset)"]);
        vs_of(&mut m).selected = 1; // cursor on b

        // A refresh brings in a session referencing a new var {c}.
        sessions.push(tmpl_session("C-x", "{c}-x"));
        remerge_preserving_cursor(vs_of(&mut m), &sessions);
        assert_eq!(
            merged_repr(&vs_of(&mut m).vars),
            ["a=1", "b(unset)", "c(unset)"]
        );
        let vs = vs_of(&mut m);
        assert_eq!(vs.vars[vs.selected].name, "b", "cursor held on b by name");
    }

    #[test]
    fn remerge_refresh_dropping_the_only_referencing_session_removes_its_unset_row() {
        let sessions = vec![tmpl_session("B-x", "{b}-x"), tmpl_session("C-x", "{c}-x")];
        let mut m = merged_vars_model(vec![], sessions);
        assert_eq!(merged_repr(&vs_of(&mut m).vars), ["b(unset)", "c(unset)"]);
        vs_of(&mut m).selected = 0; // cursor on b

        // c's session goes away; only b remains referenced.
        let sessions = vec![tmpl_session("B-x", "{b}-x")];
        remerge_preserving_cursor(vs_of(&mut m), &sessions);
        assert_eq!(merged_repr(&vs_of(&mut m).vars), ["b(unset)"]);
        let vs = vs_of(&mut m);
        assert_eq!(vs.vars[vs.selected].name, "b", "cursor still on b");
    }

    #[test]
    fn remerge_cursor_whose_variable_vanished_clamps_to_last_row() {
        let sessions = vec![tmpl_session("B-x", "{b}-x"), tmpl_session("C-x", "{c}-x")];
        let mut m = merged_vars_model(vec![], sessions); // [b(unset), c(unset)]
        vs_of(&mut m).selected = 1; // cursor on c

        // c is no longer referenced AND not set: it disappears entirely.
        let sessions = vec![tmpl_session("B-x", "{b}-x")];
        remerge_preserving_cursor(vs_of(&mut m), &sessions);
        assert_eq!(merged_repr(&vs_of(&mut m).vars), ["b(unset)"]);
        assert_eq!(vs_of(&mut m).selected, 0, "cursor clamped into bounds");
    }

    #[test]
    fn remerge_on_an_empty_list_does_not_panic_or_conjure_a_row() {
        // An empty vars list (no set vars, nothing referenced) stays empty
        // across a re-merge — reading the cursor row must not panic or
        // conjure a row. Mirrors shperl's autovivify guard.
        let sessions = vec![tmpl_session("plain", "plain")];
        let mut m = merged_vars_model(vec![], clone_sessions(&sessions));
        assert!(vs_of(&mut m).vars.is_empty(), "list starts empty");
        remerge_preserving_cursor(vs_of(&mut m), &sessions);
        assert!(
            vs_of(&mut m).vars.is_empty(),
            "still empty — no phantom row"
        );
        assert_eq!(vs_of(&mut m).selected, 0, "cursor pinned at 0");
    }

    #[test]
    fn resolution_map_drops_unset_rows() {
        let list = vec![
            set_var("editor", "vim"),
            Var {
                name: "workspace".into(),
                value: String::new(),
                unset: true,
            },
        ];
        let map = resolution_map(&list);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("editor"), Some(&"vim"));
        assert_eq!(
            map.get("workspace"),
            None,
            "unset row contributes no key (no synthetic workspace=\"\")"
        );
    }
}
