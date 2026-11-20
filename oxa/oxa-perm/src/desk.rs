//! #1512 S5: the approval desk. A run with no one to ask (`!Ui::interactive`) files a dangerous
//! command here instead of pausing; the owner answers with `oxa send <session> --approve <id>`
//! Answers come only from the desk, never from the inbox text path.
//!
//! Storage: `<agent_dir>/approvals/`, mode 0700; one `<id>.json` per request, mode 0600. Files
//! are written under a private temporary name and renamed into place, so a symlink at a record
//! name is replaced, never followed. The desk refuses a symlinked agent directory or desk
//! directory, and one owned by another user.
use crate::danger::DangerHit;
use crate::policy::danger_key;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory under the agent directory that holds the requests.
const DIR_NAME: &str = "approvals";
/// The advisory lock file of the desk, held across an answer or a consumption.
const LOCK_NAME: &str = ".lock";
/// The answers the owner can give (`oxa send --approve <id> <answer>`).
const ANSWERS: &[&str] = &["once", "session", "always", "deny"];

// This rule includes shell source read from files, processes or the network. A command
// string alone cannot identify the bytes an unattended retry will execute.
pub(crate) const OPAQUE_SHELL_INPUT: &str = "pipe-to-shell";

/// How an unattended run answers a dangerous command: the setting `oxa.approval.unattended`.
/// `Auto` refuses under the OS sandbox and files a request without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnattendedMode {
    #[default]
    Auto,
    Refuse,
    Request,
}

impl UnattendedMode {
    /// The setting's value (`auto`, `refuse`, `request`); anything else is `None`.
    pub fn parse(value: &str) -> Option<UnattendedMode> {
        match value {
            "auto" => Some(UnattendedMode::Auto),
            "refuse" => Some(UnattendedMode::Refuse),
            "request" => Some(UnattendedMode::Request),
            _ => None,
        }
    }

    /// `auto` resolved for a run: refuse with the OS sandbox, request without it. The other two
    /// modes stand as they are.
    pub fn resolve(self, os_sandbox: bool) -> UnattendedMode {
        match self {
            UnattendedMode::Auto if os_sandbox => UnattendedMode::Refuse,
            UnattendedMode::Auto => UnattendedMode::Request,
            other => other,
        }
    }
}

/// One stored request. `status` is `pending` (waiting for the owner), `refused` (no request was
/// possible: refused at once), or the owner's answer `once`, `session`, `always` or `deny`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    /// Identifies the unattended process that filed the request. A resumed process has a new
    /// nonce even when it reuses the persisted session id.
    pub run_nonce: String,
    pub session: String,
    pub cwd: String,
    pub command: String,
    pub keys: Vec<String>,
    pub reasons: Vec<String>,
    pub status: String,
    /// A `once` grant is consumed by the run that retries the command.
    pub used: bool,
    pub created_ms: u64,
    pub answered_ms: Option<u64>,
}

impl Record {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "run_nonce": self.run_nonce,
            "session": self.session,
            "cwd": self.cwd,
            "command": self.command,
            "keys": self.keys,
            "reasons": self.reasons,
            "status": self.status,
            "used": self.used,
            "created_ms": self.created_ms,
            "answered_ms": self.answered_ms,
        })
    }

    fn from_value(v: &Value) -> Option<Record> {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let list = |k: &str| -> Option<Vec<String>> {
            v.get(k)?
                .as_array()?
                .iter()
                .map(|x| x.as_str().map(str::to_string))
                .collect()
        };
        Some(Record {
            id: text("id")?,
            // Records written before run binding are history, not reusable grants.
            run_nonce: text("run_nonce").unwrap_or_default(),
            session: text("session")?,
            cwd: text("cwd")?,
            command: text("command")?,
            keys: list("keys")?,
            reasons: list("reasons")?,
            status: text("status")?,
            used: v.get("used")?.as_bool()?,
            created_ms: v.get("created_ms")?.as_u64()?,
            answered_ms: v.get("answered_ms").and_then(Value::as_u64),
        })
    }
}

