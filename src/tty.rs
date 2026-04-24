use std::io::{self, Write};
use std::mem::MaybeUninit;

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

pub fn enter_alt_screen(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[?1049h")?;
    out.write_all(b"\x1b[?25l")?;
    // DECCKM off: force normal cursor-key mode so arrows send
    // `ESC [ A/B` (which our parser understands) rather than the
    // application-mode `ESC O A/B`. Emacs and some other TUIs
    // leave DECCKM on; a mid-session detach never gives them a
    // chance to restore it, so we set the state ourselves.
    out.write_all(b"\x1b[?1l")?;
    // DECAWM off: disable auto-wrap at the right margin. We clip
    // rows to width in the renderer too, but this is a defensive
    // layer — any off-by-one in our width accounting gets
    // absorbed at the margin instead of breaking the layout.
    out.write_all(b"\x1b[?7l")?;
    Ok(())
}

pub fn leave_alt_screen(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[?25h")?;
    // DECAWM back on so the user's shell behaves normally afterward.
    out.write_all(b"\x1b[?7h")?;
    out.write_all(b"\x1b[?1049l")?;
    Ok(())
}

/// Clear the visible screen and home the cursor. Used right before
/// handing the terminal to `shpool attach` so the user's pre-launch
/// shell history doesn't flash behind the child process while it
/// boots. Scrollback is preserved (`\x1b[3J` would wipe it too).
pub fn clear_screen(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[2J\x1b[H")?;
    out.flush()
}
