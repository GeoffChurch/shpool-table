use std::io::{self, Write};

use super::keymap::{
    CONFIRM_FORCE_HINTS, CONFIRM_KILL_HINTS, CREATE_HINTS, NORMAL_BINDINGS,
};
use super::model::{Mode, Model};

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

fn confirm_force_label(name: &str) -> Label {
    let mut l = Label::default();
    l.push_key(&format!("\"{name}\""));
    l.push_plain(" already attached. force-attach?   (");
    push_hints(&mut l, CONFIRM_FORCE_HINTS);
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
            Mode::ConfirmForce(name) => confirm_force_label(name),
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
