mod session;
mod tty;
mod tui;

use std::io::{self, BufWriter, Write};
use std::process;

use anyhow::{Context, Result};

use crate::session::{ListReply, Session};
use crate::tui::{Command, Event, InputParser, Mode, Model};

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

/// Refetch sessions into the model. On failure the error is parked in
/// the model's error slot rather than propagated, so the TUI keeps
/// running with whatever stale list it has and the user sees the
/// problem in the bottom bar.
fn refresh_sessions(model: &mut Model) {
    match fetch_sessions() {
        Ok(s) => model.refresh(s),
        Err(e) => model.set_error(format!("shpool list: {e}")),
    }
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
    let mut model = Model::new(Vec::new());
    refresh_sessions(&mut model);
    run_tui(model)
}

fn run_tui(mut model: Model) -> Result<()> {
    tty::install_sigwinch_handler().context("installing SIGWINCH handler")?;

    let mut parser = InputParser::new();

    loop {
        let cmd = {
            let _raw = tty::RawMode::enter().context("entering raw mode")?;
            let stdout = io::stdout();
            let mut out = BufWriter::new(stdout.lock());
            tty::enter_alt_screen(&mut out)?;

            let result = event_loop(&mut model, &mut parser, &mut out);

            let _ = tty::leave_alt_screen(&mut out);
            let _ = out.flush();

            result?
        };

        match cmd {
            Command::Attach { name, force } => {
                // Pre-flight: refresh and verify the session is still
                // present and (for force=false) not already attached.
                // `shpool attach` reports "already has a terminal
                // attached" on stderr with exit 0, and capturing
                // stderr requires piping it — which breaks shpool's
                // own detach detection (it checks isatty on stderr).
                // So we check the attached flag here instead. If it
                // raced into Attached since the keystroke, fall into
                // the force-confirm prompt rather than silently
                // no-opping.
                refresh_sessions(&mut model);
                let Some(session) = model.sessions.iter().find(|s| s.name == name) else {
                    model.set_error(format!("session '{name}' is gone"));
                    continue;
                };
                if !force && session.attached {
                    model.mode = Mode::ConfirmForce(name);
                    continue;
                }
                let ok = shell_attach(&name, force)?;
                let err_msg = if force {
                    format!("shpool attach -f {name} failed")
                } else {
                    format!("shpool attach {name} failed")
                };
                finish_action(&mut model, &name, ok, err_msg);
            }
            Command::Create(name) => {
                // Pre-flight: reject names that already exist. `shpool
                // attach` is create-or-attach, so without this check a
                // duplicate name silently attaches (or flashes "already
                // has a terminal attached" on stderr and no-ops) —
                // neither is what the create prompt implies.
                refresh_sessions(&mut model);
                if model.sessions.iter().any(|s| s.name == name) {
                    model.set_error(format!("session '{name}' already exists"));
                    continue;
                }
                let ok = shell_attach(&name, false)?;
                finish_action(&mut model, &name, ok, format!("shpool attach {name} failed"));
            }
            Command::Kill(name) => {
                refresh_sessions(&mut model);
                if !model.sessions.iter().any(|s| s.name == name) {
                    model.set_error(format!("session '{name}' is gone"));
                    continue;
                }
                let output = process::Command::new("shpool")
                    .args(["kill", &name])
                    .output()
                    .context("running `shpool kill`")?;
                let ok = output.status.success();
                let err_msg = if ok {
                    String::new()
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let detail = stderr.trim();
                    if detail.is_empty() {
                        format!("kill {name} failed")
                    } else {
                        format!("kill {name}: {detail}")
                    }
                };
                finish_action(&mut model, &name, ok, err_msg);
            }
            Command::Quit => return Ok(()),
        }
    }
}

/// Spawn `shpool attach <name>`, letting the child take over the TTY.
/// Used for both Attach and Create (a name shpool doesn't know is
/// created on the fly). Clears our rendered frame first so the
/// user's freshly-attached shell starts on a clean viewport.
fn shell_attach(name: &str, force: bool) -> Result<bool> {
    tty::clear_screen(&mut io::stdout())?;
    let mut cmd = process::Command::new("shpool");
    cmd.arg("attach");
    if force {
        cmd.arg("-f");
    }
    cmd.arg(name);
    let status = cmd.status().context("spawning `shpool attach`")?;
    Ok(status.success())
}

/// Post-action tail shared by attach/create/kill: refresh the session
/// list, reselect the target by name if it's still there, and park an
/// error message if the action failed.
fn finish_action(model: &mut Model, name: &str, ok: bool, err_msg: String) {
    refresh_sessions(model);
    if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
        model.selected = i;
    }
    if !ok {
        model.set_error(err_msg);
    }
}

fn event_loop<W: Write>(
    model: &mut Model,
    parser: &mut InputParser,
    out: &mut W,
) -> Result<Command> {
    let mut buf = [0u8; 16];

    loop {
        let (w, h) = tty::tty_size().unwrap_or((80, 24));
        tui::render(model, w, h, out)?;
        out.flush()?;

        match tty::read_stdin(&mut buf) {
            Ok(0) => return Ok(Command::Quit),
            Ok(n) => {
                // One read can contain multiple keystrokes (e.g. "jj\r"
                // arrives as a single buffer). Decode into a key stream
                // and feed keys one-at-a-time into update — same shape
                // as the crossterm-backed version, just without the
                // per-key read.
                let mut keys = Vec::new();
                parser.feed(&buf[..n], &mut keys);
                for key in keys {
                    if let Some(cmd) = tui::update(model, Event::Key(key)) {
                        return Ok(cmd);
                    }
                }
                // Auto-refresh lives here in main.rs for now — in a
                // follow-up commit it moves into update() as
                // Command::Refresh, so the daemon-call policy lives
                // alongside the rest of the dispatch rules.
                if matches!(model.mode, Mode::Normal) {
                    refresh_sessions(model);
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
