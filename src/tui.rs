use std::io::{self, Write};

use crate::session::Session;

pub struct Model {
    pub sessions: Vec<Session>,
    pub selected: usize,
}

impl Model {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self { sessions, selected: 0 }
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
    Other,
}

#[derive(Debug, PartialEq)]
pub enum LoopAction {
    Attach(String),
    Quit,
}

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
            ParserState::Normal => match byte {
                b'\r' | b'\n' => Some(Key::Enter),
                b'q' | 0x03 => Some(Key::Quit),
                0x1b => {
                    self.state = ParserState::Esc;
                    None
                }
                _ => Some(Key::Other),
            },
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

pub fn process_input(
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
            Some(Key::Quit) => return Some(LoopAction::Quit),
            _ => {}
        }
    }
    None
}

pub fn render(
    model: &Model,
    _width: u16,
    _height: u16,
    out: &mut impl Write,
) -> io::Result<()> {
    out.write_all(b"\x1b[2J\x1b[H")?;

    write!(out, "shpool sessions ({} total)\r\n\r\n", model.sessions.len())?;

    if model.sessions.is_empty() {
        out.write_all(b"  (no sessions \xe2\x80\x94 press q to quit)\r\n")?;
    } else {
        for (i, s) in model.sessions.iter().enumerate() {
            if i == model.selected {
                out.write_all(b"\x1b[7m")?;
                write!(out, "> {}  [{}]", s.name, s.status.as_str())?;
                out.write_all(b"\x1b[0m\r\n")?;
            } else {
                write!(out, "  {}  [{}]\r\n", s.name, s.status.as_str())?;
            }
        }
    }

    out.write_all(b"\r\nup/down: navigate   enter: attach   q: quit\r\n")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStatus;

    fn mk(name: &str) -> Session {
        Session { name: name.to_string(), status: SessionStatus::Disconnected }
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

    #[test]
    fn process_input_navigate_and_attach() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        let mut p = InputParser::new();
        // Down, Down, Enter → attach "c"
        let input = [0x1b, b'[', b'B', 0x1b, b'[', b'B', b'\r'];
        assert_eq!(process_input(&input, &mut m, &mut p), Some(LoopAction::Attach("c".into())));
    }

    #[test]
    fn process_input_up_wraps_and_attach() {
        let mut m = Model::new(vec![mk("x"), mk("y"), mk("z")]);
        let mut p = InputParser::new();
        // Up wraps to "z", Enter
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
}
