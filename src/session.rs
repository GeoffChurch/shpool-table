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
                {"name": "main", "started_at_unix_ms": 1234, "status": "Attached"},
                {"name": "build", "started_at_unix_ms": 5678, "status": "Disconnected"}
            ]
        }"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.sessions.len(), 2);
        assert_eq!(reply.sessions[0].name, "main");
        assert_eq!(reply.sessions[0].status, SessionStatus::Attached);
        assert_eq!(reply.sessions[1].name, "build");
        assert_eq!(reply.sessions[1].status, SessionStatus::Disconnected);
    }

    #[test]
    fn parse_unknown_status() {
        let json = r#"{
            "sessions": [{"name": "x", "started_at_unix_ms": 0, "status": "Frozen"}]
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
            "sessions": [{"name": "x", "started_at_unix_ms": 0, "status": "Attached", "extra": true}],
            "unknown_top_level": 42
        }"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.sessions.len(), 1);
    }
}
