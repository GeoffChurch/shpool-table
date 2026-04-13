mod session;
mod tty;
mod tui;

use std::io::{self, BufWriter, Write};
use std::process::Command;

use anyhow::{Context, Result};

use crate::session::{ListReply, Session, SessionStatus};
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
                // checks isatty on stderr). So we check the status
                // field here instead. Known false-positive: ~1s after
                // your own detach, the daemon still reports Attached
                // and we'd refuse a valid re-attach. Retry works.
                refresh_sessions(&mut model);
                let Some(session) = model.sessions.iter().find(|s| s.name == name) else {
                    model.set_error(format!("session '{name}' is gone"));
                    continue;
                };
                if session.status == SessionStatus::Attached {
                    model.set_error(format!("'{name}' already attached elsewhere"));
                    continue;
                }
                tty::clear_screen(&mut io::stdout())?;
                let status = Command::new("shpool")
                    .args(["attach", &name])
                    .status()
                    .context("spawning `shpool attach`")?;
                refresh_sessions(&mut model);
                if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
                    model.selected = i;
                }
                if !status.success() {
                    model.set_error(format!("shpool attach {name} failed"));
                }
            }
            LoopAction::Create(name) => {
                // No existence pre-flight: the session doesn't exist
                // yet — that's the whole point. `shpool attach` on an
                // unknown name creates and attaches.
                tty::clear_screen(&mut io::stdout())?;
                let status = Command::new("shpool")
                    .args(["attach", &name])
                    .status()
                    .context("spawning `shpool attach`")?;
                refresh_sessions(&mut model);
                if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
                    model.selected = i;
                }
                if !status.success() {
                    model.set_error(format!("shpool attach {name} failed"));
                }
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
                refresh_sessions(&mut model);
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let detail = stderr.trim();
                    if detail.is_empty() {
                        model.set_error(format!("kill {name} failed"));
                    } else {
                        model.set_error(format!("kill {name}: {detail}"));
                    }
                }
            }
            LoopAction::Quit => return Ok(()),
        }
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
