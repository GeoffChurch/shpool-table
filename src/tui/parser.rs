// -- Input parser (bytes → Input stream) --
//
// A state machine that consumes a raw byte stream and emits decoded
// inputs: either semantic keystrokes (Input::Key) or focus events
// (Input::FocusGained). Downstream code pattern-matches on Input
// rather than raw bytes or intermediate tokens.
//
// Recognized ESC sequences:
//   - ESC [ A/B/C/D          → Input::Key(Up/Down/Right/Left)
//   - ESC [ I                → Input::FocusGained (terminal regained
//                               focus — we use this to refresh the
//                               session list on re-focus)
//   - ESC [ O                → focus lost; discarded silently
//   - ESC alone at buffer end → Input::Key(Esc)
//   - ESC + non-bracket byte  → Input::Key(Esc) + decoded(byte). Lets
//                                Alt-letter and ESC-then-q still fire.
//   - ESC [ ... <unknown>     → Input::Key(Other).

use super::keymap::Key;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Input {
    Key(Key),
    FocusGained,
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

    /// Consume `bytes`, pushing decoded inputs onto `out`. Parser
    /// state persists across calls so a CSI sequence split across
    /// reads (rare but possible) still parses correctly.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        for &b in bytes {
            match self.state {
                ParserState::Normal => {
                    if b == 0x1b {
                        self.state = ParserState::Esc;
                    } else {
                        out.push(Input::Key(decode_byte(b)));
                    }
                }
                ParserState::Esc => {
                    if b == b'[' {
                        self.state = ParserState::EscBracket;
                    } else {
                        // ESC + non-bracket byte: emit both as separate
                        // keystrokes so Alt-letter chords and ESC-then-q
                        // still fire their individual actions.
                        out.push(Input::Key(Key::Esc));
                        out.push(Input::Key(decode_byte(b)));
                        self.state = ParserState::Normal;
                    }
                }
                ParserState::EscBracket => {
                    // CSI: skip param (0x30..=0x3f) and intermediate
                    // (0x20..=0x2f) bytes until a final byte terminates.
                    if (0x40..=0x7e).contains(&b) {
                        match b {
                            b'I' => out.push(Input::FocusGained),
                            // ESC [ O is focus-lost; we don't have a
                            // use for it (the next keystroke arrives
                            // via stdin anyway), so discard.
                            b'O' => {}
                            _ => out.push(Input::Key(decode_csi(b))),
                        }
                        self.state = ParserState::Normal;
                    }
                }
            }
        }
        if matches!(self.state, ParserState::Esc) {
            out.push(Input::Key(Key::Esc));
            self.state = ParserState::Normal;
        }
    }
}

/// Decode a byte received in Normal parser state (i.e. not inside an
/// ESC sequence) into a semantic Key. Maps printable bytes to Char,
/// known controls to their named Key, and unmapped control bytes to
/// `Key::Ctrl(b)`. Non-ASCII bytes (0x80..=0xff) become `Key::Other`.
fn decode_byte(b: u8) -> Key {
    match b {
        0x08 | 0x7f => Key::Backspace, // BS and DEL — terminals disagree
        0x09 => Key::Tab,
        0x0a | 0x0d => Key::Enter, // LF and CR — both fire Enter
        0x20..=0x7e => Key::Char(b),
        // 0x01..=0x1a is Ctrl-A..Ctrl-Z, minus the ones above that
        // have dedicated names (BS, Tab, LF, CR).
        0x01..=0x1a => Key::Ctrl(b),
        _ => Key::Other,
    }
}

