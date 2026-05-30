//! The `shpool events` subscription: a child process whose stdout we
//! multiplex with stdin (see `main_loop`) so daemon-side changes —
//! sessions created, killed, attached, or detached by *other* clients —
//! push a refresh into the table without waiting on a keystroke.
//!
//! The wire payload is content-free: every line just means "something
//! changed, re-list", so we never parse it. A readable pipe triggers a
//! refresh; an EOF means the subscription is over and we fall back to
//! keystroke-driven refresh. The subscribe attempt doubles as the
//! capability probe — a daemon that's down, that predates the events
//! socket, or that drops us as a slow subscriber all converge on the
//! same EOF, handled uniformly.

use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, Command, Stdio};

use crate::Flags;

/// A live subscription: the `shpool events` child. The read end of its
/// pipe lives in `child.stdout`.
pub struct EventsSub {
    child: Child,
}

/// Outcome of draining whatever the pipe had ready.
pub enum Drain {
    /// Bytes were read — daemon-side activity. Re-list.
    Activity,
    /// End of stream: the subscription is over (daemon gone, dropped,
    /// or never had an events socket). Tear down and fall back.
    Eof,
}

impl EventsSub {
    /// Spawn `shpool [flags] events` with stderr silenced. Returns
    /// `None` if the child can't even be spawned (e.g. `shpool` missing
    /// from PATH); the caller then runs in keystroke-refresh fallback —
    /// the same state a later EOF lands in.
    pub fn spawn(flags: &Flags) -> Option<Self> {
        let mut cmd = Command::new("shpool");
        flags.apply(&mut cmd);
        cmd.arg("events")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = cmd.spawn().ok()?;
        Some(Self { child })
    }

    /// The pipe's read fd, for `poll`. Valid for the lifetime of `self`.
    pub fn fd(&self) -> RawFd {
        self.child
            .stdout
            .as_ref()
            .expect("events stdout is piped")
            .as_raw_fd()
    }

    /// Read whatever is ready (one pipe-sized gulp) and discard it — the
    /// bytes' mere arrival is the whole signal. Call only when `poll`
    /// reported the fd readable, so the read won't block.
    pub fn drain(&mut self) -> Drain {
        let mut scratch = [0u8; 4096];
        let stdout = self.child.stdout.as_mut().expect("events stdout is piped");
        match stdout.read(&mut scratch) {
            Ok(0) => Drain::Eof,
            Ok(_) => Drain::Activity,
            // A signal (SIGWINCH) interrupted the read — not EOF. Treat
            // as a spurious wake; the worst case is one extra refresh.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Drain::Activity,
            // Any other error: end the subscription and fall back.
            Err(_) => Drain::Eof,
        }
    }
}

impl Drop for EventsSub {
    /// Kill, then reap. The child is blocked reading the daemon socket
    /// and may not notice we're gone for a long time, so SIGKILL-then-
    /// wait is what keeps a quit (or any `?`-propagated error, or an
    /// unwinding panic) from hanging on the orphan. `kill()` is SIGKILL
    /// in std — fine for a stateless subscriber.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
