mod session;
mod tty;
mod tui;

use std::io::{self, BufWriter, Read, Write};
use std::process::Command;

use anyhow::{Context, Result};

use crate::session::{ListReply, Session};
use crate::tui::{InputParser, Key, Model};

enum LoopAction {
    Attach(String),
    Quit,
}

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

    if !tty::is_interactive() {
        for s in &sessions {
            println!("{}\t{}", s.name, s.status.as_str());
        }
        return Ok(());
    }

    run_tui(sessions)
}

fn run_tui(initial: Vec<Session>) -> Result<()> {
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

                let new_sessions = fetch_sessions()?;
                model = Model::new(new_sessions);
                if let Some(i) = model.sessions.iter().position(|s| s.name == name) {
                    model.selected = i;
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
    let mut stdin = io::stdin();
    let mut buf = [0u8; 16];

    loop {
        let (w, h) = tty::tty_size().unwrap_or((80, 24));
        tui::render(model, w, h, out)?;
        out.flush()?;

        let n = stdin.read(&mut buf).context("reading stdin")?;
        if n == 0 {
            return Ok(LoopAction::Quit);
        }

        for &b in &buf[..n] {
            match parser.feed(b) {
                Some(Key::Up) => model.select_prev(),
                Some(Key::Down) => model.select_next(),
                Some(Key::Enter) => {
                    if let Some(name) = model.selected_name() {
                        return Ok(LoopAction::Attach(name.to_string()));
                    }
                }
                Some(Key::Quit) => return Ok(LoopAction::Quit),
                _ => {}
            }
        }
    }
}