/// Decode a CSI final byte (the terminator of an `ESC [ ... <final>`
/// sequence) into a semantic Key. Only the arrows have names in our
/// Key vocabulary; everything else (page-up/down, function keys, etc.)
/// falls through to `Key::Other`.
fn decode_csi(final_byte: u8) -> Key {
    match final_byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Input> {
        let mut p = InputParser::new();
        let mut out = vec![];
        p.feed(bytes, &mut out);
        out
    }

    /// Shortcut for tests that only care about the Key stream,
    /// filtering out any focus events.
    fn decode_keys(bytes: &[u8]) -> Vec<Key> {
        decode(bytes)
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parser_up_arrow() {
        assert_eq!(decode_keys(&[0x1b, b'[', b'A']), vec![Key::Up]);
    }

    #[test]
    fn parser_down_arrow() {
        assert_eq!(decode_keys(&[0x1b, b'[', b'B']), vec![Key::Down]);
    }

    #[test]
    fn parser_plain_bytes_decode_to_chars_and_enter() {
        assert_eq!(
            decode_keys(b"q\rj"),
            vec![Key::Char(b'q'), Key::Enter, Key::Char(b'j')],
        );
    }

    #[test]
    fn parser_unterminated_esc_is_bare() {
        assert_eq!(decode_keys(&[0x1b]), vec![Key::Esc]);
    }

    #[test]
    fn parser_esc_plus_non_bracket_emits_both() {
        // ESC followed by a non-bracket byte in the same buffer:
        // bare Esc plus the decoded following byte. Lets ESC+q still
        // fire the Quit binding (via the Char(b'q') that follows).
        assert_eq!(decode_keys(&[0x1b, b'q']), vec![Key::Esc, Key::Char(b'q')]);
    }

    #[test]
    fn parser_csi_split_across_feeds() {
        let mut p = InputParser::new();
        let mut out = vec![];
        p.feed(&[0x1b, b'['], &mut out);
        assert!(out.is_empty());
        p.feed(&[b'B'], &mut out);
        assert_eq!(out, vec![Input::Key(Key::Down)]);
    }

    #[test]
    fn parser_csi_consumes_params() {
        // CSI with a parameter byte before the final (e.g., F5 sends
        // ESC [ 15 ~). We don't bind function keys, so the final `~`
        // decodes to Key::Other.
        assert_eq!(decode_keys(b"\x1b[15~"), vec![Key::Other]);
    }

    #[test]
    fn parser_stream_of_arrows_and_enter() {
        assert_eq!(
            decode_keys(&[0x1b, b'[', b'B', 0x1b, b'[', b'B', b'\r']),
            vec![Key::Down, Key::Down, Key::Enter],
        );
    }

    #[test]
    fn parser_focus_gained() {
        // ESC [ I is the terminal's focus-in report (requires
        // focus-reporting to be enabled, which tty.rs does on
        // alt-screen entry).
        assert_eq!(decode(b"\x1b[I"), vec![Input::FocusGained]);
    }

    #[test]
    fn parser_focus_lost_is_dropped() {
        // ESC [ O is focus-out. We don't need to react — no event
        // emitted at all.
        assert_eq!(decode(b"\x1b[O"), vec![]);
    }

    #[test]
    fn parser_focus_then_keystroke() {
        // Focus report arriving alongside input: both decode and
        // survive in order.
        assert_eq!(
            decode(b"\x1b[Ij"),
            vec![Input::FocusGained, Input::Key(Key::Char(b'j'))],
        );
    }

    #[test]
    fn decode_byte_covers_named_controls() {
        assert_eq!(decode_byte(0x08), Key::Backspace);
        assert_eq!(decode_byte(0x7f), Key::Backspace);
        assert_eq!(decode_byte(0x09), Key::Tab);
        assert_eq!(decode_byte(0x0a), Key::Enter);
        assert_eq!(decode_byte(0x0d), Key::Enter);
        assert_eq!(decode_byte(b' '), Key::Char(b' '));
        assert_eq!(decode_byte(b'a'), Key::Char(b'a'));
        assert_eq!(decode_byte(0x03), Key::Ctrl(0x03)); // Ctrl-C
        // 0xff is non-ASCII — out of any of our recognized ranges.
        assert_eq!(decode_byte(0xff), Key::Other);
    }
}
