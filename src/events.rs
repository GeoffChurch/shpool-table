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
        cmd.arg("events");
        Self::spawn_cmd(cmd)
    }

    /// Apply the standard stdio wiring — stdout piped so we can read the
    /// event stream, stdin and stderr silenced — and spawn. Split out of
    /// `spawn` so tests can drive the drain/teardown machinery through a
    /// stub program (e.g. `sh -c …`) using the very same pipe setup the
    /// real subscriber runs with, no live daemon required.
    fn spawn_cmd(mut cmd: Command) -> Option<Self> {
        cmd.stdin(Stdio::null())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A stub subscriber: `sh -c <script>` through the real stdio wiring,
    /// standing in for `shpool events` so we can test drain/teardown with
    /// no daemon. `sh` is POSIX-ubiquitous (present on the CI runner).
    fn stub(script: &str) -> EventsSub {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        EventsSub::spawn_cmd(cmd).expect("spawn sh stub")
    }

    #[test]
    fn drain_reports_activity_then_eof() {
        let mut sub = stub("printf x");
        // The byte the child wrote reads back as activity (we discard it)...
        assert!(matches!(sub.drain(), Drain::Activity));
        // ...and the now-closed pipe reads back as EOF.
        assert!(matches!(sub.drain(), Drain::Eof));
    }

    #[test]
    fn drain_reports_eof_when_stream_closes_empty() {
        // A subscriber that produces nothing and exits — the daemon-down /
        // no-events-socket shape — drains straight to EOF.
        let mut sub = stub("exit 0");
        assert!(matches!(sub.drain(), Drain::Eof));
    }

    #[test]
    fn drop_kills_and_reaps_without_awaiting_child_lifetime() {
        // The child would live 30s; Drop must SIGKILL then reap it
        // promptly, not wait() out its natural life. If kill didn't
        // precede wait, this would hang for ~30s.
        let sub = stub("sleep 30");
        let start = Instant::now();
        drop(sub);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Drop blocked on the child's lifetime — kill must precede wait",
        );
    }

    #[test]
    fn fd_is_exposed_while_live() {
        let sub = stub("sleep 30");
        assert!(sub.fd() >= 0, "piped stdout should yield a real fd");
    }
}
