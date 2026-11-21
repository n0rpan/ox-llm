//! A light index of the session files for a thread list, and the one thread order.
//!
//! [`SessionStore::list`](crate::SessionStore::list) reads and decodes every entry of every
//! session, because the picker searches all text. A thread list needs far less, and it must
//! open with thousands of sessions on disk. [`FsStore::thread_index`] therefore reads at most
//! [`THREAD_HEAD_BYTES`] from the start of each file and [`THREAD_TAIL_BYTES`] from its end,
//! whatever the size of the file, and decodes only the lines it needs.
//!
//! What a bounded read cannot know is left out, never guessed:
//! * `name` is the last `session_info` entry inside the two windows. A name set in the middle
//!   of a long session and never again is not seen.
//! * `first_message` is the first user message inside the head window (the header, the prefix
//!   entry and the first prompt normally fit there). It is empty when the prompt lies further in.
//! * `modified_ms` is the newer of the header time and the time of the last entry in the file.
//!   If the last line alone is longer than the tail window, the file's modification time is used.
//!
//! # Where a session records its parent
//! * **Fork and clone:** the header's `parentSession` holds the path of the source file
//!   (`SessionStore::create` writes it). The header does not say which of the two it was, so
//!   the index reports [`Relation::Fork`] for both.
//! * **Worker:** a worker is its own process with its own session file, created with no parent.
//!   The link is in the worker registry of the parent (`<agent_dir>/workers/<parent id>.json`,
//!   one record per worker with the worker's `session_id`). Whoever reads the registry sets
//!   [`Relation::Worker`] and `parent` to the parent's session id; [`thread_order`] accepts an
//!   id as well as a path.
//! * **Subagent:** a subagent child runs with `--no-session` (or forks in memory), so it has no
//!   session file and no row in this index. It exists only while its parent runs.

use crate::meta::message_text;
use crate::store::{decode_entry, decode_header, open_regular_session, visit_session_files};
use crate::{FsStore, SessionRef};
use oxa_proto::{EntryBody, Role};
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Bytes read from the start of a session file: the header line and, normally, the prefix
/// entry and the first prompt.
pub const THREAD_HEAD_BYTES: u64 = 64 * 1024;
/// Bytes read from the end of a session file: the last entries (name, last activity).
pub const THREAD_TAIL_BYTES: u64 = 64 * 1024;
/// A row title needs no more of the first prompt than this many characters.
const FIRST_MESSAGE_CHARS: usize = 240;

/// How a session relates to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Relation {
    /// No parent.
    Root,
    /// Forked or cloned from the parent (the header's `parentSession`).
    Fork,
    /// A clone, when the reader knows it was one. The header alone does not say.
    Clone,
    /// A background worker of the parent (from the worker registry).
    Worker,
    /// A subagent of the parent (live only: it has no session file).
    Subagent,
}

/// One row of a thread list. See the module docs for what each field can and cannot know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMeta {
    pub sref: SessionRef,
    pub cwd: PathBuf,
    /// The parent: the path of its session file (the header's `parentSession`) or its session id.
    pub parent: Option<String>,
    pub relation: Relation,
    /// Display name from the last `session_info` entry that was read.
    pub name: Option<String>,
    /// First user message on one line, cut to a row title's length; empty when it was not read.
    pub first_message: String,
    pub modified_ms: u64,
    /// Size of the session file.
    pub bytes: u64,
}

impl FsStore {
    /// The session files of `cwd`, or of every directory when `None`, sorted by path (the order
    /// of `list`). Symlinked directories and files are left out. No file is opened.
    pub fn thread_files(&self, cwd: Option<&Path>) -> Vec<PathBuf> {
        let mut out = Vec::new();
        visit_session_files(self.root(), cwd, usize::MAX, |p| out.push(p));
        out.sort();
        out
    }

