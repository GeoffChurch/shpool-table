//! Pure template helpers for the vars view.
//!
//! A session-name template like `{workspace}-edit` embeds `{name}`
//! tokens that resolve against the daemon's template variables. These
//! helpers extract those tokens, resolve a template against a value
//! map, and find which live attachments a given variable governs — the
//! set that would re-dial if the variable's value changed.
//!
//! The token grammar matches shperl's `\{(\w+)\}`: a `{`, one or more
//! identifier characters (`[A-Za-z0-9_]`), then a `}`. This admits a
//! leading digit, unlike libshpool's stricter rule, but the two agree
//! on every real variable name (libshpool rejects leading-digit names
//! at `var set` time), so we match shperl. Scanning is done by hand —
//! no `regex` dependency.

use std::collections::HashMap;

use crate::session::Session;

/// True for the identifier characters that may appear inside a `{name}`
/// token — the bytes Perl's `\w` matches in the ASCII range.
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A piece of a scanned template: either a run of literal text or a
/// `{name}` token (carrying just the inner name).
enum Segment<'a> {
    Literal(&'a str),
    Token(&'a str),
}

/// Split `tmpl` into literal runs and `{name}` tokens, left to right. A
/// `{` that doesn't open a well-formed `{ident+}` token stays in a
/// literal run (matching Perl's substitution, which leaves non-matching
/// text untouched).
fn scan_template(tmpl: &str) -> Vec<Segment<'_>> {
    let bytes = tmpl.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0;
    let mut literal_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        // Read the identifier run after the brace.
        let name_start = i + 1;
        let mut j = name_start;
        while j < bytes.len() && is_ident_char(bytes[j] as char) {
            j += 1;
        }
        // A token needs at least one identifier char and a closing brace.
        if j > name_start && j < bytes.len() && bytes[j] == b'}' {
            if literal_start < i {
                segments.push(Segment::Literal(&tmpl[literal_start..i]));
            }
            segments.push(Segment::Token(&tmpl[name_start..j]));
            i = j + 1;
            literal_start = i;
        } else {
            // Not a token — leave the brace as literal text.
            i += 1;
        }
    }
    if literal_start < bytes.len() {
        segments.push(Segment::Literal(&tmpl[literal_start..]));
    }
    segments
}

/// Variable names referenced by a template: each `{name}` token in
/// first-seen order, de-duplicated. `"{a}-{b}-{a}"` -> `["a", "b"]`;
/// `"plainsess"` -> `[]`.
pub fn template_vars(tmpl: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for seg in scan_template(tmpl) {
        if let Segment::Token(name) = seg {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Resolve a template against a `name -> value` map: each `{name}`
/// becomes its value; an unknown name is left as the literal `{name}`.
pub fn resolve_template(tmpl: &str, vars: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(tmpl.len());
    for seg in scan_template(tmpl) {
        match seg {
            Segment::Literal(lit) => out.push_str(lit),
            Segment::Token(name) => match vars.get(name) {
                Some(value) => out.push_str(value),
                None => {
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
            },
        }
    }
    out
}

/// One attachment whose template references the queried variable — the
/// set that would re-dial if the variable changed. Borrows from the
/// session slice rather than copying.
#[derive(Debug, PartialEq)]
pub struct GovernedAttachment<'a> {
    /// The session the attachment currently resolves to.
    pub session: &'a str,
    /// The template it dialed in with.
    pub template: &'a str,
    /// The attach-proc pid.
    pub pid: u64,
}

/// Attachments across all sessions whose template references `var`.
pub fn attachments_for_var<'a>(sessions: &'a [Session], var: &str) -> Vec<GovernedAttachment<'a>> {
    let mut hits = Vec::new();
    for s in sessions {
        for a in &s.attachments {
            if template_vars(&a.session_name_template)
                .iter()
                .any(|n| n == var)
            {
                hits.push(GovernedAttachment {
                    session: &s.name,
                    template: &a.session_name_template,
                    pid: a.pid,
                });
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Attachment;

    #[test]
    fn template_vars_extracts_tokens_deduped_in_order() {
        assert_eq!(template_vars("{workspace}-edit"), vec!["workspace"]);
        assert_eq!(template_vars("{a}-{b}-{a}"), vec!["a", "b"]);
        assert_eq!(template_vars("plainsess"), Vec::<String>::new());
    }

    #[test]
    fn resolve_template_substitutes_known_leaves_unknown_literal() {
        let vars: HashMap<&str, &str> =
            HashMap::from([("workspace", "newproj"), ("editor", "vim")]);
        assert_eq!(resolve_template("{workspace}-edit", &vars), "newproj-edit");
        assert_eq!(
            resolve_template("{workspace}-{editor}", &vars),
            "newproj-vim"
        );
        assert_eq!(resolve_template("{gone}-x", &vars), "{gone}-x");
    }

    fn attached(name: &str, template: &str, pid: u64) -> Session {
        Session {
            name: name.to_string(),
            attached: true,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
            attachments: vec![Attachment {
                session_name_template: template.to_string(),
                pid,
            }],
        }
    }

    fn vars_sessions() -> Vec<Session> {
        vec![
            attached("myproj-edit", "{workspace}-edit", 111),
            attached("myproj-term", "{workspace}-term", 222),
            attached("vim-notes", "{editor}-notes", 333),
            attached("plainsess", "plainsess", 444),
        ]
    }

    #[test]
    fn attachments_for_var_finds_only_referencing_attachments() {
        let sessions = vars_sessions();
        let hits = attachments_for_var(&sessions, "workspace");
        assert_eq!(hits.len(), 2);
        let mut names: Vec<&str> = hits.iter().map(|h| h.session).collect();
        names.sort();
        assert_eq!(names, vec!["myproj-edit", "myproj-term"]);
        let mut pids: Vec<u64> = hits.iter().map(|h| h.pid).collect();
        pids.sort();
        assert_eq!(pids, vec![111, 222]);
        assert_eq!(attachments_for_var(&sessions, "editor").len(), 1);
        assert_eq!(attachments_for_var(&sessions, "nope").len(), 0);
    }
}
