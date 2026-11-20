//! The lead web page attached to a running owner session (#2075).
//!
//! The service reads the newest part of one owner session's file without its writer lock and
//! sends the owner's text through that session's inbox, exactly like `oxa send`. It takes no
//! lock, opens no provider, spends no tokens and appends nothing: the session's own process
//! (a tmux TUI, RPC or headless owner) stays the only writer and reads the inbox at its next
//! turn. The id comes only from owner-private user settings (`oxa.lead.attachSessionId`);
//! HTTP input never names a session or a path.
use crate::dispatch::lead_owner::{private_directory, private_file, unavailable, user_settings};
use oxa_config::Dirs;
use oxa_proto::{Entry, EntryBody, OxaError, Role};
use oxa_session::{
    decode_entry, decode_header, lock_holder, send_inbox, session_files_under, SessionHeader, SessionRef,
};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// How much of the end of the session file one page render reads. A display bound for the
/// refreshing page (lead sessions grow to many MB), not a limit on the session.
pub(crate) const TAIL_BYTES: u64 = 256 * 1024;
const MAX_HEADER: u64 = 65_536;

/// One owner session the page is attached to. Identity is the file's (device, inode) and its
/// header id, proven again before every read and every send.
#[derive(Clone)]
pub(crate) struct AttachedSession {
    dirs: Dirs,
    uid: u32,
    session: SessionRef,
    file: (u64, u64),
    /// Started from `oxa.lead.attachSessionId`: that setting must keep naming this session.
    /// A session the owner chose on the page (`select`) is held to the file proof only.
    follows_setting: bool,
}

/// One session of this user that is running now.
pub(crate) struct LiveSession {
    pub id: String,
    /// The project directory's basename; no other part of the path is shown.
    pub cwd_name: String,
    pub modified_ms: u64,
    path: PathBuf,
}

/// What a running session is doing, from the last message of the tail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Activity {
    Working,
    Idle,
}

/// The newest part of a session file as decoded entries.
pub(crate) struct Tail {
    pub entries: Vec<Entry>,
    /// Older history exists in the file but is not shown.
    pub truncated: bool,
}

/// The selected id from owner-private user settings; `None` when unset.
fn selection(dirs: &Dirs, uid: u32) -> Result<Option<String>, OxaError> {
    let Some(settings) = user_settings(dirs, uid)? else {
        return Ok(None);
    };
    let Some(id) = settings.pointer("/oxa/lead/attachSessionId") else {
        return Ok(None);
    };
    if id.is_null() {
        return Ok(None);
    }
    let id = id
        .as_str()
        .filter(|id| id.len() <= 128 && crate::args::valid_session_id(id))
        .ok_or_else(unavailable)?;
    Ok(Some(id.to_owned()))
}

/// Prove that `path` is a private owner file with a session header: the header, the file's
/// (device, inode) and its modification time in milliseconds.
fn proof(path: &Path, uid: u32) -> Result<(SessionHeader, (u64, u64), u64), OxaError> {
    let file = private_file(path, uid)?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    let mut line = Vec::new();
    BufReader::new(file.take(MAX_HEADER + 1))
        .read_until(b'\n', &mut line)
        .map_err(|_| unavailable())?;
    let header = decode_header(&line).map_err(|_| unavailable())?;
    let modified = metadata
        .mtime()
        .max(0)
        .unsigned_abs()
        .saturating_mul(1000)
        .saturating_add(metadata.mtime_nsec().max(0).unsigned_abs() / 1_000_000);
    Ok((header, (metadata.dev(), metadata.ino()), modified))
}

/// Prove that `path` is a private owner file whose header names `id`; returns its identity.
fn file_identity(path: &Path, uid: u32, id: &str) -> Result<(u64, u64), OxaError> {
    let (header, identity, _) = proof(path, uid)?;
    if header.id != id {
        return Err(unavailable());
    }
    Ok(identity)
}

