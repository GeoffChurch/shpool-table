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
    /// Transient error message displayed in the bottom bar until the
    /// next keypress. Set by failed shell-outs and pre-flight checks.
    pub error: Option<String>,
    /// Escape-sequence parser state used while in CreateInput mode so
    /// that CSI sequences (arrow keys, focus events, bracketed-paste
    /// markers, etc.) are silently consumed instead of being read as
    /// a bare ESC cancel. Persists across reads to handle sequences
    /// split across kernel read boundaries.
    create_esc: CreateEscState,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum CreateEscState {
    #[default]
    Normal,
    /// Saw `\x1b` — next byte decides whether it's a sequence (`[`)
    /// or a 2-byte ESC form.
    Esc,
    /// Saw `\x1b [` — consuming CSI param/intermediate bytes until a
    /// final byte in the 0x40..=0x7e range terminates the sequence.
    EscBracket,
}

impl Model {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            selected: 0,
            mode: Mode::Normal,
            error: None,
            create_esc: CreateEscState::Normal,
        }
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
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

    /// Replace the session list, preserving the selection by name when
    /// possible and otherwise clamping the previous index into the new
    /// list. Used by the event loop to absorb external changes.
    pub fn refresh(&mut self, new_sessions: Vec<Session>) {
        let prev_name = self.selected_name().map(str::to_string);
        let prev_idx = self.selected;
        self.sessions = new_sessions;
        self.selected = prev_name
            .and_then(|name| self.sessions.iter().position(|s| s.name == name))
            .unwrap_or_else(|| prev_idx.min(self.sessions.len().saturating_sub(1)));
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
    /// Create (via `shpool attach <new-name>`) and immediately attach.
    /// Distinct from Attach so the main loop can skip the
    /// session-must-exist pre-flight check.
    Create(String),
    Kill(String),
    Quit,
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
        label: "↑↓/kj",
        description: "navigate",
        mappings: &[
            (Trigger::EscBracket(b'A'), Key::Up),
            (Trigger::EscBracket(b'B'), Key::Down),
            (Trigger::Byte(b'k'), Key::Up),
            (Trigger::Byte(b'j'), Key::Down),
        ],
    },
    Binding {
        label: "enter",
        description: "attach",
        mappings: &[(Trigger::Byte(b'\r'), Key::Enter), (Trigger::Byte(b'\n'), Key::Enter)],
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
                if byte == 0x1b {
                    self.state = ParserState::Esc;
                    return None;
                }
                // Fold uppercase ASCII to lowercase so bindings match
                // `N`/`J`/etc. as well as `n`/`j`. Tradeoff: uppercase
                // variants can no longer carry a distinct meaning.
                let normalized = byte.to_ascii_lowercase();
                Some(
                    lookup(|t| matches!(t, Trigger::Byte(b) if b == normalized))
                        .unwrap_or(Key::Other),
                )
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
                Some(
                    lookup(|t| matches!(t, Trigger::EscBracket(b) if b == byte))
                        .unwrap_or(Key::Other),
                )
            }
        }
    }
}

fn lookup(matches: impl Fn(Trigger) -> bool) -> Option<Key> {
    for binding in NORMAL_BINDINGS {
        for (trig, key) in binding.mappings {
            if matches(*trig) {
                return Some(*key);
            }
        }
    }
    None
}

// -- Input processing --

