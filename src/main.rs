mod session;
mod tty;
mod tui;

use std::io::{self, BufWriter, Write};
use std::process::Command;

use anyhow::{Context, Result};

use crate::session::{ListReply, Session};
use crate::tui::{InputParser, LoopAction, Mode, Model};

fn fetch_sessions() -> Result<Vec<Session>> {
    let out = Command::new("shpool")
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
    let mut model = Model::new(Vec::new());
    refresh_sessions(&mut model);
    run_tui(model)
}

fn run_tui(mut model: Model) -> Result<()> {
    tty::install_sigwinch_handler().context("installing SIGWINCH handler")?;

    let mut parser = InputParser::new();

    loop {
        let action = {
            let _raw = tty::RawMode::enter().context("entering raw mode")?;
            let stdout = io::stdout();
            let mut out = BufWriter::new(stdout.lock());
            tty::enter_alt_screen(&mut out)?;

            let result = event_loop(&mut model, &mut parser, &mut out);

            let _ = tty::leave_alt_screen(&mut out);
            let _ = out.flush();

            result?
        };

        match action {
            LoopAction::Attach(name) => {
                // Pre-flight: refresh and verify the session is still
                // present and not already attached. `shpool attach`
                // reports "already has a terminal attached" on stderr
                // with exit 0, and capturing stderr requires piping it
                // — which breaks shpool's own detach detection (it
                // checks isatty on stderr). So we check the attached
                // flag here instead. Known false-positive: ~1s after
                // your own detach, the daemon still reports Attached
                // and we'd refuse a valid re-attach. Retry works.
                refresh_sessions(&mut model);
                let Some(session) = model.sessions.iter().find(|s| s.name == name) else {
                    model.set_error(format!("session '{name}' is gone"));
                    continue;
                };
                if session.attached {
                    model.set_error(format!("'{name}' already attached elsewhere"));
                    continue;
                }
                let ok = shell_attach(&name)?;
                finish_action(&mut model, &name, ok, format!("shpool attach {name} failed"));
            }
            LoopAction::Create(name) => {
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
                let ok = shell_attach(&name)?;
                finish_action(&mut model, &name, ok, format!("shpool attach {name} failed"));
            }
            LoopAction::Kill(name) => {
                refresh_sessions(&mut model);
                if !model.sessions.iter().any(|s| s.name == name) {
                    model.set_error(format!("session '{name}' is gone"));
                    continue;
                }
                let output = Command::new("shpool")
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
            LoopAction::Quit => return Ok(()),
        }
    }
}

/// Spawn `shpool attach <name>`, letting the child take over the TTY.
/// Used for both Attach and Create (a name shpool doesn't know is
/// created on the fly). Clears our rendered frame first so the
/// user's freshly-attached shell starts on a clean viewport.
fn shell_attach(name: &str) -> Result<bool> {
    tty::clear_screen(&mut io::stdout())?;
    let status = Command::new("shpool")
        .args(["attach", name])
        .status()
        .context("spawning `shpool attach`")?;
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
) -> Result<LoopAction> {
    let mut buf = [0u8; 16];

    loop {
        let (w, h) = tty::tty_size().unwrap_or((80, 24));
        tui::render(model, w, h, out)?;
        out.flush()?;

        match tty::read_stdin(&mut buf) {
            Ok(0) => return Ok(LoopAction::Quit),
            Ok(n) => {
                if let Some(action) = tui::process_input(&buf[..n], model, parser) {
                    return Ok(action);
                }
                // Pick up sessions added or removed by other clients
                // since the last keypress. Skipped in modal modes so
                // typing into the create-name prompt isn't a per-keystroke
                // `shpool list` storm.
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
