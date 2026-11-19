//! B14: session metadata, search and sort for the picker.
// ported from pi packages/coding-agent/src/core/session-manager.ts (buildSessionInfo) and modes/interactive/components/session-selector-search.ts
use crate::{SessionHeader, SessionMeta, SessionRef};
use oxa_proto::{Entry, EntryBody, Role};
use std::collections::HashMap;

/// Sort orders of the picker (pi's three modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSort {
    /// Most recently modified first (default).
    Recent,
    /// Most recently modified first, ignoring search rank.
    Threaded,
    /// Best search match first.
    Relevance,
}

/// The metadata of a session, built from its header and entries (see [`SessionMeta`]). The one
/// builder behind both stores' `list`: the last name wins (trimmed; blank clears it) and
/// `modified_ms` is the newest timestamp of the header and entries.
pub(crate) fn meta_of_parts(sref: SessionRef, header: &SessionHeader, entries: &[Entry]) -> SessionMeta {
    let mut name = None;
    let mut modified_ms = header.ts_ms;
    let mut message_count = 0u32;
    let mut first_message: Option<String> = None;
    let mut all_text = String::new();
    for e in entries {
        modified_ms = modified_ms.max(e.ts_ms);
        match &e.body {
            EntryBody::SessionInfo { name: n } => {
                name = Some(n.trim().to_string()).filter(|n| !n.is_empty());
            }
            EntryBody::Msg { wire, .. } if matches!(wire.role, Role::User | Role::Assistant) => {
                message_count += 1;
                let text = message_text(&e.body);
                if !all_text.is_empty() {
                    all_text.push('\n');
                }
                all_text.push_str(text);
                if wire.role == Role::User && first_message.is_none() && !text.trim().is_empty() {
                    first_message = Some(text.split_whitespace().collect::<Vec<_>>().join(" "));
                }
            }
            _ => {}
        }
    }
    SessionMeta {
        sref,
        cwd: header.cwd.clone(),
        name,
        created_ms: header.ts_ms,
        modified_ms,
        message_count,
        first_message: first_message.unwrap_or_default(),
        all_text,
        parent_session: header.parent_session.clone(),
    }
}

/// Display/search text shared by the metadata builder and both stores' listings.
/// Without provenance, a trailing note-shaped block may be user-authored and is preserved.
pub(crate) fn message_text(body: &EntryBody) -> &str {
    let EntryBody::Msg { wire, parts, .. } = body else {
        return "";
    };
    let text = wire.content.as_deref().unwrap_or("");
    if wire.role == Role::User {
        if let Some(note) = parts.iter().rev().find_map(|p| match p {
            oxa_proto::Part::EnvNote(note) => Some(note.as_str()),
            _ => None,
        }) {
            return if text == note {
                ""
            } else {
                text.strip_suffix(note)
                    .and_then(|typed| typed.strip_suffix("\n\n"))
                    .unwrap_or(text)
            };
        }
    }
    text
}

/// pi's picker search: space-separated terms all match (case-insensitive) in name, first
/// message, all text or path; `"quoted phrases"` match literally; `re:<regex>` is a regex.
/// `named_only` keeps sessions with a name. Returns indices into `metas` in rank order.
///
/// Rank: each term scores where it matches (name 4, first message 2, all text 1, path 1); the
/// score is the sum over terms, ties go to the more recently modified session. An empty query
/// ranks by recency. An invalid `re:` pattern matches nothing.
pub fn search_sessions(metas: &[SessionMeta], query: &str, named_only: bool) -> Vec<usize> {
    let terms = parse_query(query);
    let mut scored: Vec<(usize, u32)> = Vec::new();
    for (i, m) in metas.iter().enumerate() {
        if named_only && m.name.is_none() {
            continue;
        }
        let fields = Fields::of(m);
        let mut score = 0u32;
        let mut all_match = true;
        for t in &terms {
            match t.score(&fields) {
                Some(s) => score += s,
                None => {
                    all_match = false;
                    break;
                }
            }
        }
        if all_match {
            scored.push((i, score));
        }
    }
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| metas[b.0].modified_ms.cmp(&metas[a.0].modified_ms))
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Sort indices (as returned by [`search_sessions`]) by `sort`.
///
/// `Recent` orders by modification time, newest first. `Threaded` does the same for whole
/// threads: a session is placed directly after its parent (`parentSession` among the indices),
/// and each thread is ranked by its newest member. `Relevance` keeps the order of the search.
pub fn sort_sessions(metas: &[SessionMeta], indices: &mut Vec<usize>, sort: SessionSort) {
    match sort {
        SessionSort::Recent => {
            indices.sort_by(|&a, &b| metas[b].modified_ms.cmp(&metas[a].modified_ms));
        }
        SessionSort::Relevance => {}
        SessionSort::Threaded => {
            *indices = threaded(metas, indices);
        }
    }
}

