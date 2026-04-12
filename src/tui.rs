use std::io::{self, Write};

use crate::session::Session;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    CreateInput(String),
    ConfirmKill(String),
}

pub struct Model {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub mode: Mode,
}

impl Model {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self { sessions, selected: 0, mode: Mode::Normal }
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
}

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

#[derive(Debug, PartialEq)]
pub enum LoopAction {
    Attach(String),
    Kill(String),
    Quit,
}

// -- Key binding tables --
//
// These tables are the single source of truth for both the parser's
// key dispatch and the footer text rendered in each mode. Bindings
// come in two flavors:
//
//   Trigger::Byte(byte, key) — a single byte that the parser matches
//       via table lookup in its Normal state. Adding an entry here
//       automatically wires up both the parser and the footer.
//
//   Trigger::BuiltIn — a multi-byte escape sequence (e.g., arrow keys)
//       or a key with multiple trigger bytes (e.g., Enter = CR or LF).
//       These are matched by hardcoded logic in the parser's state
//       machine, but listed here so the footer is generated from the
//       same source.

pub enum Trigger {
    Byte(u8, Key),
    BuiltIn,
}

pub struct Binding {
    pub trigger: Trigger,
    pub label: &'static str,
    pub description: &'static str,
}

pub const NORMAL_BINDINGS: &[Binding] = &[
    Binding { trigger: Trigger::BuiltIn, label: "up/down", description: "navigate" },
    Binding { trigger: Trigger::BuiltIn, label: "enter", description: "attach" },
    Binding { trigger: Trigger::Byte(b'n', Key::NewSession), label: "n", description: "new" },
    Binding { trigger: Trigger::Byte(b'k', Key::KillSession), label: "k", description: "kill" },
    Binding { trigger: Trigger::Byte(b'q', Key::Quit), label: "q", description: "quit" },
];

/// Footer hints for create mode. The actual byte handling lives in
/// process_create_input; these just keep the display in sync.
pub const CREATE_HINTS: &[(&str, &str)] = &[("enter", "create"), ("esc", "cancel")];
pub const CONFIRM_KILL_HINTS: &[(&str, &str)] = &[("y", "confirm"), ("n", "cancel")];

// -- Input parser --

#[derive(Default)]
pub struct InputParser {
    state: ParserState,
}

#[derive(Default, Clone, Copy)]
enum ParserState {
    #[default]
    Normal,
    Esc,
    EscBracket,
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        match self.state {
            ParserState::Normal => {
                // Table-driven dispatch for single-byte bindings.
                for binding in NORMAL_BINDINGS {
                    if let Trigger::Byte(b, key) = binding.trigger {
                        if byte == b {
                            return Some(key);
                        }
                    }
                }
                // Built-in keys: multi-byte sequences and control chars.
                match byte {
                    b'\r' | b'\n' => Some(Key::Enter),
                    0x03 => Some(Key::Quit),
                    0x1b => {
                        self.state = ParserState::Esc;
                        None
                    }
                    _ => Some(Key::Other),
                }
            }
            ParserState::Esc => match byte {
                b'[' => {
                    self.state = ParserState::EscBracket;
                    None
                }
                _ => {
                    self.state = ParserState::Normal;
                    Some(Key::Other)
                }
            },
            ParserState::EscBracket => {
                self.state = ParserState::Normal;
                match byte {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    _ => Some(Key::Other),
                }
            }
        }
    }
}

// -- Input processing --

pub fn process_input(
    buf: &[u8],
    model: &mut Model,
    parser: &mut InputParser,
) -> Option<LoopAction> {
    match model.mode {
        Mode::Normal => process_normal(buf, model, parser),
        Mode::CreateInput(_) => process_create_input(buf, model),
        Mode::ConfirmKill(_) => process_confirm_kill(buf, model),
    }
}