/// Every session of this user that is open in some oxa right now (a lead TUI, an interactive
/// oxa, a worker), newest activity first. Found by the lock sidecars; the probe takes no lock.
/// Anything that is not a private owner file with a readable header is skipped silently.
pub(crate) fn live_sessions(dirs: &Dirs, uid: u32) -> Vec<LiveSession> {
    if private_directory(&dirs.sessions_dir, uid).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(&dirs.sessions_dir) else {
        return out;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let sidecar = file.path();
            let Some(name) = sidecar.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".lock").filter(|stem| stem.ends_with(".jsonl")) else {
                continue;
            };
            let path = sidecar.with_file_name(stem);
            if lock_holder(&path).is_none() {
                continue;
            }
            let Ok((header, _, modified_ms)) = proof(&path, uid) else {
                continue;
            };
            out.push(LiveSession {
                cwd_name: header
                    .cwd
                    .file_name()
                    .map_or_else(|| "(root)".to_owned(), |name| name.to_string_lossy().into_owned()),
                id: header.id,
                modified_ms,
                path,
            });
        }
    }
    out.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Working or idle, from the newest message of the entries: an assistant reply with no tool
/// call is idle; anything else (a user message, tool calls, a result) is working.
pub(crate) fn activity(entries: &[Entry]) -> Option<Activity> {
    entries.iter().rev().find_map(|entry| match &entry.body {
        EntryBody::Msg { wire, .. } => Some(if wire.role == Role::Assistant && wire.tool_calls.is_empty() {
            Activity::Idle
        } else {
            Activity::Working
        }),
        _ => None,
    })
}

/// Prompt tokens of the newest request whose usage the session recorded. The session does not
/// record the model's window, so no share of it is ever computed or guessed.
pub(crate) fn context_tokens(entries: &[Entry]) -> Option<u32> {
    entries.iter().rev().find_map(|entry| match &entry.body {
        EntryBody::Usage { usage, .. } if !usage.unreported => Some(usage.prompt),
        _ => None,
    })
}

impl AttachedSession {
    /// The selected session, or `None` when `oxa.lead.attachSessionId` is unset. Any failure is
    /// the fixed `lead_owner_unavailable` error, with no path or id bytes.
    pub(crate) fn load(dirs: &Dirs, uid: u32) -> Result<Option<Self>, OxaError> {
        let Some(id) = selection(dirs, uid)? else {
            return Ok(None);
        };
        private_directory(&dirs.sessions_dir, uid)?;
        let mut found = None;
        for (path, header) in session_files_under(&dirs.sessions_dir) {
            if header.is_some_and(|header| header.id == id) {
                if found.is_some() {
                    return Err(unavailable());
                }
                found = Some(path);
            }
        }
        let path = found.ok_or_else(unavailable)?;
        let file = file_identity(&path, uid, &id)?;
        Ok(Some(Self {
            dirs: dirs.clone(),
            uid,
            session: SessionRef { path, id },
            file,
            follows_setting: true,
        }))
    }

    /// The session the owner chose on the page. Only a session in the current `live_sessions`
    /// result qualifies; HTTP input supplies the id and nothing else.
    pub(crate) fn select(dirs: &Dirs, uid: u32, id: &str) -> Result<Self, OxaError> {
        if id.len() > 128 || !crate::args::valid_session_id(id) {
            return Err(unavailable());
        }
        let mut live = live_sessions(dirs, uid).into_iter().filter(|live| live.id == id);
        let (Some(found), None) = (live.next(), live.next()) else {
            return Err(unavailable());
        };
        let file = file_identity(&found.path, uid, id)?;
        Ok(Self {
            dirs: dirs.clone(),
            uid,
            session: SessionRef {
                path: found.path,
                id: id.to_owned(),
            },
            file,
            follows_setting: false,
        })
    }

    /// The id, to mark the attached row in the list.
    pub(crate) fn id(&self) -> &str {
        &self.session.id
    }

    /// The settings still select this id, the file is the one proven at load, and the
    /// directories are still owner-only.
    pub(crate) fn recheck(&self) -> Result<(), OxaError> {
        private_directory(&self.dirs.sessions_dir, self.uid)?;
        if (self.follows_setting
            && selection(&self.dirs, self.uid)?.as_deref() != Some(self.session.id.as_str()))
            || file_identity(&self.session.path, self.uid, &self.session.id)? != self.file
        {
            return Err(unavailable());
        }
        Ok(())
    }

