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

fn main() -> Result<()> {
    let sessions = fetch_sessions()?;
    run_tui(sessions)
}

fn run_tui(initial: Vec<Session>) -> Result<()> {
    tty::install_sigwinch_handler().context("installing SIGWINCH handler")?;

    let mut model = Model::new(initial);
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
                Command::new("shpool")
                    .args(["attach", &name])
                    .status()
                    .context("spawning `shpool attach`")?;
                model.refresh(fetch_sessions()?);
                if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
                    model.selected = i;
                }
            }
            LoopAction::Kill(name) => {
                Command::new("shpool")
                    .args(["kill", &name])
                    .status()
                    .context("running `shpool kill`")?;
                model.refresh(fetch_sessions()?);
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
                    model.refresh(fetch_sessions()?);
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
