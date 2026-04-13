use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ListReply {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
pub struct Session {
    pub name: String,
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
        assert_eq!(reply.sessions[1].name, "build");
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