pub fn process_input(
    buf: &[u8],
    model: &mut Model,
    parser: &mut InputParser,
) -> Option<LoopAction> {
    // Any keypress dismisses a pending error — the user has now seen it.
    model.error = None;
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
                model.create_esc = CreateEscState::Normal;
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
        match model.create_esc {
            CreateEscState::Normal => match b {
                0x1b => {
                    // Possibly a bare ESC (cancel) or possibly the
                    // start of a CSI sequence. Wait for the next byte
                    // or the end of the buffer to decide.
                    model.create_esc = CreateEscState::Esc;
                }
                0x03 => {
                    model.mode = Mode::Normal;
                    return None;
                }
                0x0d | 0x0a => {
                    if let Mode::CreateInput(ref name) = model.mode {
                        if !name.is_empty() {
                            let name = name.clone();
                            model.mode = Mode::Normal;
                            return Some(LoopAction::Create(name));
                        }
                    }
                }
                0x7f | 0x08 => {
                    if let Mode::CreateInput(ref mut name) = model.mode {
                        name.pop();
                    }
                }
                // Printable non-space ASCII (shpool rejects whitespace in names).
                0x21..=0x7e => {
                    if let Mode::CreateInput(ref mut name) = model.mode {
                        name.push(b as char);
                    }
                }
                _ => {}
            },
            CreateEscState::Esc => {
                // Second byte of an ESC-prefixed sequence. `[` starts
                // a CSI and we consume further bytes; anything else is
                // a 2-byte sequence we silently drop.
                model.create_esc = if b == b'[' {
                    CreateEscState::EscBracket
                } else {
                    CreateEscState::Normal
                };
            }
            CreateEscState::EscBracket => {
                // CSI: keep consuming param (0x30..=0x3f) and
                // intermediate (0x20..=0x2f) bytes until a final byte
                // (0x40..=0x7e) terminates the sequence.
                if (0x40..=0x7e).contains(&b) {
                    model.create_esc = CreateEscState::Normal;
                }
            }
        }
    }
    // If the buffer ended while we were still in Esc state, the user
    // most likely pressed the Escape key on its own — sequences
    // normally arrive as a single read, so an unterminated ESC at a
    // read boundary is a reliable bare-Escape signal. Treat as cancel.
    if matches!(model.create_esc, CreateEscState::Esc) {
        model.create_esc = CreateEscState::Normal;
        model.mode = Mode::Normal;
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

// SGR codes for the bar chrome. The chrome reads as a phosphor-amber
// CRT bezel: dark bar background with two-tone amber text on top.
const SGR_RESET: &str = "\x1b[0m";
const SGR_BAR_BG: &str = "\x1b[48;5;236m"; // dark gray bar background (#303030)
const SGR_BAR_END: &str = "\x1b[49m"; // restore default bg, keep fg
const SGR_AMBER: &str = "\x1b[1;38;2;255;200;87m"; // bold warm amber (#ffc857)
const SGR_AMBER_DIM: &str = "\x1b[38;2;200;156;82m"; // dim warm amber (#c89c52)
const SGR_ERROR: &str = "\x1b[1;38;2;255;120;100m"; // bold warm red (#ff7864)
// Reset only fg + bold inside a bar — leaves the bar background intact.
const SGR_BAR_FG_RESET: &str = "\x1b[22;39m";
const SGR_SELECTED: &str = "\x1b[7m"; // reverse video

/// A label destined for embedding in a chrome bar. Tracks the styled
/// byte stream and the visible column count separately, so the bar's
/// trailing space fill can be sized correctly without parsing ANSI.
#[derive(Default)]
struct Label {
    styled: String,
    visible: usize,
}

impl Label {
    fn push_plain(&mut self, s: &str) {
        self.styled.push_str(SGR_AMBER_DIM);
        self.styled.push_str(s);
        self.styled.push_str(SGR_BAR_FG_RESET);
        self.visible += s.chars().count();
    }

    fn push_key(&mut self, s: &str) {
        self.styled.push_str(SGR_AMBER);
        self.styled.push_str(s);
        self.styled.push_str(SGR_BAR_FG_RESET);
        self.visible += s.chars().count();
    }

    fn push_error(&mut self, s: &str) {
        self.styled.push_str(SGR_ERROR);
        self.styled.push_str(s);
        self.styled.push_str(SGR_BAR_FG_RESET);
        self.visible += s.chars().count();
    }
}

fn title_label(model: &Model) -> Label {
    let mut l = Label::default();
    let n = model.sessions.len();
    let title = format!("shpool ({n} session{})", if n == 1 { "" } else { "s" });
    l.push_key(&title);
    l
}

fn normal_bindings_label() -> Label {
    let mut l = Label::default();
    for (i, b) in NORMAL_BINDINGS.iter().enumerate() {
        if i > 0 {
            l.push_plain(" · ");
        }
        l.push_key(b.label);
        l.push_plain(" ");
        l.push_plain(b.description);
    }
    l
}

fn create_input_label(input: &str) -> Label {
    let mut l = Label::default();
    l.push_plain("new session: ");
    l.push_key(input);
    l.push_plain("_   (");
    push_hints(&mut l, CREATE_HINTS);
    l.push_plain(")");
    l
}

fn error_label(msg: &str) -> Label {
    let mut l = Label::default();
    l.push_error("! ");
    l.push_error(msg);
    l
}

fn confirm_kill_label(name: &str) -> Label {
    let mut l = Label::default();
    l.push_plain("kill ");
    l.push_key(&format!("\"{name}\""));
    l.push_plain("?   (");
    push_hints(&mut l, CONFIRM_KILL_HINTS);
    l.push_plain(")");
    l
}

fn push_hints(l: &mut Label, hints: &[(&'static str, &'static str)]) {
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            l.push_plain(", ");
        }
        l.push_key(key);
        l.push_plain(": ");
        l.push_plain(desc);
    }
}

