// Semantic keystroke — the parser's output vocabulary. Downstream
// mode handlers (Normal, Create, ConfirmKill, ConfirmForce) pattern-
// match on this rather than on raw bytes.
//
// Char(b) carries the raw byte so we can distinguish 'y' from 'Y'
// etc. — case-folding is done explicitly in the binding table where
// wanted, rather than globally at lookup time. That means a future
// case-distinct binding (e.g., vim-style `G` = bottom) is just a data
// change, not a code change.
//
// Ctrl(b) is kept separate from Char so dispatch can match Ctrl-C
// without confusing it with the printable 'c'. Named controls that
// have their own Key variant (Backspace, Tab, Enter) don't appear as
// Ctrl() — the named variant is canonical.

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Key {
    // Arrows.
    Up,
    Down,
    Left,
    Right,
    // Named controls.
    Enter,
    Esc,
    Backspace,
    Tab,
    // Raw bytes.
    Ctrl(u8),
    Char(u8),
    // Anything we don't recognize (unmapped CSI finals, non-ASCII bytes).
    Other,
}

// -- Normal-mode action dispatch --
//
// The set of logical actions bound to keys in Normal mode. We use an
// enum rather than a direct `fn(&mut Model)` in the binding table
// because actions differ in what state they touch: SelectNext is
// stateless but AttachSelected needs model.sessions[model.selected],
// and keeping the dispatch as "lookup action, then match in update.rs"
// lets each action pull exactly what it needs. The compiler's
// exhaustiveness check then catches drift if a new action is added
// without a handler.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalAction {
    SelectPrev,
    SelectNext,
    AttachSelected,
    NewSession,
    KillSelected,
    EnsureDaemon,
    Quit,
}

pub struct Binding {
    pub label: &'static str,
    pub description: &'static str,
    /// Every Key that triggers this action. Listed explicitly — no
    /// case-folding at lookup time — so a new case-distinct binding
    /// is a pure data change.
    pub keys: &'static [Key],
    pub action: NormalAction,
}

pub const NORMAL_BINDINGS: &[Binding] = &[
    Binding {
        label: "j",
        description: "down",
        keys: &[Key::Down, Key::Char(b'j'), Key::Char(b'J')],
        action: NormalAction::SelectNext,
    },
    Binding {
        label: "k",
        description: "up",
        keys: &[Key::Up, Key::Char(b'k'), Key::Char(b'K')],
        action: NormalAction::SelectPrev,
    },
    Binding {
        label: "spc",
        description: "attach",
        keys: &[Key::Char(b' '), Key::Enter],
        action: NormalAction::AttachSelected,
    },
    Binding {
        label: "n",
        description: "new",
        keys: &[Key::Char(b'n'), Key::Char(b'N')],
        action: NormalAction::NewSession,
    },
    Binding {
        label: "d",
        description: "kill",
        keys: &[Key::Char(b'd')],
        action: NormalAction::KillSelected,
    },
    Binding {
        label: "D",
        description: "daemon",
        keys: &[Key::Char(b'D')],
        action: NormalAction::EnsureDaemon,
    },
    Binding {
        label: "q",
        description: "quit",
        // Ctrl-C (0x03) is a global-quit convention. Enumerating it
        // here rather than special-casing in dispatch keeps the
        // single-source-of-truth story intact.
        keys: &[Key::Char(b'q'), Key::Char(b'Q'), Key::Ctrl(0x03)],
        action: NormalAction::Quit,
    },
];

/// Footer hints for create mode. The actual byte handling lives in
/// update.rs's create handler; these just keep the display in sync.
pub const CREATE_HINTS: &[(&str, &str)] = &[("ret", "create"), ("esc", "cancel")];
pub const CONFIRM_KILL_HINTS: &[(&str, &str)] = &[("y", "confirm"), ("n", "cancel")];
pub const CONFIRM_FORCE_HINTS: &[(&str, &str)] = &[("y", "force"), ("n", "cancel")];

/// Look up which NormalAction (if any) a given Key triggers.
/// Linear scan; the table is ~20 entries and this runs at most once
/// per keystroke, so a HashMap would be over-engineering.
pub fn normal_action(key: Key) -> Option<NormalAction> {
    NORMAL_BINDINGS.iter().find(|b| b.keys.contains(&key)).map(|b| b.action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn normal_action_maps_bindings() {
        assert_eq!(normal_action(Key::Up), Some(NormalAction::SelectPrev));
        assert_eq!(normal_action(Key::Down), Some(NormalAction::SelectNext));
        assert_eq!(normal_action(Key::Enter), Some(NormalAction::AttachSelected));
        assert_eq!(normal_action(Key::Char(b' ')), Some(NormalAction::AttachSelected));
        assert_eq!(normal_action(Key::Char(b'q')), Some(NormalAction::Quit));
        assert_eq!(normal_action(Key::Char(b'Q')), Some(NormalAction::Quit));
        assert_eq!(normal_action(Key::Ctrl(0x03)), Some(NormalAction::Quit));
        assert_eq!(normal_action(Key::Char(b'n')), Some(NormalAction::NewSession));
        assert_eq!(normal_action(Key::Char(b'd')), Some(NormalAction::KillSelected));
        assert_eq!(normal_action(Key::Char(b'D')), Some(NormalAction::EnsureDaemon));
        assert_eq!(normal_action(Key::Char(b'j')), Some(NormalAction::SelectNext));
        assert_eq!(normal_action(Key::Char(b'k')), Some(NormalAction::SelectPrev));
        assert_eq!(normal_action(Key::Esc), None);
        assert_eq!(normal_action(Key::Other), None);
    }

    /// Guard against drift: if a new binding accidentally re-uses a
    /// key already claimed by an existing binding, `normal_action`'s
    /// linear scan would silently dispatch to whichever comes first
    /// and the new binding would never fire. Catch that at test time.
    #[test]
    fn no_key_bound_to_multiple_actions() {
        let mut claimed: HashMap<Key, &'static str> = HashMap::new();
        for b in NORMAL_BINDINGS {
            for &k in b.keys {
                if let Some(existing) = claimed.insert(k, b.label) {
                    panic!(
                        "key {:?} is bound by both [{}] and [{}] — \
                         NORMAL_BINDINGS entries must have disjoint keys",
                        k, existing, b.label,
                    );
                }
            }
        }
    }
}
