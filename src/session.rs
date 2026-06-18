use serde::{Deserialize, Deserializer};

#[derive(Debug, Deserialize)]
pub struct ListReply {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
pub struct Session {
    pub name: String,
    /// True iff shpool reports this session as currently attached to
    /// another terminal. We don't care about the wire's other status
    /// values (Disconnected, future variants) — only Attached drives
    /// behavior (the attach pre-flight refusal + the row's status dot).
    #[serde(rename = "status", deserialize_with = "deserialize_attached")]
    pub attached: bool,
    pub started_at_unix_ms: u64,
    pub last_connected_at_unix_ms: u64,
    /// `None` if the session has never been detached from since it
    /// was created (still on its first attach).
    pub last_disconnected_at_unix_ms: Option<u64>,
    /// Live attach processes on this session, each carrying the
    /// `session_name_template` it dialed in with. Only attached sessions
    /// report any; a detached session reports an empty list. `default`
    /// so list replies that omit the field (and the existing parse-test
    /// fixtures) still deserialize. Drives the vars view's preview of
    /// which attachments a template variable governs.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// A single live attach process and the template it dialed in with. The
/// resolved session name alone can't reveal which variable produced it;
/// the template (`{workspace}-edit`) can.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Attachment {
    pub session_name_template: String,
    /// shpool pids are positive; rendered verbatim in the preview.
    pub pid: u64,
}

impl Session {
    /// Unix ms of the most recent time this session was touched — the
    /// newest of last-connected, last-disconnected, and creation time.
    /// Drives the most-recent-first sort. Named "touched" rather than
    /// "active" because a brand-new session (only `started_at` set)
    /// reads as freshly touched at creation, not as currently active.
    pub fn last_touched_unix_ms(&self) -> u64 {
        self.last_connected_at_unix_ms
            .max(self.last_disconnected_at_unix_ms.unwrap_or(0))
            .max(self.started_at_unix_ms)
    }
}

fn deserialize_attached<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = String::deserialize(d)?;
    Ok(s == "Attached")
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
        assert!(reply.sessions[0].attached);
        assert_eq!(reply.sessions[0].last_touched_unix_ms(), 2000);
        assert_eq!(reply.sessions[1].name, "build");
        assert!(!reply.sessions[1].attached);
        assert_eq!(reply.sessions[1].last_touched_unix_ms(), 1500);
    }

    #[test]
    fn parse_unknown_status_is_not_attached() {
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
        assert!(!reply.sessions[0].attached);
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

    #[test]
    fn parse_attachments() {
        // An attached session carries its attach procs' templates; a
        // session that omits the field deserializes to an empty list
        // (the `#[serde(default)]` path).
        let json = r#"{
            "sessions": [
                {
                    "name": "myproj-edit",
                    "started_at_unix_ms": 0,
                    "last_connected_at_unix_ms": 0,
                    "last_disconnected_at_unix_ms": null,
                    "status": "Attached",
                    "attachments": [
                        { "session_name_template": "{workspace}-edit", "pid": 111 },
                        { "session_name_template": "{workspace}-edit", "pid": 222 }
                    ]
                },
                {
                    "name": "detached",
                    "started_at_unix_ms": 0,
                    "last_connected_at_unix_ms": 0,
                    "last_disconnected_at_unix_ms": null,
                    "status": "Disconnected"
                }
            ]
        }"#;
        let reply: ListReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.sessions[0].attachments.len(), 2);
        assert_eq!(
            reply.sessions[0].attachments[0],
            Attachment {
                session_name_template: "{workspace}-edit".into(),
                pid: 111,
            }
        );
        assert_eq!(reply.sessions[0].attachments[1].pid, 222);
        // Field omitted entirely -> empty via serde(default).
        assert!(reply.sessions[1].attachments.is_empty());
    }
}