/// The approval desk of one agent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDesk {
    dir: PathBuf,
    /// Fresh for every run that opens the desk. The owner may answer through another desk
    /// handle, but the record keeps the nonce of the run that filed it.
    run_nonce: String,
}

/// The held desk lock; dropping it releases the lock and closes the descriptor.
struct DeskLock(File);

impl Drop for DeskLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl ApprovalDesk {
    /// Open (and create, private) the desk under `agent_dir`. Refuses a symlinked agent or desk
    /// directory, and a desk directory that another user owns.
    pub fn open(agent_dir: &Path) -> io::Result<ApprovalDesk> {
        refuse_symlink(agent_dir)?;
        DirBuilder::new().recursive(true).mode(0o700).create(agent_dir)?;
        let dir = agent_dir.join(DIR_NAME);
        refuse_symlink(&dir)?;
        match fs::metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(invalid_dir("approval desk is not a directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Two processes can see `NotFound` at once: the loser of the create gets
                // `AlreadyExists` and must carry on with the directory the winner made, or
                // approval requests would be disabled for that whole process.
                match DirBuilder::new().mode(0o700).create(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if !fs::metadata(&dir)?.is_dir() {
                            return Err(invalid_dir("approval desk is not a directory"));
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        // A file this process creates now is owned by this user; the directory must match it.
        let probe = dir.join(format!(".probe-{}", random_hex(4)));
        let owner = {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&probe)?;
            let uid = file.metadata()?.uid();
            drop(file);
            let _ = fs::remove_file(&probe);
            uid
        };
        if fs::metadata(&dir)?.uid() != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "approval desk is not owned by this user",
            ));
        }
        Ok(ApprovalDesk {
            dir,
            run_nonce: random_hex(16),
        })
    }

