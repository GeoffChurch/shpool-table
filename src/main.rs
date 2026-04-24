mod session;
mod tty;
mod tui;

use std::io::{self, BufWriter, Write};
use std::process;

use anyhow::{Context, Result};

use crate::session::{ListReply, Session};
use crate::tui::{Command, Event, InputParser, Model};

/// Current wall-clock time in unix milliseconds. Passed into `render`
/// so the relative-age columns have a deterministic `now` (tests pass
/// a fixed value; production passes the current time).
fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn fetch_sessions() -> Result<Vec<Session>> {
    let out = process::Command::new("shpool")
        .args(["list", "--json"])
        .output()
        .context("running `shpool list --json`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("`shpool list --json` failed: {}", stderr.trim());
    }
    let reply: ListReply =
        serde_json::from_slice(&out.stdout).context("parsing shpool list JSON")?;
    Ok(reply.sessions)
}

fn main() -> Result<()> {
    if let Ok(inside) = std::env::var("SHPOOL_SESSION_NAME") {
        // Nested sessions get messy: a force-attach to the outer
        // session bumps us off, sessions created from here inherit
        // this env, and ^D leaves the user in the wrong layer.
        // Detach first, then fall through to `shpool list`.
        eprintln!(
            "shpool-table: inside shpool session \"{inside}\" — won't run here. Nested\n\
             sessions get messy (outer attach gets bumped on force, sessions created\n\
             here inherit this env, ^D leaves you in the wrong layer). Detach first\n\
             to manage sessions. Current list:\n"
        );
        use std::os::unix::process::CommandExt;
        let err = process::Command::new("shpool").arg("list").exec();
        anyhow::bail!("exec shpool list: {err}");
    }
    run_tui()
}

fn run_tui() -> Result<()> {
    tty::install_sigwinch_handler().context("installing SIGWINCH handler")?;

    let mut model = Model::new(Vec::new());
    let mut parser = InputParser::new();

    // RawMode guard held for the whole TUI session — only temporarily
    // toggled via suspend/resume when we need to hand the terminal to
    // a child process. Dropped on exit (normal or error) via RAII.
    let raw = tty::RawMode::enter().context("entering raw mode")?;
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    tty::enter_alt_screen(&mut out)?;

    // Ensure we leave alt-screen even on error returns from main_loop.
    let result = main_loop(&mut model, &mut parser, &mut out, &raw);

    let _ = tty::leave_alt_screen(&mut out);
    let _ = out.flush();
    result
}

