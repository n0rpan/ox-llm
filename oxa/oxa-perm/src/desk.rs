//! #1512 S5: the approval desk. A run with no one to ask (`!Ui::interactive`) files a dangerous
//! command here instead of pausing; the owner answers with `oxa send <session> --approve <id>`
//! (or the lead web page). Answers come only from the desk, never from the inbox text path.
//!
//! Storage: `<agent_dir>/approvals/`, mode 0700; one `<id>.json` per request, mode 0600. Files
//! are written under a private temporary name and renamed into place, so a symlink at a record
//! name is replaced, never followed. The desk refuses a symlinked agent directory or desk
//! directory, and one owned by another user.
use crate::danger::DangerHit;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory under the agent directory that holds the requests.
const DIR_NAME: &str = "approvals";
/// The answers the owner can give (`oxa send --approve <id> <answer>`).
const ANSWERS: &[&str] = &["once", "session", "always", "deny"];

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
                DirBuilder::new().mode(0o700).create(&dir)?;
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
        Ok(ApprovalDesk { dir })
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
        let keys: Vec<String> = hits.iter().map(DangerHit::key).collect();
        if status == "pending" {
            let open = self
                .all()?
                .into_iter()
                .find(|r| r.status == "pending" && r.session == session && r.keys == keys);
            if let Some(open) = open {
                return Ok(open.id);
            }
        }
        let rec = Record {
            id: random_hex(8),
            session: session.to_string(),
            cwd: cwd.display().to_string(),
            command: command.to_string(),
            keys,
            reasons: hits.iter().map(|h| format!("{}: {}", h.rule, h.reason)).collect(),
            status: status.to_string(),
            used: false,
            created_ms: now_ms(),
            answered_ms: None,
        };
        self.write(&rec)?;
        Ok(rec.id)
    }

    /// The owner's answer to a `pending` request. Any other status is refused.
    pub fn answer(&self, id: &str, status: &str) -> io::Result<Record> {
        if !valid_id(id) {
            return Err(invalid_input("invalid approval request id"));
        }
        if !ANSWERS.contains(&status) {
            return Err(invalid_input("answer must be once, session, always or deny"));
        }
        let mut rec = self
            .load(id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such approval request"))?;
        if rec.status != "pending" {
            return Err(invalid_input(&format!(
                "approval request {id} is {}, not pending",
                rec.status
            )));
        }
        rec.status = status.to_string();
        rec.answered_ms = Some(now_ms());
        self.write(&rec)?;
        Ok(rec)
    }

    /// `true` when a grant for `session` covers every key. `session` and `always` grants stay;
    /// a `once` grant is consumed here (marked used), so the next run asks again. Consumes
    /// nothing on `false`.
    pub fn granted(&self, session: &str, keys: &[String]) -> bool {
        if keys.is_empty() {
            return false;
        }
        let Ok(records) = self.all() else {
            return false;
        };
        for mut rec in records {
            if rec.session != session || !keys.iter().all(|k| rec.keys.contains(k)) {
                continue;
            }
            match rec.status.as_str() {
                "session" | "always" => return true,
                "once" if !rec.used => {
                    rec.used = true;
                    return self.write(&rec).is_ok();
                }
                _ => {}
            }
        }
        false
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