/// Newest first, each session followed by its children (also newest first) when they are in `idx`.
fn threaded(metas: &[SessionMeta], idx: &[usize]) -> Vec<usize> {
    let by_path: HashMap<String, usize> = idx
        .iter()
        .map(|&i| (metas[i].sref.path.to_string_lossy().into_owned(), i))
        .collect();
    let parent_of = |i: usize| -> Option<usize> {
        metas[i]
            .parent_session
            .as_ref()
            .and_then(|p| by_path.get(p).copied())
            .filter(|&p| p != i)
    };
    // Newest modification in each subtree (a thread's rank).
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for &i in idx {
        match parent_of(i) {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut out = Vec::with_capacity(idx.len());
    let mut seen = vec![false; metas.len()];
    fn walk(
        i: usize,
        children: &HashMap<usize, Vec<usize>>,
        metas: &[SessionMeta],
        seen: &mut [bool],
        out: &mut Vec<usize>,
    ) {
        if seen[i] {
            return;
        }
        seen[i] = true;
        out.push(i);
        let mut kids = children.get(&i).cloned().unwrap_or_default();
        kids.sort_by(|&a, &b| metas[b].modified_ms.cmp(&metas[a].modified_ms));
        for k in kids {
            walk(k, children, metas, seen, out);
        }
    }
    // The rank of a root is the newest member of its subtree.
    fn subtree_newest(i: usize, children: &HashMap<usize, Vec<usize>>, metas: &[SessionMeta]) -> u64 {
        let mut best = metas[i].modified_ms;
        for &k in children.get(&i).map(Vec::as_slice).unwrap_or(&[]) {
            best = best.max(subtree_newest(k, children, metas));
        }
        best
    }
    roots.sort_by_key(|&a| std::cmp::Reverse(subtree_newest(a, &children, metas)));
    for r in roots {
        walk(r, &children, metas, &mut seen, &mut out);
    }
    // Members of a parent cycle are reached from no root; keep them at the end.
    for &i in idx {
        if !seen[i] {
            walk(i, &children, metas, &mut seen, &mut out);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Query terms
// ---------------------------------------------------------------------------------------------

/// The searchable fields of a session, lowercased once.
struct Fields {
    name: String,
    first: String,
    all: String,
    path: String,
}

impl Fields {
    fn of(m: &SessionMeta) -> Fields {
        Fields {
            name: m.name.as_deref().unwrap_or("").to_lowercase(),
            first: m.first_message.to_lowercase(),
            all: m.all_text.to_lowercase(),
            path: format!("{} {}", m.sref.path.to_string_lossy(), m.cwd.to_string_lossy()).to_lowercase(),
        }
    }
}

enum Term {
    /// A word or a `"quoted phrase"`: a case-insensitive substring.
    Text(String),
    /// `re:<pattern>`; `None` when the pattern does not compile.
    Re(Option<Regex>),
}

impl Term {
    /// The rank contribution of this term, or `None` when no field matches.
    fn score(&self, f: &Fields) -> Option<u32> {
        let hit = |field: &str| match self {
            Term::Text(t) => field.contains(t.as_str()),
            Term::Re(Some(re)) => re.is_match(field),
            Term::Re(None) => false,
        };
        let mut s = 0u32;
        if hit(&f.name) {
            s += 4;
        }
        if hit(&f.first) {
            s += 2;
        }
        if hit(&f.all) {
            s += 1;
        }
        if hit(&f.path) {
            s += 1;
        }
        (s > 0).then_some(s)
    }
}

/// Split a query into terms. Whitespace separates terms; `"..."` is a literal phrase; `re:` starts
/// a regex that runs to the next whitespace, and `re:"..."` takes a quoted pattern (for patterns
/// that contain spaces). An unclosed quote runs to the end of the query.
fn parse_query(q: &str) -> Vec<Term> {
    let chars: Vec<char> = q.chars().collect();
    let mut terms = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if let Some(text) = quoted_at(&chars, i) {
            // A quoted phrase.
            if !text.1.is_empty() {
                terms.push(Term::Text(text.1.to_lowercase()));
            }
            i = text.0;
            continue;
        }
        if chars[i..].starts_with(&['r', 'e', ':', '"']) {
            // A quoted regex: `re:"..."`.
            let (next, pattern) = quoted_at(&chars, i + 3).unwrap_or((chars.len(), String::new()));
            if !pattern.is_empty() {
                terms.push(Term::Re(Regex::new(&pattern)));
            }
            i = next;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if let Some(pattern) = word.strip_prefix("re:") {
            if !pattern.is_empty() {
                terms.push(Term::Re(Regex::new(pattern)));
            }
        } else {
            terms.push(Term::Text(word.to_lowercase()));
        }
    }
    terms
}

/// If a `"` opens at `at`, the index after the closing quote (or the end) and the text between.
fn quoted_at(chars: &[char], at: usize) -> Option<(usize, String)> {
    if chars.get(at) != Some(&'"') {
        return None;
    }
    let start = at + 1;
    let end = chars[start..]
        .iter()
        .position(|&c| c == '"')
        .map_or(chars.len(), |p| start + p);
    let text: String = chars[start..end].iter().collect();
    Some(((end + 1).min(chars.len()), text))
}

// ---------------------------------------------------------------------------------------------
// `re:` terms use the `regex` crate: matching runs in linear time and without recursion, so a
// pattern typed into the picker (it runs on every keystroke) can neither freeze nor crash oxa.
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Regex(regex::Regex);

impl Regex {
    /// Case-insensitive, with `.` also matching a newline (the text fields hold whole messages).
    /// `None` when the pattern does not compile or is too large.
    fn new(pattern: &str) -> Option<Regex> {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .dot_matches_new_line(true)
            .size_limit(1 << 20)
            .build()
            .ok()
            .map(Regex)
    }

    fn is_match(&self, text: &str) -> bool {
        self.0.is_match(text)
    }
}
