//! Spill lifetime follows the full fork/clone ancestry, including deleted intermediates.
use crate::store::{check_safe, ensure_dir, read_file, validate_session_id};
use crate::{
    decode_header, encode_header, remove_spill_dir, Removed, SessionHeader, SessionRef, SessionStore,
};
use oxa_proto::{codes, OxaError};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const ANCESTRY_DIR: &str = ".spill-ancestry";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
type Retained = HashMap<PathBuf, (SessionRef, SessionHeader)>;

/// Remove a session, saving only its header and original path first. Deleted headers keep
/// ancestry traversable (including through sessions with no spill files) until the last
/// descendant is removed. Call [`remove_spill_dir_unless_forked`] after a successful removal.
/// The private metadata is outside all per-session read scopes; no transcript is retained.
pub fn remove_session_preserving_spills(
    store: &dyn SessionStore,
    tmp_dir: &Path,
    session: &SessionRef,
) -> Result<Removed, OxaError> {
    validate_session_id(&session.id)?;
    let header = store.header(session)?;
    if header.id != session.id {
        return Err(OxaError::new(codes::INVALID, "session header id mismatch"));
    }
    let mut bytes = Vec::new();
    encode_header(&header, &mut bytes);
    let mut record: Value = serde_json::from_slice(&bytes).expect("encoded header");
    record["sessionPath"] = Value::from(session.path.to_string_lossy().as_ref());
    let bytes = serde_json::to_vec(&record).expect("header record");
    ensure_dir(tmp_dir)?;
    let root = tmp_dir.join(ANCESTRY_DIR);
    ensure_dir(&root)?;
    let path = root.join(format!("{}.json", session.id));
    check_safe(&path)?;
    // Only complete, synced records become visible to readers. An interrupted write leaves
    // at most an unpublished .tmp file, never a truncated record at the final path.
    let (staging, mut file) = loop {
        let staging = root.join(format!(
            ".{}-{}-{}.tmp",
            session.id,
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staging)
        {
            Ok(file) => break (staging, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(OxaError::io("cannot preserve spill ancestry", &e)),
        }
    };
    let publish = || -> Result<(), OxaError> {
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| OxaError::io("cannot preserve spill ancestry", &e))?;
        check_safe(&path)?;
        fs::rename(&staging, &path)
            .and_then(|()| File::open(&root)?.sync_all())
            .map_err(|e| OxaError::io("cannot publish spill ancestry", &e))
    }();
    if publish.is_err() {
        let _ = fs::remove_file(&staging);
    }
    publish?;
    store.remove(session)
}

fn read_retained_session(path: &Path) -> Result<(SessionRef, SessionHeader), OxaError> {
    check_safe(path)?;
    let bytes = read_file(path)?;
    let header = decode_header(&bytes)?;
    validate_session_id(&header.id)?;
    if path.file_name() != Some(std::ffi::OsStr::new(&format!("{}.json", header.id))) {
        return Err(OxaError::new(codes::INVALID, "spill ancestry id mismatch"));
    }
    let record: Value = serde_json::from_slice(&bytes).expect("decoded header");
    let original = record["sessionPath"]
        .as_str()
        .ok_or_else(|| OxaError::new(codes::INVALID, "spill ancestry has no original session path"))?;
    let sref = SessionRef {
        path: PathBuf::from(original),
        id: header.id.clone(),
    };
    Ok((sref, header))
}

fn retained_sessions(tmp_dir: &Path, tolerate_invalid_records: bool) -> Result<Retained, OxaError> {
    let root = tmp_dir.join(ANCESTRY_DIR);
    check_safe(&root)?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(OxaError::io("cannot read spill ancestry", &e)),
    };
    let mut retained = HashMap::new();
    for entry in entries {
        let path = entry
            .map_err(|e| OxaError::io("cannot read spill ancestry", &e))?
            .path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue; // Unpublished staging files are not ancestry records.
        }
        match read_retained_session(&path) {
            Ok((sref, header)) => {
                retained.insert(sref.path.clone(), (sref, header));
            }
            // Reads may use independently validated records. Cleanup must fail closed:
            // an incomplete ancestry map cannot prove that a spill is unreferenced.
            Err(_) if tolerate_invalid_records => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(retained)
}

fn ancestors(store: &dyn SessionStore, session: &SessionRef, retained: &Retained) -> Vec<SessionRef> {
    let mut result = Vec::new();
    let mut seen = HashSet::from([session.path.clone()]);
    let mut source = session.clone();
    loop {
        let header = store
            .header(&source)
            .ok()
            .or_else(|| retained.get(&source.path).map(|(_, header)| header.clone()));
        let Some(parent) = header.and_then(|header| header.parent_session) else {
            break;
        };
        let parent = store
            .resolve(&parent)
            .ok()
            .or_else(|| retained.get(Path::new(&parent)).map(|(sref, _)| sref.clone()));
        let Some(parent) = parent else { break };
        if !seen.insert(parent.path.clone()) || validate_session_id(&parent.id).is_err() {
            break;
        }
        result.push(parent.clone());
        source = parent;
    }
    result
}

/// Read-only spill scopes inherited from trusted session headers, never from tool/model text.
/// Deleted headers are consulted only when the corresponding live session no longer resolves.
pub fn inherited_spill_dirs(store: &dyn SessionStore, tmp_dir: &Path, session: &SessionRef) -> Vec<PathBuf> {
    let retained = retained_sessions(tmp_dir, true).unwrap_or_default();
    ancestors(store, session, &retained)
        .iter()
        .map(|s| tmp_dir.join(&s.id))
        .collect()
}

/// After deletion, retain the spill directories reachable from ANY surviving session in the
/// store, across workspaces and through deleted intermediates. Revisit retained sources in
/// that store, so deleting the last descendant also reclaims its ancestors' output and headers.
/// Other stores can share `tmp_dir` (via `--session-dir`); their live references have not been
/// checked, so their retained sources are never candidates. The same applies to an explicit
/// removed path outside this store. Arbitrary scratch directories are not candidates either.
/// Returns the number of spill files removed (not metadata files).
pub fn remove_spill_dir_unless_forked(
    store: &dyn SessionStore,
    tmp_dir: &Path,
    removed: &SessionRef,
) -> Result<usize, OxaError> {
    validate_session_id(&removed.id)?;
    let retained = retained_sessions(tmp_dir, false)?;
    let mut live = HashSet::new();
    for session in store.list(None) {
        live.insert(session.sref.id.clone());
        live.extend(
            ancestors(store, &session.sref, &retained)
                .into_iter()
                .map(|s| s.id),
        );
    }
    let candidates: HashSet<_> = retained
        .values()
        .map(|(s, _)| s)
        .chain(std::iter::once(removed))
        .filter(|s| store.lists_path(&s.path))
        .map(|s| s.id.as_str())
        .collect();
    let mut removed_files = 0;
    for id in candidates {
        if live.contains(id) {
            continue;
        }
        removed_files += remove_spill_dir(tmp_dir, id)?;
        let path = tmp_dir.join(ANCESTRY_DIR).join(format!("{id}.json"));
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(OxaError::io("cannot remove spill ancestry", &e)),
        }
    }
    Ok(removed_files)
}
