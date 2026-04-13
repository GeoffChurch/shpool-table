use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ListReply {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
pub struct Session {
    pub name: String,
    pub status: SessionStatus,
    pub started_at_unix_ms: u64,
    pub last_connected_at_unix_ms: u64,
    /// `None` if the session has never been detached from since it
    /// was created (still on its first attach).
    pub last_disconnected_at_unix_ms: Option<u64>,
}

impl Session {
    /// Unix ms of the most recent state transition — the newer of
    /// last-connected and last-disconnected, falling back to
    /// creation time. Used for "last-active" sorting.
    pub fn last_active_unix_ms(&self) -> u64 {
        self.last_connected_at_unix_ms
            .max(self.last_disconnected_at_unix_ms.unwrap_or(0))
            .max(self.started_at_unix_ms)
    }
}

/// The status reported by `shpool list --json`. We don't display it
/// (shpool's daemon takes up to ~1s to mark a session Disconnected
/// after detach, so the label would flash stale), but we do use
/// `Attached` in the pre-flight check to refuse re-attaching a
/// session that shpool would reject with "already has a terminal
/// attached" (which shpool reports on stderr with exit 0).
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Attached,
    Disconnected,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shpool_list_json() {
        let json = r#"{
            "sessions": [
                {
                    "name": "main",
                    "started_at_unix_ms": 1000,
                    "last_connected_at_unix_ms": 2000,
                    "last_disconnected_at_unix_ms": null,
                    "status": "Attached"
                },
                {
                    "name": "build",
                    "started_at_unix_ms": 500,
                    "last_connected_at_unix_ms": 500,
                    "last_disconnected_at_unix_ms": 1500,
                    "status": "Disconnected"
                }
            ]
        }"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.sessions.len(), 2);
        assert_eq!(reply.sessions[0].name, "main");
        assert_eq!(reply.sessions[0].status, SessionStatus::Attached);
        assert_eq!(reply.sessions[0].last_active_unix_ms(), 2000);
        assert_eq!(reply.sessions[1].name, "build");
        assert_eq!(reply.sessions[1].last_active_unix_ms(), 1500);
    }

    #[test]
    fn parse_unknown_status() {
        let json = r#"{
            "sessions": [{
                "name": "x",
                "started_at_unix_ms": 0,
                "last_connected_at_unix_ms": 0,
                "last_disconnected_at_unix_ms": null,
                "status": "Frozen"
            }]
        }"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.sessions[0].status, SessionStatus::Unknown);
    }

    #[test]
    fn parse_empty_sessions() {
        let json = r#"{"sessions": []}"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert!(reply.sessions.is_empty());
    }

    #[test]
    fn parse_ignores_extra_fields() {
        let json = r#"{
            "sessions": [{
                "name": "x",
                "started_at_unix_ms": 0,
                "last_connected_at_unix_ms": 0,
                "last_disconnected_at_unix_ms": null,
                "status": "Attached",
                "extra": true
            }],
            "unknown_top_level": 42
        }"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.sessions.len(), 1);
    }
}
