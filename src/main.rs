use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ListReply {
    sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
struct Session {
    name: String,
    status: SessionStatus,
}

#[derive(Debug, Deserialize)]
enum SessionStatus {
    Attached,
    Disconnected,
    #[serde(other)]
    Unknown,
}

impl SessionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Attached => "attached",
            SessionStatus::Disconnected => "disconnected",
            SessionStatus::Unknown => "unknown",
        }
    }
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
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    for s in &sessions {
        println!("{}\t{}", s.name, s.status.as_str());
    }
    Ok(())
}