fn process_normal(
    buf: &[u8],
    model: &mut Model,
    parser: &mut InputParser,
) -> Option<LoopAction> {
    for &b in buf {
        match parser.feed(b) {
            Some(Key::Up) => model.select_prev(),
            Some(Key::Down) => model.select_next(),
            Some(Key::Enter) => {
                if let Some(name) = model.selected_name() {
                    return Some(LoopAction::Attach(name.to_string()));
                }
            }
            Some(Key::NewSession) => {
                model.mode = Mode::CreateInput(String::new());
                return None;
            }
            Some(Key::KillSession) => {
                if let Some(name) = model.selected_name() {
                    model.mode = Mode::ConfirmKill(name.to_string());
                }
                return None;
            }
            Some(Key::Quit) => return Some(LoopAction::Quit),
            Some(Key::Other) | None => {}
        }
    }
    None
}

fn process_create_input(buf: &[u8], model: &mut Model) -> Option<LoopAction> {
    for &b in buf {
        match b {
            0x0d | 0x0a => {
                if let Mode::CreateInput(ref name) = model.mode {
                    if !name.is_empty() {
                        let name = name.clone();
                        model.mode = Mode::Normal;
                        return Some(LoopAction::Attach(name));
                    }
                }
            }
            0x1b | 0x03 => {
                model.mode = Mode::Normal;
                return None;
            }
            0x7f | 0x08 => {
                if let Mode::CreateInput(ref mut name) = model.mode {
                    name.pop();
                }
            }
            // Printable non-space ASCII (shpool rejects whitespace in names).
            b if (0x21..=0x7e).contains(&b) => {
                if let Mode::CreateInput(ref mut name) = model.mode {
                    name.push(b as char);
                }
            }
            _ => {}
        }
    }
    None
}

fn process_confirm_kill(buf: &[u8], model: &mut Model) -> Option<LoopAction> {
    for &b in buf {
        if b == b'y' || b == b'Y' {
            if let Mode::ConfirmKill(ref name) = model.mode {
                let name = name.clone();
                model.mode = Mode::Normal;
                return Some(LoopAction::Kill(name));
            }
        } else {
            model.mode = Mode::Normal;
            return None;
        }
    }
    None
}

// -- Rendering --

fn render_hints(
    out: &mut impl Write,
    hints: impl IntoIterator<Item = (&'static str, &'static str)>,
    separator: &str,
) -> io::Result<()> {
    for (i, (label, desc)) in hints.into_iter().enumerate() {
        if i > 0 {
            out.write_all(separator.as_bytes())?;
        }
        write!(out, "{label}: {desc}")?;
    }
    Ok(())
}

// Header (2 lines) + blank before footer + footer (1 line) = 4 lines of overhead.
const CHROME_LINES: usize = 4;

pub fn render(
    model: &Model,
    width: u16,
    height: u16,
    out: &mut impl Write,
) -> io::Result<()> {
    let w = width as usize;

    out.write_all(b"\x1b[2J\x1b[H")?;

    write!(out, "shpool sessions ({} total)\r\n\r\n", model.sessions.len())?;

    if model.sessions.is_empty() {
        out.write_all(b"  (no sessions)\r\n")?;
    } else {
        let max_visible = (height as usize).saturating_sub(CHROME_LINES);
        let (start, end) = viewport(model.sessions.len(), model.selected, max_visible);

        for (i, s) in model.sessions[start..end].iter().enumerate() {
            let abs_i = start + i;
            let text = if abs_i == model.selected {
                format!("> {}  [{}]", s.name, s.status.as_str())
            } else {
                format!("  {}  [{}]", s.name, s.status.as_str())
            };
            if abs_i == model.selected {
                write!(out, "\x1b[7m{text:<w$}\x1b[0m\r\n")?;
            } else {
                write!(out, "{text:<w$}\r\n")?;
            }
        }
    }

    match &model.mode {
        Mode::Normal => {
            out.write_all(b"\r\n")?;
            render_hints(out, NORMAL_BINDINGS.iter().map(|b| (b.label, b.description)), "   ")?;
            out.write_all(b"\r\n")?;
        }
        Mode::CreateInput(input) => {
            write!(out, "\r\nnew session: {input}_   (")?;
            render_hints(out, CREATE_HINTS.iter().copied(), ", ")?;
            out.write_all(b")\r\n")?;
        }
        Mode::ConfirmKill(name) => {
            write!(out, "\r\nkill \"{name}\"? (")?;
            render_hints(out, CONFIRM_KILL_HINTS.iter().copied(), ", ")?;
            out.write_all(b")\r\n")?;
        }
    }

    Ok(())
}

