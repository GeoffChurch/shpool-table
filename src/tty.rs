use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::sync::Once;

use anyhow::{Context, Result};

/// RAII guard that puts stdin into raw mode and restores the previous
/// termios on drop.
pub struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    pub fn enter() -> Result<Self> {
        let saved = unsafe {
            let mut t = MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(libc::STDIN_FILENO, t.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error()).context("tcgetattr");
            }
            t.assume_init()
        };
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        let rc = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) };
        if rc != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr raw");
        }
        Ok(RawMode { saved })
    }

    /// Temporarily restore the saved (cooked) terminal settings.
    /// Pair with `resume` — used by suspend-TUI so child processes
    /// (`shpool attach`) see a cooked terminal, then we restore raw
    /// mode when they exit.
    pub fn suspend(&self) -> Result<()> {
        let rc = unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr cooked");
        }
        Ok(())
    }

    /// Re-apply raw mode after a `suspend`. Symmetric with `enter`
    /// but reuses the saved termios from construction rather than
    /// re-reading it.
    pub fn resume(&self) -> Result<()> {
        let mut raw = self.saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        let rc = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) };
        if rc != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr raw");
        }
        Ok(())
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

/// Query the terminal size as (cols, rows).
pub fn tty_size() -> Result<(u16, u16)> {
    let mut ws = MaybeUninit::<libc::winsize>::uninit();
    let rc =
        unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error()).context("ioctl TIOCGWINSZ");
    }
    let ws = unsafe { ws.assume_init() };
    Ok((ws.ws_col, ws.ws_row))
}

/// Install a no-op SIGWINCH handler so the signal interrupts blocking
/// reads (libc::read returns EINTR) instead of being silently ignored.
/// This lets the event loop re-query terminal size and redraw on resize.
pub fn install_sigwinch_handler() -> Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigwinch_noop as *const () as usize;
        // No SA_RESTART: let read() return EINTR on signal delivery.
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error()).context("sigaction SIGWINCH");
        }
    }
    Ok(())
}

extern "C" fn sigwinch_noop(_sig: libc::c_int) {}

/// Read from stdin via libc::read so EINTR from signal handlers is
/// visible to the caller (Rust's std Read silently retries on EINTR).
pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

// Terminal state sequences. `TUI_ENTER` sets: 1049h alt-screen,
// 25l cursor hidden, 1l DECCKM off (arrows send `ESC [ A/B` not
// `ESC O A/B`), 7l DECAWM off (no auto-wrap at right margin),
// 1004h focus reporting on.
//
// DECCKM: Emacs and some other TUIs leave it on; a mid-session detach
// never gives them a chance to restore it, so we assert it off here.
// DECAWM: renderer clips to width anyway — this is a defensive layer.
//
// `TUI_LEAVE` mirrors enter but deliberately OMITS re-enabling DECCKM
// (`?1h`). The user's shell typically wants DECCKM off by default.
const TUI_ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?1l\x1b[?7l\x1b[?1004h";
const TUI_LEAVE: &[u8] = b"\x1b[?25h\x1b[?7h\x1b[?1004l\x1b[?1049l";

/// Write `bytes` to stdout via `libc::write`, retrying on EINTR and
/// handling short writes. Used for terminal-state sequences so they
/// bypass any BufWriter the caller owns — safe to call from Drop
/// during unwinding, where the BufWriter may be partially torn down.
fn write_raw_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let n = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                remaining.as_ptr() as *const libc::c_void,
                remaining.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        remaining = &remaining[n as usize..];
    }
    Ok(())
}

/// RAII guard for alt-screen + cursor-hide + DECCKM/DECAWM + focus
/// reporting. `Drop` writes the disable sequence via `libc::write`
/// directly to the tty fd, so unwinding panics, `?` early returns,
/// and normal exits all clean up without threading a writer through.
pub struct AltScreen;

impl AltScreen {
    pub fn enter() -> Result<Self> {
        write_raw_stdout(TUI_ENTER).context("enter alt-screen")?;
        Ok(AltScreen)
    }

    /// Temporarily leave alt-screen so a child process (`shpool
    /// attach`) gets a normal terminal. Pair with `resume`.
    pub fn suspend(&self) -> Result<()> {
        write_raw_stdout(TUI_LEAVE).context("suspend alt-screen")
    }

    /// Re-enter alt-screen after a `suspend`.
    pub fn resume(&self) -> Result<()> {
        write_raw_stdout(TUI_ENTER).context("resume alt-screen")
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        let _ = write_raw_stdout(TUI_LEAVE);
    }
}

static PANIC_HOOK: Once = Once::new();

/// Install a panic hook that emits the terminal-reset sequence before
/// delegating to the previous hook. Idempotent. Required for
/// `panic = "abort"` builds where Drop never runs; belt-and-braces on
/// unwinding builds in case a guard gets refactored out of a code path.
///
/// Hook order matters: resets go out first so the default hook's panic
/// message lands on the primary screen, not the alt-screen that's
/// about to be wiped.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = write_raw_stdout(TUI_LEAVE);
            prev(info);
        }));
    });
}

/// Clear the visible screen and home the cursor. Used right before
/// handing the terminal to `shpool attach` so the user's pre-launch
/// shell history doesn't flash behind the child process while it
/// boots. Scrollback is preserved (`\x1b[3J` would wipe it too).
pub fn clear_screen(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[2J\x1b[H")?;
    out.flush()
}
