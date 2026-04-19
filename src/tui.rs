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
}

impl Model {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self { sessions, selected: 0, mode: Mode::Normal, error: None }
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
    /// list. Sessions are sorted with the most recently active first,
    /// so whichever session you last touched floats to the top.
    pub fn refresh(&mut self, mut new_sessions: Vec<Session>) {
        new_sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active_unix_ms()));
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

// -- Input parser --
//
// One state machine turns a raw byte stream into a token stream:
//   - Byte(b)    — a regular byte (caller decides what to do with it)
//   - Csi(b)     — a terminated CSI sequence (`ESC [ ... <final>`);
//                  the token carries only the final byte, which is all
//                  our bindings care about.
//   - BareEsc    — an unterminated ESC at the buffer boundary. A
//                  terminal emits full CSI sequences as one write, so
//                  a lone ESC at end-of-buffer is reliably a bare
//                  Escape keypress.
// This replaces a pair of near-identical parsers (one in Normal mode
// returning Key, one inline in Create mode). Each mode handler now
// interprets the token stream on its own terms.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Token {
    Byte(u8),
    Csi(u8),
    BareEsc,
}

#[derive(Default, Clone, Copy)]
enum ParserState {
    #[default]
    Normal,
    Esc,
    EscBracket,
}

#[derive(Default)]
pub struct InputParser {
    state: ParserState,
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume `bytes`, pushing tokens onto `out`. Parser state persists
    /// across calls so a CSI sequence split across reads (rare but
    /// possible) still parses correctly.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Token>) {
        for &b in bytes {
            match self.state {
                ParserState::Normal => {
                    if b == 0x1b {
                        self.state = ParserState::Esc;
                    } else {
                        out.push(Token::Byte(b));
                    }
                }
                ParserState::Esc => {
                    if b == b'[' {
                        self.state = ParserState::EscBracket;
                    } else {
                        // ESC followed by a non-bracket byte in the same
                        // buffer: treat as bare Escape plus whatever
                        // followed (so ESC+q still fires Quit).
                        out.push(Token::BareEsc);
                        out.push(Token::Byte(b));
                        self.state = ParserState::Normal;
                    }
                }
                ParserState::EscBracket => {
                    // CSI: skip param (0x30..=0x3f) and intermediate
                    // (0x20..=0x2f) bytes until a final byte terminates.
                    if (0x40..=0x7e).contains(&b) {
                        out.push(Token::Csi(b));
                        self.state = ParserState::Normal;
                    }
                }
            }
        }
        if matches!(self.state, ParserState::Esc) {
            out.push(Token::BareEsc);
            self.state = ParserState::Normal;
        }
    }
}

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
fn token_to_key(token: Token) -> Key {
    match token {
        Token::Byte(b) => byte_key(b.to_ascii_lowercase()).unwrap_or(Key::Other),
        Token::Csi(b) => csi_key(b).unwrap_or(Key::Other),
        Token::BareEsc => Key::Other,
    }
}

// -- Input processing --

pub fn process_input(
    buf: &[u8],
    model: &mut Model,
    parser: &mut InputParser,
) -> Option<LoopAction> {
    // Any keypress dismisses a pending error — the user has now seen it.
    model.error = None;
    let mut tokens = Vec::with_capacity(buf.len());
    parser.feed(buf, &mut tokens);
    match model.mode {
        Mode::Normal => process_normal(&tokens, model),
        Mode::CreateInput(_) => process_create_input(&tokens, model),
        Mode::ConfirmKill(_) => process_confirm_kill(&tokens, model),
    }
}

fn process_normal(tokens: &[Token], model: &mut Model) -> Option<LoopAction> {
    for &t in tokens {
        match token_to_key(t) {
            Key::Up => model.select_prev(),
            Key::Down => model.select_next(),
            Key::Enter => {
                if let Some(name) = model.selected_name() {
                    return Some(LoopAction::Attach(name.to_string()));
                }
            }
            Key::NewSession => {
                model.mode = Mode::CreateInput(String::new());
                return None;
            }
            Key::KillSession => {
                if let Some(name) = model.selected_name() {
                    model.mode = Mode::ConfirmKill(name.to_string());
                }
                return None;
            }
            Key::Quit => return Some(LoopAction::Quit),
            Key::Other => {}
        }
    }
    None
}

