use super::keymap::{normal_action, Key, NormalAction};
use super::model::{Mode, Model};
use super::parser::InputParser;

#[derive(Debug, PartialEq)]
pub enum LoopAction {
    Attach(String),
    /// Force-attach (`shpool attach -f <name>`) — bumps an existing
    /// terminal off the session. Only reached via the ConfirmForce
    /// prompt, which is itself only entered after the attach
    /// pre-flight detects the session is attached elsewhere.
    AttachForce(String),
    /// Create (via `shpool attach <new-name>`) and immediately attach.
    /// Distinct from Attach so the main loop can skip the
    /// session-must-exist pre-flight check.
    Create(String),
    Kill(String),
    Quit,
}

// -- Input processing --

pub fn process_input(
    buf: &[u8],
    model: &mut Model,
    parser: &mut InputParser,
) -> Option<LoopAction> {
    // Any keypress dismisses a pending error — the user has now seen it.
    model.error = None;
    let mut keys = Vec::with_capacity(buf.len());
    parser.feed(buf, &mut keys);
    match model.mode {
        Mode::Normal => process_normal(&keys, model),
        Mode::CreateInput(_) => process_create_input(&keys, model),
        Mode::ConfirmKill(_) => process_confirm_kill(&keys, model),
        Mode::ConfirmForce(_) => process_confirm_force(&keys, model),
    }
}

fn process_normal(keys: &[Key], model: &mut Model) -> Option<LoopAction> {
    for &k in keys {
        let Some(action) = normal_action(k) else { continue };
        match action {
            NormalAction::SelectPrev => model.select_prev(),
            NormalAction::SelectNext => model.select_next(),
            NormalAction::AttachSelected => {
                if let Some(name) = model.selected_name() {
                    return Some(LoopAction::Attach(name.to_string()));
                }
            }
            NormalAction::NewSession => {
                model.mode = Mode::CreateInput(String::new());
                return None;
            }
            NormalAction::KillSelected => {
                if let Some(name) = model.selected_name() {
                    model.mode = Mode::ConfirmKill(name.to_string());
                }
                return None;
            }
            NormalAction::Quit => return Some(LoopAction::Quit),
        }
    }
    None
}

fn process_create_input(keys: &[Key], model: &mut Model) -> Option<LoopAction> {
    for &k in keys {
        match k {
            // Esc and Ctrl-C both cancel. Ctrl-C is a reflexive cancel
            // in most interactive tools; making it equivalent here
            // means the user never gets stuck in the modal mid-typing.
            Key::Esc | Key::Ctrl(0x03) => {
                model.mode = Mode::Normal;
                return None;
            }
            Key::Enter => {
                if let Mode::CreateInput(ref name) = model.mode {
                    if !name.is_empty() {
                        let name = name.clone();
                        model.mode = Mode::Normal;
                        return Some(LoopAction::Create(name));
                    }
                }
            }
            Key::Backspace => {
                if let Mode::CreateInput(ref mut name) = model.mode {
                    name.pop();
                }
            }
            // Printable non-space ASCII (shpool rejects whitespace in names).
            // The decoder already rejected non-ASCII bytes (→ Key::Other).
            Key::Char(b) if b != b' ' => {
                if let Mode::CreateInput(ref mut name) = model.mode {
                    name.push(b as char);
                }
            }
            _ => {}
        }
    }
    None
}

fn process_confirm_kill(keys: &[Key], model: &mut Model) -> Option<LoopAction> {
    for &k in keys {
        match k {
            Key::Char(b'y') | Key::Char(b'Y') => {
                if let Mode::ConfirmKill(ref name) = model.mode {
                    let name = name.clone();
                    model.mode = Mode::Normal;
                    return Some(LoopAction::Kill(name));
                }
            }
            _ => {
                model.mode = Mode::Normal;
                return None;
            }
        }
    }
    None
}

fn process_confirm_force(keys: &[Key], model: &mut Model) -> Option<LoopAction> {
    for &k in keys {
        match k {
            Key::Char(b'y') | Key::Char(b'Y') => {
                if let Mode::ConfirmForce(ref name) = model.mode {
                    let name = name.clone();
                    model.mode = Mode::Normal;
                    return Some(LoopAction::AttachForce(name));
                }
            }
            _ => {
                model.mode = Mode::Normal;
                return None;
            }
        }
    }
    None
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

    #[test]
    fn process_input_navigate_and_attach() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        let mut p = InputParser::new();
        let input = [0x1b, b'[', b'B', 0x1b, b'[', b'B', b'\r'];
        assert_eq!(process_input(&input, &mut m, &mut p), Some(LoopAction::Attach("c".into())));
    }

    #[test]
    fn process_input_up_wraps_and_attach() {
        let mut m = Model::new(vec![mk("x"), mk("y"), mk("z")]);
        let mut p = InputParser::new();
        let input = [0x1b, b'[', b'A', b'\r'];
        assert_eq!(process_input(&input, &mut m, &mut p), Some(LoopAction::Attach("z".into())));
    }

    #[test]
    fn process_input_quit() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"q", &mut m, &mut p), Some(LoopAction::Quit));
    }

    #[test]
    fn process_input_ctrl_c() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(&[0x03], &mut m, &mut p), Some(LoopAction::Quit));
    }

    #[test]
    fn process_input_enter_empty_list() {
        let mut m = Model::new(vec![]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"\r", &mut m, &mut p), None);
    }

    #[test]
    fn process_input_clears_error() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        m.set_error("session 'a' is gone");
        assert!(m.error.is_some());
        process_input(b"j", &mut m, &mut p);
        assert!(m.error.is_none());
    }

    #[test]
    fn process_input_ignores_other_keys() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"xyz", &mut m, &mut p), None);
    }

    #[test]
    fn process_input_create_flow() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::CreateInput(String::new()));
        assert_eq!(
            process_input(b"foo\r", &mut m, &mut p),
            Some(LoopAction::Create("foo".into()))
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_create_cancel() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(process_input(b"bar\x1b", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_create_backspace() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(
            process_input(b"ab\x7fc\r", &mut m, &mut p),
            Some(LoopAction::Create("ac".into()))
        );
    }

    #[test]
    fn process_input_create_reject_empty() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(process_input(b"\r", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::CreateInput(String::new()));
    }

    #[test]
    fn process_input_create_rejects_spaces() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(
            process_input(b"a b\r", &mut m, &mut p),
            Some(LoopAction::Create("ab".into()))
        );
    }

    #[test]
    fn process_input_kill_confirm() {
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"d", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::ConfirmKill("a".into()));
        assert_eq!(process_input(b"y", &mut m, &mut p), Some(LoopAction::Kill("a".into())));
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_kill_cancel() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"d", &mut m, &mut p), None);
        assert_eq!(process_input(b"x", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_kill_empty_list() {
        let mut m = Model::new(vec![]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"d", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_force_confirm() {
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmForce("a".into());
        let mut p = InputParser::new();
        assert_eq!(
            process_input(b"y", &mut m, &mut p),
            Some(LoopAction::AttachForce("a".into())),
        );
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_force_cancel() {
        let mut m = Model::new(vec![mk("a")]);
        m.mode = Mode::ConfirmForce("a".into());
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_vim_navigate_and_attach() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"jj\r", &mut m, &mut p), Some(LoopAction::Attach("c".into())));
    }
}