    /// The thread row of one session file, from a bounded read (see the module docs). `None`
    /// when the file cannot be opened as a regular session file or has no valid header line
    /// inside the head window.
    pub fn thread_meta(&self, path: &Path) -> Option<ThreadMeta> {
        let w = read_windows(path)?;
        let head = complete_lines(&w.head, false);
        let tail = w.tail.as_deref().map(|t| complete_lines(t, true));
        let (first, head) = head.split_first()?;
        let header = decode_header(first).ok()?;

        let mut name = None;
        let mut first_message = String::new();
        scan(head, true, &mut name, &mut first_message);
        if let Some(tail) = &tail {
            scan(tail, false, &mut name, &mut first_message);
        }

        // The last entry that decodes is the newest: entries are appended in time order.
        let last = tail.iter().flatten().rev().chain(head.iter().rev());
        let last_ms = last.copied().find_map(|l| decode_entry(l).ok()).map(|e| e.ts_ms);
        let mut modified_ms = header.ts_ms.max(last_ms.unwrap_or(0));
        if tail.as_ref().is_some_and(|t| t.is_empty()) {
            modified_ms = modified_ms.max(w.mtime_ms);
        }

        let relation = if header.parent_session.is_some() {
            Relation::Fork
        } else {
            Relation::Root
        };
        Some(ThreadMeta {
            sref: SessionRef {
                path: path.to_path_buf(),
                id: header.id,
            },
            cwd: header.cwd,
            parent: header.parent_session,
            relation,
            name,
            first_message,
            modified_ms,
            bytes: w.size,
        })
    }

    /// The thread rows of `cwd`, or of every directory when `None`, in the order of
    /// [`FsStore::thread_files`]; unreadable files are skipped. At most [`THREAD_HEAD_BYTES`] +
    /// [`THREAD_TAIL_BYTES`] (and one byte) are read per file.
    pub fn thread_index(&self, cwd: Option<&Path>) -> Vec<ThreadMeta> {
        let files = self.thread_files(cwd);
        files.iter().filter_map(|p| self.thread_meta(p)).collect()
    }
}

/// The threaded order of `metas`: `(index, depth)`, newest thread first.
///
/// A session is placed directly after its parent, when the parent is in `metas` (matched by
/// path, then by session id); its depth is the parent's plus one. Children are ordered newest
/// first, and a whole thread is ranked by its newest member. Members of a parent cycle are
/// reached from no root; they come last, the first of them at depth 0. This is the order of
/// `sort_sessions(.., SessionSort::Threaded)`: both call the same function.
pub fn thread_order(metas: &[ThreadMeta]) -> Vec<(usize, u16)> {
    let mut by_key: HashMap<Cow<'_, str>, usize> = HashMap::with_capacity(metas.len() * 2);
    for (i, m) in metas.iter().enumerate() {
        by_key.insert(Cow::Borrowed(m.sref.id.as_str()), i);
    }
    // A path wins over an id that happens to read the same.
    for (i, m) in metas.iter().enumerate() {
        by_key.insert(m.sref.path.to_string_lossy(), i);
    }
    let idx: Vec<usize> = (0..metas.len()).collect();
    let parent_of = |i: usize| metas[i].parent.as_deref().and_then(|p| by_key.get(p).copied());
    order_threads(metas.len(), &idx, parent_of, |i| metas[i].modified_ms)
}

