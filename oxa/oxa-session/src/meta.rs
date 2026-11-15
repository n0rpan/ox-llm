//! B14: session metadata, search and sort for the picker. Owner: task B14. Signatures below are frozen; bodies are yours.
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

/// Build the metadata of one loaded session. `modified_ms` = timestamp of the last entry.
pub fn meta_of(sref: &SessionRef, header: &SessionHeader, entries: &[Entry]) -> SessionMeta {
    let mut name = None;
    let mut message_count = 0u32;
    let mut first_message: Option<String> = None;
    let mut all_text = String::new();
    for e in entries {
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
    let modified_ms = entries.last().map_or(header.ts_ms, |e| e.ts_ms);
    SessionMeta {
        sref: sref.clone(),
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

/// Display/search text shared by the standalone metadata builder and both stores' listings.
/// Without provenance, a trailing note-shaped block may be user-authored and is preserved.
pub(crate) fn message_text(body: &EntryBody) -> &str {
    let EntryBody::Msg { wire, parts } = body else {
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
// A small regex matcher for `re:` terms: alternation, groups (also `(?:..)`), `* + ? {m} {m,} {m,n}`,
// `.`, `^`, `$`, classes `[..]` with ranges and negation, and `\d \D \w \W \s \S`. Matching is
// case-insensitive: the subject is lowercased and so is the pattern's literal text. Backtracking.
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Regex {
    alts: Vec<Vec<Node>>,
}

#[derive(Debug)]
enum Node {
    Lit(char),
    Any,
    Set {
        items: Vec<Item>,
        neg: bool,
    },
    Start,
    End,
    Group(Vec<Vec<Node>>),
    Rep {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy)]
enum Class {
    Digit,
    Word,
    Space,
}

#[derive(Debug)]
enum Item {
    Range(char, char),
    Class(Class, bool),
}

impl Regex {
    fn new(pattern: &str) -> Option<Regex> {
        let mut p = Parser {
            c: pattern.chars().collect(),
            i: 0,
        };
        let alts = p.alt()?;
        (p.i == p.c.len()).then_some(Regex { alts })
    }

    fn is_match(&self, text: &str) -> bool {
        let t: Vec<char> = text.chars().map(lower).collect();
        (0..=t.len()).any(|start| m_alt(&self.alts, &t, start, &mut |_| true))
    }
}

struct Parser {
    c: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn alt(&mut self) -> Option<Vec<Vec<Node>>> {
        let mut alts = vec![self.seq()?];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.seq()?);
        }
        Some(alts)
    }

    fn seq(&mut self) -> Option<Vec<Node>> {
        let mut seq = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            seq.push(self.quantified(atom)?);
        }
        Some(seq)
    }

    fn atom(&mut self) -> Option<Node> {
        Some(match self.next()? {
            '(' => {
                if self.peek() == Some('?') {
                    self.i += 1;
                    if self.next()? != ':' {
                        return None;
                    }
                }
                let alts = self.alt()?;
                if self.next()? != ')' {
                    return None;
                }
                Node::Group(alts)
            }
            '[' => self.class()?,
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '\\' => match self.escape()? {
                Esc::Char(c) => Node::Lit(lower(c)),
                Esc::Class(k, neg) => Node::Set {
                    items: vec![Item::Class(k, neg)],
                    neg: false,
                },
            },
            '*' | '+' | '?' => return None,
            c => Node::Lit(lower(c)),
        })
    }

    fn quantified(&mut self, atom: Node) -> Option<Node> {
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => match self.counted() {
                Some(bounds) => {
                    return Some(Node::Rep {
                        node: Box::new(atom),
                        min: bounds.0,
                        max: bounds.1,
                    })
                }
                None => return Some(atom),
            },
            _ => return Some(atom),
        };
        self.i += 1;
        Some(Node::Rep {
            node: Box::new(atom),
            min,
            max,
        })
    }

    /// `{m}`, `{m,}` or `{m,n}` at the cursor. Consumes it when well formed; `{` is a literal otherwise.
    fn counted(&mut self) -> Option<(u32, Option<u32>)> {
        let close = self.c[self.i..].iter().position(|&c| c == '}')? + self.i;
        let body: String = self.c[self.i + 1..close].iter().collect();
        let (min, max) = match body.split_once(',') {
            None => {
                let n: u32 = body.parse().ok()?;
                (n, Some(n))
            }
            Some((lo, "")) => (lo.parse().ok()?, None),
            Some((lo, hi)) => {
                let (lo, hi): (u32, u32) = (lo.parse().ok()?, hi.parse().ok()?);
                if hi < lo {
                    return None;
                }
                (lo, Some(hi))
            }
        };
        self.i = close + 1;
        Some((min, max))
    }

    fn escape(&mut self) -> Option<Esc> {
        Some(match self.next()? {
            'd' => Esc::Class(Class::Digit, false),
            'D' => Esc::Class(Class::Digit, true),
            'w' => Esc::Class(Class::Word, false),
            'W' => Esc::Class(Class::Word, true),
            's' => Esc::Class(Class::Space, false),
            'S' => Esc::Class(Class::Space, true),
            'n' => Esc::Char('\n'),
            't' => Esc::Char('\t'),
            'r' => Esc::Char('\r'),
            c => Esc::Char(c),
        })
    }

    fn class(&mut self) -> Option<Node> {
        let neg = if self.peek() == Some('^') {
            self.i += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = self.next()?;
            if c == ']' && !first {
                break;
            }
            first = false;
            let lo = match c {
                '\\' => match self.escape()? {
                    Esc::Class(k, n) => {
                        items.push(Item::Class(k, n));
                        continue;
                    }
                    Esc::Char(ch) => ch,
                },
                ch => ch,
            };
            // A range needs a `-` followed by something other than the closing `]`.
            if self.peek() == Some('-') && self.c.get(self.i + 1).is_some_and(|&n| n != ']') {
                self.i += 1;
                let hi = match self.next()? {
                    '\\' => match self.escape()? {
                        Esc::Char(ch) => ch,
                        Esc::Class(..) => return None,
                    },
                    ch => ch,
                };
                if hi < lo {
                    return None;
                }
                items.push(Item::Range(lower(lo), lower(hi)));
            } else {
                items.push(Item::Range(lower(lo), lower(lo)));
            }
        }
        Some(Node::Set { items, neg })
    }
}

enum Esc {
    Char(char),
    Class(Class, bool),
}

/// The first lowercase character of `c` (the subject is lowercased the same way).
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn class_has(k: Class, c: char) -> bool {
    match k {
        Class::Digit => c.is_ascii_digit(),
        Class::Word => c.is_alphanumeric() || c == '_',
        Class::Space => c.is_whitespace(),
    }
}

fn set_has(items: &[Item], neg: bool, c: char) -> bool {
    let hit = items.iter().any(|it| match it {
        Item::Range(lo, hi) => *lo <= c && c <= *hi,
        Item::Class(k, n) => class_has(*k, c) != *n,
    });
    hit != neg
}

/// Whether a node matches exactly one character (these repeat without recursion).
fn is_single(n: &Node) -> bool {
    matches!(n, Node::Lit(_) | Node::Any | Node::Set { .. })
}

/// Whether the one-character node `n` matches `c`. Only called on nodes for which `is_single`.
fn single_matches(n: &Node, c: char) -> bool {
    match n {
        Node::Lit(l) => *l == c,
        Node::Any => true,
        Node::Set { items, neg } => set_has(items, *neg, c),
        _ => false,
    }
}

fn m_alt(alts: &[Vec<Node>], t: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    alts.iter().any(|seq| m_seq(seq, 0, t, pos, k))
}

fn m_seq(seq: &[Node], i: usize, t: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    if i == seq.len() {
        return k(pos);
    }
    m_node(&seq[i], t, pos, &mut |p| m_seq(seq, i + 1, t, p, k))
}

fn m_node(n: &Node, t: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    match n {
        Node::Lit(_) | Node::Any | Node::Set { .. } => {
            pos < t.len() && single_matches(n, t[pos]) && k(pos + 1)
        }
        Node::Start => pos == 0 && k(pos),
        Node::End => pos == t.len() && k(pos),
        Node::Group(alts) => m_alt(alts, t, pos, k),
        Node::Rep { node, min, max } => match is_single(node) {
            // Repeats of one character: count the run, then try the longest first.
            true => {
                let limit = max.map_or(usize::MAX, |m| m as usize);
                let mut run = 0usize;
                while run < limit && pos + run < t.len() && single_matches(node, t[pos + run]) {
                    run += 1;
                }
                let least = *min as usize;
                if run < least {
                    return false;
                }
                (least..=run).rev().any(|c| k(pos + c))
            }
            false => m_rep(node, *min, *max, 0, t, pos, k),
        },
    }
}

/// A repeat of a group: each iteration must consume input, so an empty iteration cannot loop.
fn m_rep(
    node: &Node,
    min: u32,
    max: Option<u32>,
    count: u32,
    t: &[char],
    pos: usize,
    k: &mut dyn FnMut(usize) -> bool,
) -> bool {
    if max.is_none_or(|m| count < m) {
        let more = m_node(node, t, pos, &mut |p| {
            if p == pos {
                false
            } else {
                m_rep(node, min, max, count + 1, t, p, k)
            }
        });
        if more {
            return true;
        }
    }
    count >= min && k(pos)
}