    /// Hold the desk's exclusive advisory lock for one read-modify-write transition. The lock
    /// lives in the kernel (`flock(2)` through `std`), so a process that dies holding it releases
    /// it: a crashed answer can never lock the desk forever. The lock file is not a record, so
    /// `all` ignores it.
    fn lock(&self) -> io::Result<DeskLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.dir.join(LOCK_NAME))?;
        file.lock()?;
        Ok(DeskLock(file))
    }

    /// File a request for `hits` of `command` in `session`. A `pending` request reuses the open
    /// one with the same session and keys. Returns the request id.
    pub fn record(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        hits: &[DangerHit],
        status: &str,
    ) -> io::Result<String> {
        // The same key the policy grants use: the rule over the complete command line, so an
        // approval is bound to the command the owner reviewed and never to a bare segment.
        let keys: Vec<String> = hits.iter().map(|h| danger_key(h, command)).collect();
        // Dedup and the write are one transition: two runs filing at once must not both insert.
        let _lock = self.lock()?;
        let dir = dir_key(cwd);
        if status == "pending" {
            let open = self
                .all()?
                .into_iter()
                .find(|r| r.status == "pending" && r.session == session && r.cwd == dir && r.keys == keys);
            if let Some(open) = open {
                return Ok(open.id);
            }
        }
        let rec = Record {
            id: random_hex(8),
            run_nonce: self.run_nonce.clone(),
            session: session.to_string(),
            cwd: dir,
            command: command.to_string(),
            keys,
            reasons: hits.iter().map(|h| format!("{}: {}", h.rule, h.reason)).collect(),
            status: status.to_string(),
            used: false,
            created_ms: now_ms(),
            answered_ms: None,
        };
        if status == "refused" || status == "pending" {
            self.rotate(status)?;
        }
        self.write(&rec)?;
        Ok(rec.id)
    }

    /// Make room for one more `refused` or `pending` record: delete the oldest ones of that
    /// status beyond [`REFUSED_KEEP`] / [`PENDING_KEEP`]. Neither status is a grant, so an
    /// answered approval is never evicted. Called with the desk lock held.
    fn rotate(&self, status: &str) -> io::Result<()> {
        let keep = if status == "pending" {
            PENDING_KEEP
        } else {
            REFUSED_KEEP
        };
        let same: Vec<Record> = self.all()?.into_iter().filter(|r| r.status == status).collect();
        let excess = (same.len() + 1).saturating_sub(keep);
        for rec in same.iter().take(excess) {
            match fs::remove_file(self.dir.join(format!("{}.json", rec.id))) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The owner's answer to a `pending` request. Any other status is refused.
    pub fn answer(&self, id: &str, status: &str) -> io::Result<Record> {
        if !valid_id(id) {
            return Err(invalid_input("invalid approval request id"));
        }
        if !ANSWERS.contains(&status) {
            return Err(invalid_input("answer must be once, session, always or deny"));
        }
        let _lock = self.lock()?;
        let mut rec = self
            .load(id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such approval request"))?;
        if rec.status != "pending" {
            return Err(invalid_input(&format!(
                "approval request {id} is {}, not pending",
                rec.status
            )));
        }
        if status != "deny"
            && rec.keys.iter().any(|key| {
                key.split_once(' ')
                    .is_some_and(|(rule, _)| rule == OPAQUE_SHELL_INPUT)
            })
        {
            return Err(invalid_input(
                "shell input cannot be approved for unattended replay",
            ));
        }
        rec.status = status.to_string();
        rec.answered_ms = Some(now_ms());
        self.write(&rec)?;
        Ok(rec)
    }

    /// `true` when a grant for `session` covers every key, spending nothing. A policy asks this
    /// while it is still deciding (a verdict, a `danger_ask` the caller may yet turn into a
    /// prompt), so a `once` grant stays available until [`ApprovalDesk::consume`] confirms that
    /// the command it approved is the one about to run.
    pub fn covers(&self, session: &str, cwd: &Path, keys: &[String]) -> bool {
        if keys.is_empty() {
            return false;
        }
        let Ok(records) = self.all() else {
            return false;
        };
        let dir = dir_key(cwd);
        records.iter().any(|rec| {
            rec.session == session
                && rec.run_nonce == self.run_nonce
                && rec.cwd == dir
                && keys.iter().all(|k| rec.keys.contains(k))
                && match rec.status.as_str() {
                    "session" | "always" => self.reusable(rec),
                    "once" => !rec.used,
                    _ => false,
                }
        })
    }

    fn claim(&self, session: &str, cwd: &Path, keys: &[String], reusable: bool) -> bool {
        if keys.is_empty() {
            return false;
        }
        let Ok(_lock) = self.lock() else {
            return false;
        };
        let Ok(records) = self.all() else {
            return false;
        };
        let dir = dir_key(cwd);
        for mut rec in records {
            if rec.session != session
                || rec.run_nonce != self.run_nonce
                || rec.cwd != dir
                || !keys.iter().all(|k| rec.keys.contains(k))
            {
                continue;
            }
            match rec.status.as_str() {
                "session" | "always" if reusable && self.reusable(&rec) => return true,
                "once" if !rec.used => {
                    rec.used = true;
                    return self.write(&rec).is_ok();
                }
                _ => {}
            }
        }
        false
    }

    /// A desk grant holds only in the run that filed it. Later runs read `always` grants from
    /// the user settings, so removing a key there revokes it; a `session` grant simply expires.
    fn reusable(&self, rec: &Record) -> bool {
        match rec.status.as_str() {
            "session" | "always" => rec.run_nonce == self.run_nonce,
            _ => false,
        }
    }

    /// Spend a matching unused `once` grant. Reusable grants stay untouched.
    pub fn consume(&self, session: &str, cwd: &Path, keys: &[String]) -> bool {
        self.claim(session, cwd, keys, false)
    }

    /// Check and claim authorization under one lock, requiring a successful write for `once`.
    pub fn granted(&self, session: &str, cwd: &Path, keys: &[String]) -> bool {
        self.claim(session, cwd, keys, true)
    }

    /// Remove the records of `session` (all of them when `None`) from the desk under
    /// `agent_dir`, without creating it: a directory that does not exist holds nothing, so a
    /// wipe leaves no empty approval directory behind.
    pub fn wipe(agent_dir: &Path, session: Option<&str>) -> io::Result<usize> {
        if !agent_dir.join(DIR_NAME).is_dir() {
            return Ok(0);
        }
        let desk = ApprovalDesk::open(agent_dir)?;
        match session {
            Some(id) => desk.forget_session(id),
            None => desk.forget_all(),
        }
    }

    /// Remove every record of `session`, including private temporary records left by an
    /// interrupted write: a wiped session keeps no copy of its commands, working directories or
    /// ids. Returns how many records were removed.
    pub fn forget_session(&self, session: &str) -> io::Result<usize> {
        self.forget(false, |rec| rec.session == session)
    }

    /// Remove every complete or private temporary record, for `oxa wipe --all`.
    pub fn forget_all(&self) -> io::Result<usize> {
        self.forget(true, |_| true)
    }

    fn forget(&self, all_sessions: bool, doomed: impl Fn(&Record) -> bool) -> io::Result<usize> {
        // The same lock as an answer or a consumption: an update already running cannot rename
        // its copy of a record back into place after this wipe removed it.
        let _lock = self.lock()?;
        let mut removed = 0;
        for rec in self.all()? {
            if !doomed(&rec) {
                continue;
            }
            match fs::remove_file(self.dir.join(format!("{}.json", rec.id))) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        // A process can exit after syncing its private temporary record but before the rename.
        // That file contains the same command history, so privacy cleanup must include it.
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.') || !name.ends_with(".tmp") || !entry.file_type()?.is_file() {
                continue;
            }
            let remove = all_sessions
                || fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .and_then(|value| Record::from_value(&value))
                    .is_some_and(|record| doomed(&record));
            if remove {
                match fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(removed)
    }

    /// Every record, pending or answered.
    pub fn records(&self) -> io::Result<Vec<Record>> {
        self.all()
    }

    /// Every request still waiting for the owner, oldest first.
    pub fn pending(&self) -> io::Result<Vec<Record>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|r| r.status == "pending")
            .collect())
    }

    /// Write `rec` under a private temporary name, then rename it over its record.
    fn write(&self, rec: &Record) -> io::Result<()> {
        let target = self.dir.join(format!("{}.json", rec.id));
        let tmp = self.dir.join(format!(".{}.{}.tmp", rec.id, random_hex(4)));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let text = rec.to_value().to_string();
        let result = match file.write_all(text.as_bytes()) {
            Ok(()) => file.sync_all(),
            Err(e) => Err(e),
        };
        drop(file);
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::rename(&tmp, &target)
    }

    /// One record by id; a symlink, a non-file or an unreadable record is `None`.
    fn load(&self, id: &str) -> io::Result<Option<Record>> {
        let path = self.dir.join(format!("{id}.json"));
        let before = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // No `O_NOFOLLOW` without libc: the opened file must be the very inode checked above, so
        // a symlink swapped in between the check and the open is never followed.
        let mut file = File::open(&path)?;
        let opened = file.metadata()?;
        if opened.dev() != before.dev() || opened.ino() != before.ino() || !opened.is_file() {
            return Ok(None);
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(Record::from_value))
    }

    /// Every readable record, oldest first.
    fn all(&self) -> io::Result<Vec<Record>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            if let Some(rec) = self.load(id)? {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.created_ms);
        Ok(out)
    }
}