#[derive(Clone, Copy)]
enum BarAlign {
    Left,
    Center,
}

/// Render a chrome bar with an embedded label, filling `width` columns
/// with the bar background. Left-aligned bars get a 2-col leading
/// pad; centered bars split the remaining space evenly.
fn render_bar(
    out: &mut impl Write,
    width: usize,
    label: &Label,
    align: BarAlign,
) -> io::Result<()> {
    let (lead, trail) = match align {
        BarAlign::Left => {
            let lead = 2;
            let trail = width.saturating_sub(lead + label.visible);
            (lead, trail)
        }
        BarAlign::Center => {
            let slack = width.saturating_sub(label.visible);
            let lead = slack / 2;
            let trail = slack - lead;
            (lead, trail)
        }
    };
    out.write_all(SGR_BAR_BG.as_bytes())?;
    for _ in 0..lead {
        out.write_all(b" ")?;
    }
    out.write_all(label.styled.as_bytes())?;
    for _ in 0..trail {
        out.write_all(b" ")?;
    }
    out.write_all(SGR_BAR_END.as_bytes())?;
    out.write_all(SGR_RESET.as_bytes())?;
    out.write_all(b"\r\n")?;
    Ok(())
}

/// Clip a string to at most `max_chars` characters. Used on row
/// content so rows longer than the terminal width are truncated
/// rather than wrapped or clobbering the rightmost column.
fn clip(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

// Top bar + bottom bar = 2 lines of overhead.
const CHROME_LINES: usize = 2;

pub fn render(
    model: &Model,
    width: u16,
    height: u16,
    out: &mut impl Write,
) -> io::Result<()> {
    let w = width as usize;

    out.write_all(b"\x1b[2J\x1b[H")?;

    render_bar(out, w, &title_label(model), BarAlign::Center)?;

    if model.sessions.is_empty() {
        out.write_all(b"  (no sessions)\r\n")?;
    } else {
        let max_visible = (height as usize).saturating_sub(CHROME_LINES);
        let (start, end) = viewport(model.sessions.len(), model.selected, max_visible);

        for (i, s) in model.sessions[start..end].iter().enumerate() {
            let abs_i = start + i;
            let text = clip(
                &if abs_i == model.selected {
                    format!("> {}", s.name)
                } else {
                    format!("  {}", s.name)
                },
                w,
            );
            if abs_i == model.selected {
                write!(out, "{SGR_SELECTED}{text:<w$}{SGR_RESET}\r\n")?;
            } else {
                write!(out, "{text:<w$}\r\n")?;
            }
        }
    }

    let bottom = if let Some(err) = &model.error {
        error_label(err)
    } else {
        match &model.mode {
            Mode::Normal => normal_bindings_label(),
            Mode::CreateInput(input) => create_input_label(input),
            Mode::ConfirmKill(name) => confirm_kill_label(name),
        }
    };
    render_bar(out, w, &bottom, BarAlign::Left)?;

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
    fn mk(name: &str) -> Session {
        use crate::session::SessionStatus;
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
        assert_eq!(p.feed(b'd'), Some(Key::KillSession));
    }

    #[test]
    fn parser_vim_navigation() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b'j'), Some(Key::Down));
        assert_eq!(p.feed(b'k'), Some(Key::Up));
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

    // -- process_input: create mode --

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

    // -- process_input: kill mode --

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
    fn process_input_vim_navigate_and_attach() {
        let mut m = Model::new(vec![mk("a"), mk("b"), mk("c")]);
        let mut p = InputParser::new();
        assert_eq!(process_input(b"jj\r", &mut m, &mut p), Some(LoopAction::Attach("c".into())));
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
