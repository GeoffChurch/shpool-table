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

/// Variables a template references that aren't in `known` — the names a
/// create-time prompt has to ask for. `template_vars(name)` minus the
/// `known` set, first-seen order preserved (so the prompt walks them in
/// the order they appear in the name). Pure and `Var`-free, like
/// `template_vars`; the caller supplies the set of already-known names
/// (the keys of the resolution map). A var set to the empty string still
/// counts as known — membership, not truthiness — so it is never
/// re-prompted.
pub fn unknown_template_vars(name: &str, known: &std::collections::HashSet<&str>) -> Vec<String> {
    template_vars(name)
        .into_iter()
        .filter(|n| !known.contains(n.as_str()))
        .collect()
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

/// Candidate values for `target`: values that, substituted for `target`
/// (the other variables pinned to their current values in `vars`), make
/// one of `target`'s templates resolve to a session name that currently
/// exists. `vars` is the `name -> value` map `resolve_template` consumes.
///
/// Templates come only from currently-attached sessions (`attachments`),
/// but the match scans ALL session names — a detached session still
/// exists by name and is a valid re-dial target. The capture is a
/// prefix/suffix strip (`strip_prefix`/`strip_suffix`, char-safe and
/// never panicking), so it's correct on multibyte names. Returns the
/// current value first, then the harvested captures, de-duplicated.
pub fn candidate_values(
    sessions: &[Session],
    vars: &HashMap<&str, &str>,
    target: &str,
) -> Vec<String> {
    let current = vars.get(target).copied().unwrap_or("").to_string();

    // Distinct templates (from attachments) that mention {target}.
    let mut templates: Vec<&str> = Vec::new();
    for s in sessions {
        for a in &s.attachments {
            let tmpl = a.session_name_template.as_str();
            if template_vars(tmpl).iter().any(|n| n == target) && !templates.contains(&tmpl) {
                templates.push(tmpl);
            }
        }
    }

    let mut cands: Vec<String> = vec![current.clone()];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(current);

    for tmpl in templates {
        // Locate the single {target} token among the scanned segments;
        // skip the template if it appears zero or 2+ times. Pin every
        // other token to its current value (unknown co-var stays literal
        // {name}, per resolve_template), collapsing T to
        // prefix + {target} + suffix.
        let segments = scan_template(tmpl);
        let target_count = segments
            .iter()
            .filter(|seg| matches!(seg, Segment::Token(name) if *name == target))
            .count();
        if target_count != 1 {
            continue;
        }
        let mut prefix = String::new();
        let mut suffix = String::new();
        let mut seen_target = false;
        for seg in &segments {
            let into = if seen_target {
                &mut suffix
            } else {
                &mut prefix
            };
            match seg {
                Segment::Literal(lit) => into.push_str(lit),
                Segment::Token(name) if *name == target => seen_target = true,
                Segment::Token(name) => match vars.get(name) {
                    Some(value) => into.push_str(value),
                    None => {
                        into.push('{');
                        into.push_str(name);
                        into.push('}');
                    }
                },
            }
        }

        for s in sessions {
            // Strip the prefix off the front and the suffix off the back;
            // the remainder is the captured value. strip_* is char-safe
            // and won't panic on a multibyte boundary.
            let Some(rest) = s.name.strip_prefix(&prefix) else {
                continue;
            };
            let Some(cap) = rest.strip_suffix(&suffix) else {
                continue;
            };
            if cap.is_empty() {
                continue; // empty capture dropped
            }
            if seen.insert(cap.to_string()) {
                cands.push(cap.to_string());
            }
        }
    }
    cands
}

/// ASCII-lowercase a string: `A`–`Z` fold to `a`–`z`, every other byte
/// (including multibyte UTF-8) is left untouched. Matches shperl's
/// `tr/A-Z/a-z/`, so folding is identical across the two ports.
fn ascii_fold(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Filter + rank `candidates` against `query`. Keep candidates whose
/// ASCII-folded form has the ASCII-folded query as a subsequence; order
/// by a total key: (1) exact match, (2) contiguous (folded candidate
/// *contains* folded query) before scattered, (3) earliest first-match
/// char index, (4) fewer characters (`chars().count()`), (5) harvest
/// index. An empty query keeps everything in harvest order. All
/// comparisons are on folded character forms, so the ranking is
/// identical regardless of byte width.
pub fn filter_rank(candidates: &[String], query: &str) -> Vec<String> {
    if query.is_empty() {
        return candidates.to_vec();
    }

    let folded_query = ascii_fold(query);
    let query_chars: Vec<char> = folded_query.chars().collect();
    let first_query = query_chars[0];

    // The total-order sort key from the doc, in priority order: exact
    // match, contiguous-before-scattered, earliest first-match char
    // index, fewer characters, harvest index.
    type RankKey = (u8, u8, usize, usize, usize);

    let mut ranked: Vec<(RankKey, &String)> = Vec::new();
    for (i, cand) in candidates.iter().enumerate() {
        let folded: Vec<char> = ascii_fold(cand).chars().collect();

        // Subsequence test on folded chars; also record the index of the
        // first candidate char equal to the first query char.
        let mut first = usize::MAX;
        let mut qi = 0usize;
        for (ci, &fc) in folded.iter().enumerate() {
            if first == usize::MAX && fc == first_query {
                first = ci;
            }
            if qi < query_chars.len() && fc == query_chars[qi] {
                qi += 1;
            }
        }
        if qi != query_chars.len() {
            continue; // not a subsequence
        }

        let folded_str: String = folded.iter().collect();
        let exact = u8::from(folded_str != folded_query);
        let contiguous = u8::from(!folded_str.contains(&folded_query));
        ranked.push(((exact, contiguous, first, folded.len(), i), cand));
    }
    ranked.sort_by_key(|(key, _)| *key);
    ranked.into_iter().map(|(_, c)| c.clone()).collect()
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

    /// Build a `known` set from a slice of names, for
    /// `unknown_template_vars`.
    fn known<'a>(names: &[&'a str]) -> std::collections::HashSet<&'a str> {
        names.iter().copied().collect()
    }

    #[test]
    fn unknown_template_vars_tokens_minus_known_order_preserved() {
        // Ports shperl's `unknown_template_vars` subtest: 0/1/many tokens,
        // some/all known, repeated, order.
        assert_eq!(
            unknown_template_vars("plainsess", &known(&[])),
            Vec::<String>::new(),
            "no tokens -> nothing unknown"
        );
        assert_eq!(
            unknown_template_vars("{a}-x", &known(&[])),
            vec!["a"],
            "one token, none known"
        );
        assert_eq!(
            unknown_template_vars("{a}-{b}-{c}", &known(&[])),
            vec!["a", "b", "c"],
            "many tokens, first-seen order"
        );
        assert_eq!(
            unknown_template_vars("{a}-{b}-{c}", &known(&["b"])),
            vec!["a", "c"],
            "a known middle var is dropped, the rest keep order"
        );
        assert_eq!(
            unknown_template_vars("{a}-{b}", &known(&["a", "b"])),
            Vec::<String>::new(),
            "all known -> nothing to prompt"
        );
        assert_eq!(
            unknown_template_vars("{a}-{b}-{a}", &known(&[])),
            vec!["a", "b"],
            "a repeated token is de-duped (template_vars order)"
        );
        // A var set to the empty string still counts as known
        // (membership, not truthiness) — must not be re-prompted. The
        // detect-time set carries the name regardless of its value.
        assert_eq!(
            unknown_template_vars("{a}-x", &known(&["a"])),
            Vec::<String>::new(),
            "a set-but-empty var is known, not prompted"
        );
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

    // -- candidate_values --

    /// A detached session: exists by name, carries no attachments, so it
    /// contributes a template to nobody but is still a valid match target.
    fn detached(name: &str) -> Session {
        Session {
            name: name.to_string(),
            attached: false,
            started_at_unix_ms: 0,
            last_connected_at_unix_ms: 0,
            last_disconnected_at_unix_ms: None,
            attachments: Vec::new(),
        }
    }

    fn vmap<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn candidate_values_single_template_strips_prefix_suffix() {
        // demo-edit is detached but still a valid target; noise doesn't
        // fit the prefix/suffix.
        let sessions = vec![
            attached("myproj-edit", "{workspace}-edit", 1),
            detached("demo-edit"),
            detached("noise"),
        ];
        assert_eq!(
            candidate_values(&sessions, &vmap(&[("workspace", "myproj")]), "workspace"),
            vec!["myproj", "demo"],
        );
    }

    #[test]
    fn candidate_values_unions_across_a_variables_templates() {
        let sessions = vec![
            attached("a-edit", "{w}-edit", 1),
            attached("b-term", "{w}-term", 2),
            detached("c-edit"),
        ];
        // Captures unioned across {w}-edit (a, c) then {w}-term (b),
        // current first.
        assert_eq!(
            candidate_values(&sessions, &vmap(&[("w", "a")]), "w"),
            vec!["a", "c", "b"],
        );
    }

    #[test]
    fn candidate_values_pins_co_vars_to_current_values() {
        let sessions = vec![
            attached("vim-myproj-edit", "{editor}-{workspace}-edit", 1),
            detached("vim-demo-edit"),
            detached("nano-other-edit"), // editor != vim -> excluded
        ];
        assert_eq!(
            candidate_values(
                &sessions,
                &vmap(&[("editor", "vim"), ("workspace", "myproj")]),
                "workspace",
            ),
            vec!["myproj", "demo"],
        );
    }

    #[test]
    fn candidate_values_delimiter_bearing_co_var_pins_literally() {
        // editor="a-b" makes the pinned prefix "a-b-"; only names with
        // that exact prefix contribute, on the literal string.
        let sessions = vec![
            attached("a-b-myproj", "{editor}-{workspace}", 1),
            detached("a-b-demo"),
            detached("a-other"), // prefix "a-b-" mismatch
        ];
        assert_eq!(
            candidate_values(
                &sessions,
                &vmap(&[("editor", "a-b"), ("workspace", "myproj")]),
                "workspace",
            ),
            vec!["myproj", "demo"],
        );
    }

    #[test]
    fn candidate_values_bare_token_captures_every_name() {
        let sessions = vec![
            attached("alpha", "{x}", 1),
            detached("beta"),
            detached("gamma"),
        ];
        // Bare {x}: prefix and suffix both empty -> all names.
        assert_eq!(
            candidate_values(&sessions, &vmap(&[("x", "alpha")]), "x"),
            vec!["alpha", "beta", "gamma"],
        );
    }

    #[test]
    fn candidate_values_no_attached_template_yields_current_only() {
        let sessions = vec![attached("myproj-edit", "{workspace}-edit", 1)];
        // No template references {gone} -> just the current value.
        assert_eq!(
            candidate_values(&sessions, &vmap(&[("gone", "cur")]), "gone"),
            vec!["cur"],
        );
    }

    #[test]
    fn candidate_values_multibyte_name_is_char_safe() {
        // café-edit / naïve-edit: the strip works on characters, never
        // panicking on a byte boundary inside a multibyte char.
        let sessions = vec![attached("café-edit", "{w}-edit", 1), detached("naïve-edit")];
        let c = candidate_values(&sessions, &vmap(&[("w", "café")]), "w");
        assert_eq!(c, vec!["café", "naïve"]);
        assert_eq!(c[1].chars().count(), 5, "naïve is 5 characters");
    }

    #[test]
    fn candidate_values_empty_capture_is_dropped() {
        // A session named exactly "-edit" would capture "" under
        // {w}-edit; that empty capture is dropped, "real" kept.
        let sessions = vec![attached("-edit", "{w}-edit", 1), detached("real-edit")];
        assert_eq!(
            candidate_values(&sessions, &vmap(&[("w", "cur")]), "w"),
            vec!["cur", "real"],
        );
    }

    #[test]
    fn candidate_values_repeated_token_template_is_skipped() {
        let sessions = vec![attached("a-a", "{v}-{v}", 1), attached("x-y", "{v}-y", 2)];
        // {v}-{v} skipped (token appears twice); {v}-y still captures x.
        assert_eq!(
            candidate_values(&sessions, &vmap(&[("v", "a")]), "v"),
            vec!["a", "x"],
        );
    }

    // -- filter_rank --

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn filter_rank_ascii_case_insensitive_subsequence() {
        // Folded query matches a folded candidate; non-match dropped.
        assert_eq!(filter_rank(&strs(&["XRP", "abc"]), "xrp"), vec!["XRP"]);
    }

    #[test]
    fn filter_rank_exact_match_sorts_to_top() {
        // The exact candidate outranks the longer subsequence matches.
        assert_eq!(
            filter_rank(&strs(&["xrpz", "xrp", "xrpa"]), "xrp"),
            vec!["xrp", "xrpz", "xrpa"],
        );
    }

    #[test]
    fn filter_rank_contiguous_before_scattered() {
        // xrp (contains "xr") outranks xmr (scattered); djt drops out.
        assert_eq!(
            filter_rank(&strs(&["xmr", "xrp", "djt"]), "xr"),
            vec!["xrp", "xmr"],
        );
    }

    #[test]
    fn filter_rank_sparse_subsequence_matches() {
        // k...b is a subsequence of key-bugfix; unrelated drops out.
        assert_eq!(
            filter_rank(&strs(&["key-bugfix", "unrelated"]), "kb"),
            vec!["key-bugfix"],
        );
    }

    #[test]
    fn filter_rank_empty_query_keeps_harvest_order() {
        assert_eq!(
            filter_rank(&strs(&["c", "a", "b"]), ""),
            vec!["c", "a", "b"],
        );
    }

    #[test]
    fn filter_rank_harvest_index_is_final_tiebreak() {
        // Both contiguous, first-match index 0, same char count -> input
        // order preserved by the harvest-index tiebreak.
        assert_eq!(
            filter_rank(&strs(&["abx", "aby"]), "ab"),
            vec!["abx", "aby"],
        );
    }

    #[test]
    fn filter_rank_length_compared_by_chars_not_bytes() {
        // Both contain folded "x"; "Xé" is 2 chars, "aXc" is 3 chars, so
        // the multibyte 2-char candidate sorts first.
        assert_eq!(filter_rank(&strs(&["aXc", "Xé"]), "x"), vec!["Xé", "aXc"]);
    }
}
