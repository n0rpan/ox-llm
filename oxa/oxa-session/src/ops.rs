//! B14: fork, clone, names and labels. Owner: task B14. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/session-manager.ts (createBranchedSession, fork)
use crate::{new_entry_id, SessionRef, SessionStore, Tree};
use oxa_proto::{codes, Entry, EntryBody, OxaError};
use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Create a new session file holding the branch root..=`entry` of `src` (ids kept), with
/// `parentSession` = `src.path`. `entry` must be on some branch of `src` (`not_found`).
pub fn fork_session(
    store: &dyn SessionStore,
    src: &SessionRef,
    entry: &str,
    cwd: &Path,
) -> Result<SessionRef, OxaError> {
    copy_branch(store, src, entry, cwd)
}

/// Copy the active branch of `src` (root..=leaf) into a new session with `parentSession` set.
pub fn clone_session(
    store: &dyn SessionStore,
    src: &SessionRef,
    leaf: &str,
    cwd: &Path,
) -> Result<SessionRef, OxaError> {
    copy_branch(store, src, leaf, cwd)
}

/// `--fork <session>` (pi `SessionManager.forkFrom`): a new session in `cwd` holding EVERY entry
/// of `src` (the whole tree, ids and parents kept, in file order, so the leaf on load is the
/// source's), with `parentSession` = `src.path` and the session id `id` when given (`--session-id`).
/// An empty source is `invalid` (pi: "source session file is empty or invalid"). A failed copy
/// removes the new file.
pub fn fork_file(
    store: &dyn SessionStore,
    src: &SessionRef,
    cwd: &Path,
    id: Option<&str>,
) -> Result<SessionRef, OxaError> {
    let entries = store.load(src)?;
    if entries.is_empty() {
        return Err(OxaError::new(
            codes::INVALID,
            format!("cannot fork: source session {} is empty", src.path.display()),
        ));
    }
    let dst = store.create(cwd, id, Some(src.path.as_path()))?;
    for e in &entries {
        if let Err(err) = store.append(&dst, e) {
            let _ = store.delete(&dst);
            return Err(err);
        }
    }
    Ok(dst)
}

/// Append a `SessionInfo` entry under `leaf`. Returns the new entry id.
pub fn set_name(
    store: &dyn SessionStore,
    s: &SessionRef,
    leaf: &str,
    name: &str,
) -> Result<String, OxaError> {
    let body = EntryBody::SessionInfo {
        name: name.trim().to_string(),
    };
    append_under(store, s, leaf, None, body)
}

/// Append a `Label` entry (`label: None` clears). Returns the new entry id.
pub fn set_label(
    store: &dyn SessionStore,
    s: &SessionRef,
    leaf: &str,
    target: &str,
    label: Option<&str>,
) -> Result<String, OxaError> {
    // An empty or blank label clears the label, as a missing one does.
    let label = label.map(str::trim).filter(|l| !l.is_empty()).map(String::from);
    let body = EntryBody::Label {
        target: target.to_string(),
        label,
    };
    append_under(store, s, leaf, Some(target), body)
}

/// The shared body of fork and clone: root..=`tip` of `src` written to a new session in `cwd`.
/// The copied entries keep their ids and parents; the current labels of the copied entries are
/// written again as new `Label` entries after them, as pi does. A failed copy removes the new file.
fn copy_branch(
    store: &dyn SessionStore,
    src: &SessionRef,
    tip: &str,
    cwd: &Path,
) -> Result<SessionRef, OxaError> {
    let tree = Tree::new(store.load(src)?);
    if tree.get(tip).is_none() {
        return Err(not_found(tip, src));
    }
    let branch: Vec<Entry> = tree.branch(tip).into_iter().cloned().collect();
    let dst = store.create(cwd, None, Some(src.path.as_path()))?;
    let written = write_branch(store, &dst, &tree, &branch);
    if let Err(e) = written {
        let _ = store.delete(&dst);
        return Err(e);
    }
    Ok(dst)
}

fn write_branch(
    store: &dyn SessionStore,
    dst: &SessionRef,
    tree: &Tree,
    branch: &[Entry],
) -> Result<(), OxaError> {
    for e in branch {
        store.append(dst, e)?;
    }
    let mut taken: HashSet<String> = branch.iter().map(|e| e.id.clone()).collect();
    let mut parent = branch.last().map(|e| e.id.clone());
    for e in branch {
        let Some(label) = tree.label(&e.id) else {
            continue;
        };
        let id = new_entry_id(&|x| taken.contains(x));
        taken.insert(id.clone());
        let body = EntryBody::Label {
            target: e.id.clone(),
            label: Some(label.to_string()),
        };
        store.append(
            dst,
            &Entry {
                id: id.clone(),
                parent: parent.clone(),
                ts_ms: now_ms(),
                body,
            },
        )?;
        parent = Some(id);
    }
    Ok(())
}

/// Append `body` as a child of `leaf`. `need` is an entry id that must also exist in the session.
fn append_under(
    store: &dyn SessionStore,
    s: &SessionRef,
    leaf: &str,
    need: Option<&str>,
    body: EntryBody,
) -> Result<String, OxaError> {
    let entries = store.load(s)?;
    let taken: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
    if !taken.contains(leaf) {
        return Err(not_found(leaf, s));
    }
    if let Some(target) = need {
        if !taken.contains(target) {
            return Err(not_found(target, s));
        }
    }
    let id = new_entry_id(&|x| taken.contains(x));
    let e = Entry {
        id: id.clone(),
        parent: Some(leaf.to_string()),
        ts_ms: now_ms(),
        body,
    };
    store.append(s, &e)?;
    Ok(id)
}

fn not_found(id: &str, s: &SessionRef) -> OxaError {
    OxaError::new(codes::NOT_FOUND, format!("entry {id} is not in session {}", s.id))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
