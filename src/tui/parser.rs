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

#[cfg(test)]
mod tests {
    use super::*;

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
}
