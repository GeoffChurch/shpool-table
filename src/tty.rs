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

/// True when both stdin and stdout are connected to a terminal.
pub fn is_interactive() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

pub fn enter_alt_screen(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[?1049h")?;
    out.write_all(b"\x1b[?25l")?;
    Ok(())
}

pub fn leave_alt_screen(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[?25h")?;
    out.write_all(b"\x1b[?1049l")?;
    Ok(())
}
