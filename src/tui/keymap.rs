use super::parser::Token;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Key {
    Up,
    Down,
    Enter,
    Quit,
    NewSession,
    KillSession,
    Other,
}

// -- Key binding tables --
//
// This table is the single source of truth for both the parser's key
// dispatch and the footer text rendered in each mode. Each Binding is
// one footer row (label + description) plus a list of (trigger, key)
// mappings the parser dispatches from. A single binding can fan out
// to multiple keys (e.g., navigation maps ↑/k → Up and ↓/j → Down).

#[derive(Clone, Copy)]
pub enum Trigger {
    /// A single byte matched in the parser's Normal state.
    Byte(u8),
    /// An ESC [ <byte> sequence (e.g., arrow keys).
    EscBracket(u8),
}

pub struct Binding {
    pub label: &'static str,
    pub description: &'static str,
    pub mappings: &'static [(Trigger, Key)],
}

pub const NORMAL_BINDINGS: &[Binding] = &[
    Binding {
        label: "j",
        description: "down",
        mappings: &[
            (Trigger::EscBracket(b'B'), Key::Down),
            (Trigger::Byte(b'j'), Key::Down),
        ],
    },
    Binding {
        label: "k",
        description: "up",
        mappings: &[
            (Trigger::EscBracket(b'A'), Key::Up),
            (Trigger::Byte(b'k'), Key::Up),
        ],
    },
    Binding {
        label: "spc",
        description: "attach",
        mappings: &[
            (Trigger::Byte(b' '), Key::Enter),
            (Trigger::Byte(b'\r'), Key::Enter),
            (Trigger::Byte(b'\n'), Key::Enter),
        ],
    },
    Binding {
        label: "n",
        description: "new",
        mappings: &[(Trigger::Byte(b'n'), Key::NewSession)],
    },
    Binding {
        label: "d",
        description: "kill",
        mappings: &[(Trigger::Byte(b'd'), Key::KillSession)],
    },
    Binding {
        label: "q",
        description: "quit",
        mappings: &[(Trigger::Byte(b'q'), Key::Quit), (Trigger::Byte(0x03), Key::Quit)],
    },
];

/// Footer hints for create mode. The actual byte handling lives in
/// process_create_input; these just keep the display in sync.
pub const CREATE_HINTS: &[(&str, &str)] = &[("ret", "create"), ("esc", "cancel")];
pub const CONFIRM_KILL_HINTS: &[(&str, &str)] = &[("y", "confirm"), ("n", "cancel")];
pub const CONFIRM_FORCE_HINTS: &[(&str, &str)] = &[("y", "force"), ("n", "cancel")];

// Precomputed dispatch tables so per-byte key lookup is O(1). Built
// lazily from NORMAL_BINDINGS on first use.
fn byte_key(b: u8) -> Option<Key> {
    static T: std::sync::OnceLock<[Option<Key>; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| build_table(|t| matches!(t, Trigger::Byte(_)), |t| match t {
        Trigger::Byte(v) => v,
        _ => unreachable!(),
    }))[b as usize]
}

fn csi_key(b: u8) -> Option<Key> {
    static T: std::sync::OnceLock<[Option<Key>; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| build_table(|t| matches!(t, Trigger::EscBracket(_)), |t| match t {
        Trigger::EscBracket(v) => v,
        _ => unreachable!(),
    }))[b as usize]
}

fn build_table(
    matches: impl Fn(&Trigger) -> bool,
    key_of: impl Fn(Trigger) -> u8,
) -> [Option<Key>; 256] {
    let mut arr = [None; 256];
    for binding in NORMAL_BINDINGS {
        for (trig, key) in binding.mappings {
            if matches(trig) {
                arr[key_of(*trig) as usize] = Some(*key);
            }
        }
    }
    arr
}

/// Map a token to a normal-mode Key, for the Normal-mode handler.
/// Tokens that don't map to any binding become Key::Other.
pub fn token_to_key(token: Token) -> Key {
    match token {
        Token::Byte(b) => byte_key(b.to_ascii_lowercase()).unwrap_or(Key::Other),
        Token::Csi(b) => csi_key(b).unwrap_or(Key::Other),
        Token::BareEsc => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_to_key_maps_bindings() {
        assert_eq!(token_to_key(Token::Csi(b'A')), Key::Up);
        assert_eq!(token_to_key(Token::Csi(b'B')), Key::Down);
        assert_eq!(token_to_key(Token::Byte(b'q')), Key::Quit);
        assert_eq!(token_to_key(Token::Byte(b'Q')), Key::Quit); // case fold
        assert_eq!(token_to_key(Token::Byte(b'n')), Key::NewSession);
        assert_eq!(token_to_key(Token::Byte(b'j')), Key::Down);
        assert_eq!(token_to_key(Token::BareEsc), Key::Other);
        assert_eq!(token_to_key(Token::Csi(b'Z')), Key::Other);
    }
}