    /// The first characters of the id, for the page heading.
    pub(crate) fn short_id(&self) -> String {
        self.session.id.chars().take(8).collect()
    }

    /// Some oxa process holds this session open right now.
    pub(crate) fn live(&self) -> bool {
        lock_holder(&self.session.path).is_some()
    }

    /// The newest part of the session file. Read-only: no lock, no append, no sync, no repair.
    pub(crate) fn tail(&self, max_bytes: u64) -> Result<Tail, OxaError> {
        self.recheck()?;
        let mut file = private_file(&self.session.path, self.uid)?;
        let metadata = file.metadata().map_err(|_| unavailable())?;
        if (metadata.dev(), metadata.ino()) != self.file {
            return Err(unavailable());
        }
        read_tail(&mut file, metadata.len(), max_bytes)
    }

    /// Publish the owner's text into the session's inbox, as `oxa send <id> <text>` does. The
    /// session reads it at its next turn; it is a follow-up for the model, never an approval.
    pub(crate) fn send(&self, text: &str) -> Result<(), OxaError> {
        self.recheck()?;
        send_inbox(&self.dirs.agent_dir, &self.session.id, text).map_err(|_| unavailable())
    }
}

fn read_tail(file: &mut File, len: u64, max_bytes: u64) -> Result<Tail, OxaError> {
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).map_err(|_| unavailable())?;
    let mut bytes = Vec::new();
    file.take(len - start)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    Ok(parse_tail(&bytes, start > 0))
}