/// Run the cascade loop until `model.quit` is set.
///
/// Keeps the TUI in alt-screen + raw mode the whole time, except when
/// `execute` spawns a child `shpool attach` — that path toggles both
/// off, runs the child, and toggles them back on.
fn main_loop<W: Write>(
    model: &mut Model,
    parser: &mut InputParser,
    out: &mut W,
    raw: &tty::RawMode,
) -> Result<()> {
    // Initial fetch: if the daemon is up, show its list immediately;
    // if not, the RefreshFailed event surfaces the error in the footer
    // and the user can retry (or quit).
    let initial = match fetch_sessions() {
        Ok(s) => Event::SessionsRefreshed(s),
        Err(e) => Event::RefreshFailed(format!("{e}")),
    };
    run_cascade(model, initial, out, raw)?;

    let mut buf = [0u8; 16];
    loop {
        let (w, h) = tty::tty_size().unwrap_or((80, 24));
        tui::render(model, w, h, now_unix_ms(), out)?;
        out.flush()?;

        // `quit` is checked AFTER the draw, not before. The final
        // frame is written but immediately wiped by leave_alt_screen
        // on exit, so the user never sees it. Saves one draw-on-exit
        // and keeps the model's visible state aligned with the last
        // drawn frame — inverting would subtly decouple them.
        if model.quit {
            return Ok(());
        }

        match tty::read_stdin(&mut buf) {
            Ok(0) => {
                // EOF on stdin — treat as quit so we exit cleanly.
                model.quit = true;
            }
            Ok(n) => {
                // A single read can decode to multiple keys (pastes,
                // CSI sequences, jj\r typed fast). Feed each through
                // its own cascade so auto-refresh / attach / etc.
                // fire per keystroke.
                let mut keys = Vec::new();
                parser.feed(&buf[..n], &mut keys);
                for key in keys {
                    run_cascade(model, Event::Key(key), out, raw)?;
                    if model.quit {
                        break;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // SIGWINCH — loop back to re-query tty_size and redraw.
                continue;
            }
            Err(e) => return Err(e).context("reading stdin"),
        }
    }
}

/// Feed `event` through `update`, execute any resulting Command, then
/// feed the executor's follow-up event back through `update`, repeat
/// until the cascade runs dry.
///
/// Load-bearing: the Create flow is
///   Key(Enter) → Create → AttachExited → Refresh → SessionsRefreshed
/// where the final Refresh step is how the newly-created session
/// enters `model.sessions`. Stopping after the first Command would
/// leave the new session invisible.
fn run_cascade<W: Write>(
    model: &mut Model,
    event: Event,
    out: &mut W,
    raw: &tty::RawMode,
) -> Result<()> {
    let mut next = tui::update(model, event);
    while let Some(cmd) = next.take() {
        let follow_up = execute(cmd, model, out, raw)?;
        let Some(ev) = follow_up else { break };
        next = tui::update(model, ev);
    }
    Ok(())
}

/// Side-effect executor. All I/O happens here: `shpool list --json`,
/// `shpool kill`, the attach subprocess + terminal suspend, and the
/// quit flag. Returns the follow-up Event for the cascade to feed
/// back through `update`, or None if there's nothing to cascade.
fn execute<W: Write>(
    cmd: Command,
    model: &mut Model,
    out: &mut W,
    raw: &tty::RawMode,
) -> Result<Option<Event>> {
    match cmd {
        Command::Quit => {
            model.quit = true;
            Ok(None)
        }
        Command::Refresh => {
            let ev = match fetch_sessions() {
                Ok(sessions) => Event::SessionsRefreshed(sessions),
                Err(e) => Event::RefreshFailed(format!("{e}")),
            };
            Ok(Some(ev))
        }
        Command::Attach { name, force } => match preflight_attach(&name, force) {
            Preflight::RefreshFailed(e) => {
                // Route through Event::RefreshFailed so the
                // "shpool list:" prefix lives in exactly one place
                // (update's RefreshFailed handler).
                Ok(Some(Event::RefreshFailed(format!("{e}"))))
            }
            Preflight::Gone { sessions } => {
                model.set_error(format!("session '{name}' is gone"));
                Ok(Some(Event::SessionsRefreshed(sessions)))
            }
            Preflight::AttachedElsewhere { sessions } => {
                // Pop ConfirmForce rather than bumping the other
                // client silently. The user can press 'y' to re-issue
                // Attach with force=true, which skips this check.
                model.mode = crate::tui::Mode::ConfirmForce(name);
                Ok(Some(Event::SessionsRefreshed(sessions)))
            }
            Preflight::ClearToAttach => {
                let ok =
                    with_tui_suspended(out, raw, || shell_attach(&name, force))?;
                Ok(Some(Event::AttachExited { ok, name }))
            }
        },
        Command::Create(name) => {
            // Duplicate-name check happened in update; by here it's
            // safe to spawn. `shpool attach` with a new name creates
            // atomically on the daemon side.
            let ok =
                with_tui_suspended(out, raw, || shell_attach(&name, false))?;
            Ok(Some(Event::AttachExited { ok, name }))
        }
        Command::Kill(name) => {
            let (ok, err) = shell_kill(&name)?;
            Ok(Some(Event::KillFinished { ok, name, err }))
        }
    }
}

/// Result of the attach pre-flight — a fresh `shpool list --json`
/// query followed by a presence + attached-elsewhere check.
///
/// Split out of `execute` because the decision (which of these four
/// outcomes) is independent of the action (what to emit next). Also
/// keeps the match arms in execute readable.
enum Preflight {
    /// `fetch_sessions` itself failed — error is display-ready.
    RefreshFailed(anyhow::Error),
    /// Session no longer exists; some other client killed it since
    /// the user's last view. Fresh sessions are carried so the
    /// follow-up Event::SessionsRefreshed can update the model.
    Gone { sessions: Vec<Session> },
    /// Session exists but is attached from another terminal and
    /// `force` was not set. Caller transitions model to ConfirmForce.
    AttachedElsewhere { sessions: Vec<Session> },
    /// Session exists and is ready to attach. `sessions` is not
    /// carried — the AttachExited handler cascades into a fresh
    /// Refresh anyway.
    ClearToAttach,
}

fn preflight_attach(name: &str, force: bool) -> Preflight {
    let sessions = match fetch_sessions() {
        Ok(s) => s,
        Err(e) => return Preflight::RefreshFailed(e),
    };
    if !sessions.iter().any(|s| s.name == name) {
        return Preflight::Gone { sessions };
    }
    let attached_elsewhere = !force
        && sessions.iter().find(|s| s.name == name).map(|s| s.attached).unwrap_or(false);
    if attached_elsewhere {
        return Preflight::AttachedElsewhere { sessions };
    }
    Preflight::ClearToAttach
}

/// Spawn `shpool attach [-f] <name>` and block until it exits.
/// The caller (`with_tui_suspended`) is responsible for putting the
/// terminal into cooked mode + primary screen first.
fn shell_attach(name: &str, force: bool) -> Result<bool> {
    let mut cmd = process::Command::new("shpool");
    cmd.arg("attach");
    if force {
        cmd.arg("-f");
    }
    cmd.arg(name);
    let status = cmd.status().context("spawning `shpool attach`")?;
    Ok(status.success())
}

/// Run `shpool kill <name>` and collect its outcome + stderr.
/// Returns (ok, err_message). Shell-out errors (couldn't even spawn
/// shpool) propagate; non-zero exit with a stderr payload comes back
/// as `ok=false` + `err=Some(detail)`.
fn shell_kill(name: &str) -> Result<(bool, Option<String>)> {
    let output = process::Command::new("shpool")
        .args(["kill", name])
        .output()
        .context("running `shpool kill`")?;
    let ok = output.status.success();
    let err = if ok {
        None
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            Some(format!("kill {name} failed"))
        } else {
            Some(format!("kill {name}: {detail}"))
        }
    };
    Ok((ok, err))
}

/// Tear the TUI down (leave alt-screen, cooked mode), run `f`, then
/// put the TUI back up. Used for the attach subprocess, which needs a
/// clean cooked tty to take over.
///
/// Restores terminal state on both success and error return paths of
/// `f` so an attach that failed halfway still hands us back a usable
/// TUI.
fn with_tui_suspended<F, R, W: Write>(
    out: &mut W,
    raw: &tty::RawMode,
    f: F,
) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    tty::leave_alt_screen(out)?;
    // Clear the primary screen before the child starts so its first
    // frame doesn't land on top of leftover shell prompt history.
    tty::clear_screen(out)?;
    out.flush()?;
    raw.suspend()?;

    // Capture the result so we still restore the terminal on error.
    let result = f();

    raw.resume()?;
    tty::enter_alt_screen(out)?;
    out.flush()?;

    result
}
