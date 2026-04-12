use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ListReply {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
pub struct Session {
    pub name: String,
    pub status: SessionStatus,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Attached,
    Disconnected,
    #[serde(other)]
    Unknown,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Attached => "attached",
            SessionStatus::Disconnected => "disconnected",
            SessionStatus::Unknown => "unknown",
        }
    }
}