/// A request id: 16 lowercase hex digits.
fn valid_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "approval desk path is a symlink",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn invalid_dir(text: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text)
}

fn invalid_input(text: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text)
}

/// The directory identity a grant binds to: the canonical path, so a symlink retargeted after
/// the owner approved no longer matches the directory the command will run in. A directory that
/// cannot be resolved keeps its given path.
fn dir_key(cwd: &Path) -> String {
    fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

/// How many `refused` records the desk keeps. A refusal needs no answer, so older ones rotate out
/// instead of growing the directory without bound when a model repeats refused commands.
const REFUSED_KEEP: usize = 500;
/// The most `pending` requests the desk keeps; the oldest unanswered one makes room.
const PENDING_KEEP: usize = 500;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `bytes` random bytes as lowercase hex. `/dev/urandom`; if that is unreadable, a digest of the
/// clock and the process id. The ids are names, not secrets: answering needs the owner's CLI.
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    let from_os = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    if from_os.is_err() {
        let mut hasher = Sha256::new();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        hasher.update(nanos.to_le_bytes());
        hasher.update(std::process::id().to_le_bytes());
        let digest = hasher.finalize();
        buf = digest.iter().take(bytes).copied().collect();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxa-desk-{tag}-{}", random_hex(6)));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn hit() -> DangerHit {
        DangerHit {
            rule: "force-push".into(),
            reason: "overwrites remote history".into(),
            segment: "git push --force".into(),
        }
    }

    /// A wipe waits for the desk lock, so it cannot interleave with an answer that has already
    /// loaded a record and is about to write it back.
    #[test]
    fn forgetting_records_waits_for_the_desk_lock() {
        let root = temp_dir("lock");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let cwd = temp_dir("lockcwd");
        desk.record("s1", &cwd, "git push --force", &[hit()], "pending")
            .expect("record");
        let guard = desk.lock().expect("lock");
        let (tx, rx) = mpsc::channel();
        let other = desk.clone();
        let wipe = std::thread::spawn(move || {
            let removed = other.forget_all().expect("forget");
            tx.send(removed).expect("send");
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "forget must wait while another update holds the lock"
        );
        drop(guard);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).expect("removed"), 1);
        wipe.join().expect("thread");
    }

    /// A symlink retargeted after approval is another directory: the grant no longer matches.
    #[test]
    fn a_grant_follows_the_canonical_directory_not_a_symlink_name() {
        let root = temp_dir("link");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let (a, b) = (temp_dir("linka"), temp_dir("linkb"));
        let link = root.join("work");
        std::os::unix::fs::symlink(&a, &link).expect("link");
        let command = "git push --force";
        let keys = vec![danger_key(&hit(), command)];
        let id = desk
            .record("s1", &link, command, &[hit()], "pending")
            .expect("record");
        let owner = ApprovalDesk::open(&root).expect("owner desk");
        owner.answer(&id, "session").expect("answer");
        assert!(desk.covers("s1", &link, &keys));
        fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&b, &link).expect("relink");
        assert!(
            !desk.covers("s1", &link, &keys),
            "the retargeted link is another directory"
        );
        assert!(!desk.granted("s1", &link, &keys));
        assert!(
            desk.covers("s1", &a, &keys),
            "the reviewed directory still is covered"
        );
    }

    /// Refused records rotate: repeating refused commands cannot grow the desk without bound.
    #[test]
    fn refused_records_rotate_at_a_fixed_count() {
        let root = temp_dir("rotate");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let cwd = temp_dir("rotatecwd");
        for i in 0..REFUSED_KEEP + 25 {
            desk.record(
                "s1",
                &cwd,
                &format!("git push --force origin b{i}"),
                &[hit()],
                "refused",
            )
            .expect("record");
        }
        let kept = desk.records().expect("records");
        assert_eq!(kept.len(), REFUSED_KEEP);
        assert!(kept.iter().all(|r| r.status == "refused"));
        let pending = desk
            .record("s1", &cwd, "git push --force origin main", &[hit()], "pending")
            .expect("pending");
        assert!(desk.pending().expect("pending").iter().any(|r| r.id == pending));
    }

    /// Pending requests rotate too: varying an inert argument files a new request each time,
    /// but the desk keeps at most [`PENDING_KEEP`] of them and never evicts an answered grant.
    #[test]
    fn pending_records_rotate_and_keep_answered_grants() {
        let root = temp_dir("rotatepending");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let cwd = temp_dir("rotatependingcwd");
        let granted = desk
            .record("s1", &cwd, "git push --force origin kept", &[hit()], "pending")
            .expect("record");
        desk.answer(&granted, "session").expect("answer");
        for i in 0..PENDING_KEEP + 25 {
            desk.record(
                "s1",
                &cwd,
                &format!("git push --force origin p{i}"),
                &[hit()],
                "pending",
            )
            .expect("record");
        }
        let kept = desk.records().expect("records");
        assert_eq!(
            kept.iter().filter(|r| r.status == "pending").count(),
            PENDING_KEEP
        );
        assert!(kept.iter().any(|r| r.id == granted && r.status == "session"));
    }

    /// An `always` answer is a grant in the run it was answered in. A later run (a new desk
    /// open) relies on the user settings instead, so removing the key there revokes it.
    #[test]
    fn an_always_record_from_an_earlier_run_needs_the_user_settings() {
        let root = temp_dir("alwaysrevoke");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let cwd = temp_dir("alwaysrevokecwd");
        let command = "git push --force origin main";
        let id = desk
            .record("s1", &cwd, command, &[hit()], "pending")
            .expect("record");
        desk.answer(&id, "always").expect("answer");
        let keys = vec![danger_key(&hit(), command)];
        assert!(desk.covers("s1", &cwd, &keys), "the answering run uses it");
        assert!(desk.granted("s1", &cwd, &keys));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let later = ApprovalDesk::open(&root).expect("desk again");
        assert!(
            !later.covers("s1", &cwd, &keys),
            "a later run must not bypass revocation"
        );
        assert!(!later.granted("s1", &cwd, &keys));
    }

    #[test]
    fn a_session_record_from_an_earlier_run_is_not_reused() {
        let root = temp_dir("session-run");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let cwd = temp_dir("session-run-cwd");
        let command = "git push --force origin main";
        let id = desk
            .record("s1", &cwd, command, &[hit()], "pending")
            .expect("record");
        let owner = ApprovalDesk::open(&root).expect("owner desk");
        owner.answer(&id, "session").expect("answer");
        let keys = vec![danger_key(&hit(), command)];
        assert!(desk.granted("s1", &cwd, &keys), "the answering run uses it");

        let later = ApprovalDesk::open(&root).expect("later run");
        assert!(
            !later.granted("s1", &cwd, &keys),
            "a resumed session must not inherit an earlier run's grant"
        );
    }

    #[test]
    fn wipe_removes_only_the_selected_sessions_orphaned_private_temporary_records() {
        let root = temp_dir("orphan-temp");
        let desk = ApprovalDesk::open(&root).expect("desk");
        let cwd = temp_dir("orphan-temp-cwd");
        let first = desk
            .record("s1", &cwd, "git push --force", &[hit()], "pending")
            .expect("record");
        let second = desk
            .record("s2", &cwd, "rm -rf build", &[hit()], "pending")
            .expect("record");
        let first_record = desk.load(&first).expect("load").expect("record");
        let second_record = desk.load(&second).expect("load").expect("record");
        let first_orphan = desk.dir.join(".0123456789abcdef.deadbeef.tmp");
        let second_orphan = desk.dir.join(".fedcba9876543210.deadbeef.tmp");
        fs::write(&first_orphan, first_record.to_value().to_string()).expect("first orphan");
        fs::write(&second_orphan, second_record.to_value().to_string()).expect("second orphan");

        assert_eq!(desk.forget_session("s1").expect("session wipe"), 2);
        assert!(
            !first_orphan.exists(),
            "session wipe retained its private temp data"
        );
        assert!(
            second_orphan.exists(),
            "session wipe removed another session's temp data"
        );

        assert_eq!(desk.forget_all().expect("wipe all"), 2);
        assert!(!second_orphan.exists(), "full wipe retained private temp data");
    }

    #[test]
    fn danger_keys_preserve_semantic_whitespace() {
        let command = "rm -rf /tmp/reviewed\\ ";
        let other = "rm -rf /tmp/reviewed\\\n";
        assert_ne!(danger_key(&hit(), command), danger_key(&hit(), other));
    }
}