fn process_create_input(tokens: &[Token], model: &mut Model) -> Option<LoopAction> {
    for &t in tokens {
        match t {
            Token::BareEsc => {
                model.mode = Mode::Normal;
                return None;
            }
            Token::Csi(_) => {} // arrow keys etc. are ignored in this mode
            Token::Byte(b) => match b {
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
        }
    }
    None
}

fn process_confirm_kill(tokens: &[Token], model: &mut Model) -> Option<LoopAction> {
    for &t in tokens {
        match t {
            Token::Byte(b'y') | Token::Byte(b'Y') => {
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

// -- Rendering --

// SGR codes for the bar chrome. The chrome reads as a phosphor-amber
// CRT bezel: dark bar background with two-tone amber text on top.
const SGR_RESET: &str = "\x1b[0m";
const SGR_BAR_BG: &str = "\x1b[48;5;236m"; // dark gray bar background (#303030)
const SGR_BAR_END: &str = "\x1b[49m"; // restore default bg, keep fg
const SGR_AMBER: &str = "\x1b[1;38;2;235;185;90m"; // bold warm amber (#ebb95a)
const SGR_AMBER_DIM: &str = "\x1b[38;2;130;105;75m"; // muted warm amber (#82694b)
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
            l.push_plain("   ");
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
    // Clip the styled label to whatever visible space is left after
    // the leading pad. For labels that already fit this is a no-op;
    // for over-long labels this drops the tail so nothing bleeds
    // onto or clobbers the right margin.
    let available = width.saturating_sub(lead);
    let clipped = clip_styled(&label.styled, available);
    out.write_all(SGR_BAR_BG.as_bytes())?;
    for _ in 0..lead {
        out.write_all(b" ")?;
    }
    out.write_all(clipped.as_bytes())?;
    for _ in 0..trail {
        out.write_all(b" ")?;
    }
    out.write_all(SGR_BAR_END.as_bytes())?;
    out.write_all(SGR_RESET.as_bytes())?;
    out.write_all(b"\r\n")?;
    Ok(())
}

// Top bar + table header + bottom bar = 3 lines of overhead.
const CHROME_LINES: usize = 3;

// Column widths sized to their headers. Short relative-age values
// ("now", "42s", "13m", "4h", "9d") never exceed the header width.
const COL_CREATED: &str = "created";
const COL_ACTIVE: &str = "active";
const COL_GAP: usize = 2;

/// Clip a string to at most `max_chars` characters. Used on row
/// content so rows longer than the terminal width are truncated
/// rather than wrapped or clobbering the rightmost column.
fn clip(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Clip a styled string (ANSI + text) so the visible character count
/// does not exceed `max_visible`. ANSI CSI escape sequences
/// (`\x1b[...<final>`) are passed through verbatim — they don't
/// consume visible columns, but they stay attached to the text
/// they were styling.
fn clip_styled(styled: &str, max_visible: usize) -> String {
    let mut out = String::new();
    let mut visible = 0usize;
    // 0 = normal, 1 = just saw ESC, 2 = inside CSI (past `[`, reading
    // params/intermediates until a final byte in 0x40..=0x7e).
    let mut esc = 0u8;
    for ch in styled.chars() {
        match esc {
            0 => {
                if ch == '\x1b' {
                    out.push(ch);
                    esc = 1;
                } else {
                    if visible >= max_visible {
                        break;
                    }
                    out.push(ch);
                    visible += 1;
                }
            }
            1 => {
                out.push(ch);
                esc = if ch == '[' { 2 } else { 0 };
            }
            _ => {
                out.push(ch);
                if matches!(ch as u32, 0x40..=0x7e) {
                    esc = 0;
                }
            }
        }
    }
    out
}

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Render a unix-ms timestamp as a short relative-to-now string:
/// "now" for the first 5 seconds, then "Ns", "Nm", "Nh", "Nd".
fn format_age(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 5 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

pub fn render(
    model: &Model,
    width: u16,
    height: u16,
    out: &mut impl Write,
) -> io::Result<()> {
    let w = width as usize;

    out.write_all(b"\x1b[2J\x1b[H")?;

    render_bar(out, w, &title_label(model), BarAlign::Center)?;

    // Name column width grows to fit the longest session name, with a
    // floor of "NAME".len() so the header never overflows.
    let name_width = model
        .sessions
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("name".len());
    let created_width = COL_CREATED.len();
    let active_width = COL_ACTIVE.len();

    // Header row, styled like the bindings (dim amber) to subordinate
    // it to the list content.
    let header = clip(
        &format!(
            "  {name:<name_width$}{gap}{created:<created_width$}{gap}{active:<active_width$}",
            name = "name",
            created = COL_CREATED,
            active = COL_ACTIVE,
            gap = " ".repeat(COL_GAP),
        ),
        w,
    );
    write!(out, "{SGR_BAR_BG}{SGR_AMBER_DIM}{header:<w$}{SGR_RESET}\r\n")?;

    if model.sessions.is_empty() {
        out.write_all(b"  (no sessions)\r\n")?;
    } else {
        let now = now_unix_ms();
        let max_visible = (height as usize).saturating_sub(CHROME_LINES);
        let (start, end) = viewport(model.sessions.len(), model.selected, max_visible);

        for (i, s) in model.sessions[start..end].iter().enumerate() {
            let abs_i = start + i;
            // 2-char prefix: [attached marker][selected arrow]. An
            // asterisk marks sessions attached elsewhere so the user
            // sees the "already attached" state without having to
            // hit Enter and get the pre-flight rejection. ASCII so
            // we don't depend on the terminal's locale/font.
            let dot = if s.attached { '*' } else { ' ' };
            let arrow = if abs_i == model.selected { '>' } else { ' ' };
            let created = format_age(now, s.started_at_unix_ms);
            let active = format_age(now, s.last_active_unix_ms());
            let text = clip(
                &format!(
                    "{dot}{arrow}{name:<name_width$}{gap}{created:<created_width$}{gap}{active:<active_width$}",
                    name = s.name,
                    gap = " ".repeat(COL_GAP),
                ),
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
        Session {
            name: name.to_string(),
            attached: false,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
        }
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

    fn tokenize(bytes: &[u8]) -> Vec<Token> {
        let mut p = InputParser::new();
        let mut out = vec![];
        p.feed(bytes, &mut out);
        out
    }

    #[test]
    fn parser_up_arrow() {
        assert_eq!(tokenize(&[0x1b, b'[', b'A']), vec![Token::Csi(b'A')]);
    }

    #[test]
    fn parser_down_arrow() {
        assert_eq!(tokenize(&[0x1b, b'[', b'B']), vec![Token::Csi(b'B')]);
    }

    #[test]
    fn parser_plain_bytes_are_byte_tokens() {
        assert_eq!(
            tokenize(b"q\rj"),
            vec![Token::Byte(b'q'), Token::Byte(b'\r'), Token::Byte(b'j')],
        );
    }

    #[test]
    fn parser_unterminated_esc_is_bare() {
        assert_eq!(tokenize(&[0x1b]), vec![Token::BareEsc]);
    }

    #[test]
    fn parser_esc_plus_non_bracket_emits_both() {
        // ESC followed by a non-bracket byte in the same buffer:
        // bare Escape plus the following byte. Lets ESC+q still quit.
        assert_eq!(tokenize(&[0x1b, b'q']), vec![Token::BareEsc, Token::Byte(b'q')]);
    }

    #[test]
    fn parser_csi_split_across_feeds() {
        let mut p = InputParser::new();
        let mut out = vec![];
        p.feed(&[0x1b, b'['], &mut out);
        assert!(out.is_empty());
        p.feed(&[b'B'], &mut out);
        assert_eq!(out, vec![Token::Csi(b'B')]);
    }

    #[test]
    fn parser_csi_consumes_params() {
        // CSI with a parameter byte before the final (e.g., F5).
        assert_eq!(tokenize(b"\x1b[15~"), vec![Token::Csi(b'~')]);
    }

    #[test]
    fn parser_stream_of_arrows_and_enter() {
        assert_eq!(
            tokenize(&[0x1b, b'[', b'B', 0x1b, b'[', b'B', b'\r']),
            vec![Token::Csi(b'B'), Token::Csi(b'B'), Token::Byte(b'\r')],
        );
    }

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