/// The one thread order, over the items `idx` of a list of `len` items. `parent_of(i)` is the
/// parent of `i` when it is one of `idx`; `modified(i)` is its time. No recursion: a chain of
/// forks may be as long as the list.
pub(crate) fn order_threads(
    len: usize,
    idx: &[usize],
    parent_of: impl Fn(usize) -> Option<usize>,
    modified: impl Fn(usize) -> u64,
) -> Vec<(usize, u16)> {
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); len];
    let mut roots = Vec::new();
    for &i in idx {
        match parent_of(i).filter(|&p| p != i) {
            Some(p) => children[p].push(i),
            None => roots.push(i),
        }
    }
    for kids in &mut children {
        kids.sort_by_key(|&k| Reverse(modified(k)));
    }

    // The rank of a root is the newest member of its subtree. In a pre-order every descendant
    // comes after its ancestor, so the reverse pass has finished a node before it reaches the parent.
    let mut newest: Vec<u64> = (0..len).map(&modified).collect();
    let mut pre: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for &root in &roots {
        stack.extend(children[root].iter().map(|&k| (k, root)));
        while let Some((node, parent)) = stack.pop() {
            pre.push((node, parent));
            stack.extend(children[node].iter().map(|&k| (k, node)));
        }
    }
    for &(node, parent) in pre.iter().rev() {
        newest[parent] = newest[parent].max(newest[node]);
    }
    roots.sort_by_key(|&r| Reverse(newest[r]));

    // Roots first; then whatever no root reached (a parent cycle), in the order given.
    let mut out = Vec::with_capacity(idx.len());
    let mut seen = vec![false; len];
    let mut walk: Vec<(usize, u16)> = Vec::new();
    for &start in roots.iter().chain(idx) {
        walk.push((start, 0));
        while let Some((node, depth)) = walk.pop() {
            if seen[node] {
                continue;
            }
            seen[node] = true;
            out.push((node, depth));
            let below = depth.saturating_add(1);
            walk.extend(children[node].iter().rev().map(|&k| (k, below)));
        }
    }
    out
}

/// What was read of one file.
struct Windows {
    /// From the start: the whole file when it is no larger than the two windows together.
    head: Vec<u8>,
    /// From the end, starting one byte early (see [`complete_lines`]); `None` when `head` is the whole file.
    tail: Option<Vec<u8>>,
    size: u64,
    mtime_ms: u64,
}

fn read_windows(path: &Path) -> Option<Windows> {
    let mut file = open_regular_session(path).ok()?;
    let meta = file.metadata().ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    let mtime_ms = mtime.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let whole = size <= THREAD_HEAD_BYTES + THREAD_TAIL_BYTES;
    let mut head = Vec::new();
    let head_len = if whole { size } else { THREAD_HEAD_BYTES };
    file.by_ref().take(head_len).read_to_end(&mut head).ok()?;
    let tail = if whole {
        None
    } else {
        let mut tail = Vec::new();
        file.seek(SeekFrom::Start(size - THREAD_TAIL_BYTES - 1)).ok()?;
        file.by_ref()
            .take(THREAD_TAIL_BYTES + 1)
            .read_to_end(&mut tail)
            .ok()?;
        Some(tail)
    };
    Some(Windows {
        head,
        tail,
        size,
        mtime_ms,
    })
}

/// The complete lines of a window. The last piece is dropped: it is a torn line, a line the
/// window cut, or empty after a final newline. A tail window starts one byte early, so its
/// first piece is either a line cut at the start or the empty rest of the line before it;
/// `cut_start` drops that piece too.
fn complete_lines(window: &[u8], cut_start: bool) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = window.split(|b| *b == b'\n').collect();
    lines.pop();
    if cut_start && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

/// Take the name (the last one wins; a blank name clears it) and, when `want_first`, the first
/// user message from `lines`. Only lines that can hold one of the two are decoded.
fn scan(lines: &[&[u8]], want_first: bool, name: &mut Option<String>, first_message: &mut String) {
    for line in lines {
        let may_name = contains(line, b"\"session_info\"");
        let may_first = want_first && first_message.is_empty() && contains(line, b"\"user\"");
        if !may_name && !may_first {
            continue;
        }
        let Ok(entry) = decode_entry(line) else { continue };
        match &entry.body {
            EntryBody::SessionInfo { name: n } => {
                *name = Some(n.trim().to_string()).filter(|n| !n.is_empty());
            }
            EntryBody::Msg { wire, .. } if may_first && wire.role == Role::User => {
                let words: Vec<&str> = message_text(&entry.body).split_whitespace().collect();
                *first_message = words.join(" ").chars().take(FIRST_MESSAGE_CHARS).collect();
            }
            _ => {}
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