/// Compute the visible window [start, end) that keeps `selected` on screen.
fn viewport(total: usize, selected: usize, max_visible: usize) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let half = max_visible / 2;
    let ideal_start = selected.saturating_sub(half);
    let start = ideal_start.min(total.saturating_sub(max_visible));
    (start, start + max_visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStatus;

    fn mk(name: &str) -> Session {
        Session { name: name.to_string(), status: SessionStatus::Disconnected }
    }

    // -- Model tests --

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

    // -- Parser tests --

    #[test]
    fn parser_up_arrow() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), None);
        assert_eq!(p.feed(b'['), None);
        assert_eq!(p.feed(b'A'), Some(Key::Up));
    }

    #[test]
    fn parser_down_arrow() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), None);
        assert_eq!(p.feed(b'['), None);
        assert_eq!(p.feed(b'B'), Some(Key::Down));
    }

    #[test]
    fn parser_enter_crlf() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b'\r'), Some(Key::Enter));
        assert_eq!(p.feed(b'\n'), Some(Key::Enter));
    }

    #[test]
    fn parser_quit() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b'q'), Some(Key::Quit));
        assert_eq!(p.feed(0x03), Some(Key::Quit));
    }

    #[test]
    fn parser_new_session() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b'n'), Some(Key::NewSession));
    }

    #[test]
    fn parser_kill_session() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b'k'), Some(Key::KillSession));
    }

    #[test]
    fn parser_unknown_esc_sequence() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), None);
        assert_eq!(p.feed(b'['), None);
        assert_eq!(p.feed(b'Z'), Some(Key::Other));
    }

    #[test]
    fn parser_stream() {
        let mut p = InputParser::new();
        let mut out = vec![];
        for &b in &[0x1b, b'[', b'B', 0x1b, b'[', b'B', b'\r'] {
            if let Some(k) = p.feed(b) {
                out.push(k);
            }
        }
        assert_eq!(out, vec![Key::Down, Key::Down, Key::Enter]);
    }

    // -- process_input: normal mode --

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
    fn process_input_ignores_other_keys() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"xyz", &mut m, &mut p), None);
    }

    // -- process_input: create mode --

    #[test]
    fn process_input_create_flow() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"n", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::CreateInput(String::new()));
        assert_eq!(
            process_input(b"foo\r", &mut m, &mut p),
            Some(LoopAction::Attach("foo".into()))
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
            Some(LoopAction::Attach("ac".into()))
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
            Some(LoopAction::Attach("ab".into()))
        );
    }

    // -- process_input: kill mode --

    #[test]
    fn process_input_kill_confirm() {
        let mut m = Model::new(vec![mk("a"), mk("b")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"k", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::ConfirmKill("a".into()));
        assert_eq!(process_input(b"y", &mut m, &mut p), Some(LoopAction::Kill("a".into())));
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_kill_cancel() {
        let mut m = Model::new(vec![mk("a")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"k", &mut m, &mut p), None);
        assert_eq!(process_input(b"x", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    #[test]
    fn process_input_kill_empty_list() {
        let mut m = Model::new(vec![]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"k", &mut m, &mut p), None);
        assert_eq!(m.mode, Mode::Normal);
    }

    // -- Viewport --

    #[test]
    fn viewport_fits() {
        assert_eq!(viewport(3, 0, 10), (0, 3));
    }

    #[test]
    fn viewport_scrolls_down() {
        assert_eq!(viewport(20, 15, 5), (13, 18));
    }

    #[test]
    fn viewport_clamps_to_end() {
        assert_eq!(viewport(20, 19, 5), (15, 20));
    }

    #[test]
    fn viewport_centers_selected() {
        assert_eq!(viewport(20, 10, 5), (8, 13));
    }
}