/// Entries of a chunk of the file. A torn last line (no newline yet) and, when the chunk starts
/// inside the file, the partial first line are dropped; a line that does not decode is skipped.
fn parse_tail(bytes: &[u8], from_middle: bool) -> Tail {
    let complete = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |i| i + 1);
    let mut lines = &bytes[..complete];
    if from_middle {
        lines = match lines.iter().position(|byte| *byte == b'\n') {
            Some(i) => &lines[i + 1..],
            None => &[],
        };
    }
    let entries = lines
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| decode_entry(line).ok())
        .collect();
    Tail {
        entries,
        truncated: from_middle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use oxa_proto::{EntryBody, WireMsg};
    use oxa_session::{new_entry_id, FsStore, SessionStore};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn user_entry(text: &str) -> Entry {
        Entry {
            id: new_entry_id(&|_| false),
            parent: None,
            ts_ms: oxa_proto::time::now_ms(),
            body: EntryBody::Msg {
                wire: WireMsg::user(text),
                parts: Vec::new(),
                tool: None,
            },
        }
    }

    fn text_of(entry: &Entry) -> String {
        match &entry.body {
            EntryBody::Msg { wire, .. } => wire.content.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn select(test: &TestEnv, id: &str) {
        let settings = test.write(
            "home/.oxa/agent/settings.json",
            &serde_json::json!({"oxa": {"lead": {"attachSessionId": id}}}).to_string(),
        );
        std::fs::set_permissions(settings, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn fixture(count: usize) -> (TestEnv, Dirs, u32, SessionRef) {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(test.root()).unwrap().uid();
        let store = FsStore::new(dirs.sessions_dir.clone());
        let session = store.create(&test.work(), None, None).unwrap();
        for index in 0..count {
            store
                .append(
                    &session,
                    &user_entry(&format!("line {index:04} {}", "x".repeat(200))),
                )
                .unwrap();
        }
        select(&test, &session.id);
        (test, dirs, uid, session)
    }

    #[test]
    fn tail_drops_a_partial_first_line_and_a_torn_last_line() {
        let (_test, dirs, uid, session) = fixture(200);
        // A writer is mid-append: bytes without their newline yet.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&session.path)
            .unwrap();
        file.write_all(br#"{"type":"message","id":"torn"#).unwrap();
        let attached = AttachedSession::load(&dirs, uid).unwrap().unwrap();
        let before = std::fs::read(&session.path).unwrap();
        let tail = attached.tail(4096).unwrap();
        assert!(tail.truncated);
        assert!(!tail.entries.is_empty());
        let texts: Vec<String> = tail.entries.iter().map(text_of).collect();
        // The newest complete entry is shown, the oldest is not, and nothing is half decoded.
        assert!(texts.last().unwrap().starts_with("line 0199"), "{texts:?}");
        assert!(texts.iter().all(|text| text.len() > 200), "{texts:?}");
        assert!(!texts.iter().any(|text| text.starts_with("line 0000")));
        // Reading changed nothing, not even the torn bytes.
        assert_eq!(std::fs::read(&session.path).unwrap(), before);
        // A bound larger than the file shows everything and says nothing was cut.
        let all = attached.tail(TAIL_BYTES * 8).unwrap();
        assert!(!all.truncated);
        assert_eq!(all.entries.len(), 200);
    }

    #[test]
    fn recheck_fails_after_the_file_is_replaced_or_the_setting_changes() {
        let (test, dirs, uid, session) = fixture(3);
        let attached = AttachedSession::load(&dirs, uid).unwrap().unwrap();
        attached.recheck().unwrap();

        // The same bytes under a new inode: rename a copy over the session file.
        let copy = session.path.with_extension("copy");
        std::fs::copy(&session.path, &copy).unwrap();
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&copy, &session.path).unwrap();
        assert!(attached.recheck().is_err());
        assert!(attached.tail(TAIL_BYTES).is_err());
        assert!(attached.send("hello").is_err());

        let fresh = AttachedSession::load(&dirs, uid).unwrap().unwrap();
        fresh.recheck().unwrap();
        select(&test, "some-other-session");
        assert!(fresh.recheck().is_err());
        assert!(fresh.send("hello").is_err());
    }

    #[test]
    fn a_group_readable_session_file_is_refused() {
        let (_test, dirs, uid, session) = fixture(1);
        std::fs::set_permissions(&session.path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(AttachedSession::load(&dirs, uid).is_err());
    }

    #[test]
    fn only_a_live_session_can_be_selected_and_follows_no_setting() {
        let (test, dirs, uid, session) = fixture(1);
        assert!(live_sessions(&dirs, uid).is_empty());
        assert!(AttachedSession::select(&dirs, uid, &session.id).is_err());
        let store = FsStore::new(dirs.sessions_dir.clone());
        store.acquire(&session).unwrap();
        store.commit_lock();
        let live = live_sessions(&dirs, uid);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, session.id);
        assert_eq!(live[0].cwd_name, "work");
        let chosen = AttachedSession::select(&dirs, uid, &session.id).unwrap();
        assert!(chosen.live());
        // The owner's page choice does not depend on the settings key.
        select(&test, "some-other-session");
        chosen.recheck().unwrap();
        for bad in ["", "../x", "unknown-session", "a/b"] {
            assert!(AttachedSession::select(&dirs, uid, bad).is_err(), "{bad:?}");
        }
        drop(store);
        assert!(!chosen.live());
        assert!(AttachedSession::select(&dirs, uid, &session.id).is_err());
    }

    #[test]
    fn activity_and_context_come_from_the_recorded_entries() {
        let mut call = WireMsg::assistant("running");
        call.tool_calls.push(oxa_proto::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            args_raw: "{}".into(),
        });
        let message = |wire| Entry {
            body: EntryBody::Msg {
                wire,
                parts: Vec::new(),
                tool: None,
            },
            ..user_entry("x")
        };
        assert_eq!(activity(&[]), None);
        assert_eq!(activity(&[user_entry("hi")]), Some(Activity::Working));
        let done = [user_entry("hi"), message(WireMsg::assistant("done"))];
        assert_eq!(activity(&done), Some(Activity::Idle));
        let busy = [message(WireMsg::assistant("done")), message(call)];
        assert_eq!(activity(&busy), Some(Activity::Working));
        assert_eq!(context_tokens(&done), None);
        let usage = |prompt, unreported| Entry {
            body: EntryBody::Usage {
                kind: "turn".into(),
                usage: oxa_proto::Usage {
                    prompt,
                    completion: 1,
                    cached: 0,
                    cached_missing: false,
                    unreported,
                },
            },
            ..user_entry("x")
        };
        assert_eq!(context_tokens(&[usage(100, false), usage(200, false)]), Some(200));
        // A request that reported no usage is not a reading; the earlier one stays.
        assert_eq!(context_tokens(&[usage(100, false), usage(0, true)]), Some(100));
    }
}
