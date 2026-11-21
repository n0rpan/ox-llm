//! The durable kanban store: one owner-private directory per repository, holding `board.json`
//! (the whole board), `ops.jsonl` (the undo/audit/flow log) and `archive.jsonl` (removed items).
//! It reuses the private facts-root primitives: an atomic rename for the board, an flock for
//! writers, and synced append/rotate for the journals. A kill at any point leaves a whole board.
use super::model::*;
use super::ops;
use crate::facts_io::Root;
use oxa_proto::time::now_ms;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

const BOARD: &str = "board.json";
const LOCK: &str = ".kanban.lock";
const OPS: &str = "ops.jsonl";
const OPS_OLD: &str = "ops.1.jsonl";
const ARCHIVE: &str = "archive.jsonl";
const ARCHIVE_OLD: &str = "archive.1.jsonl";
const JOURNAL_READ_LIMIT: usize = 2 * MAX_OPS_BYTES as usize;

#[derive(Clone)]
pub struct Store {
    root: Root,
    dir: PathBuf,
}

impl Store {
    /// Open (or initialize) the board for one repository under an owner-private directory.
    /// The directory is created `0700` and validated by the private-root rules: owner uid,
    /// no group/other access, canonical, no symlinks. `repo` must match an existing board.
    pub fn open(dir: &Path, repo: &str) -> Result<Self, Error> {
        if let Some(parent) = dir.parent() {
            if !parent.exists() {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(parent)
                    .map_err(|e| Error::Storage(format!("cannot create kanban parent: {:?}", e.kind())))?;
            }
        }
        let root = Root::open(dir, true).map_err(Error::Storage)?;
        let store = Self {
            root,
            dir: dir.to_path_buf(),
        };
        let _lock = store.root.lock(LOCK).map_err(Error::Storage)?;
        if store.root.exists(BOARD).map_err(Error::Storage)? {
            let board = store.load()?;
            if board.repo != repo {
                return Err(Error::Storage(
                    "kanban directory belongs to a different repository".into(),
                ));
            }
        } else {
            store.save(&Kanban::new(repo))?;
        }
        store.ensure_journal(OPS)?;
        store.ensure_journal(ARCHIVE)?;
        Ok(store)
    }

    /// A revision-consistent read of the whole board under the lock.
    pub fn read(&self) -> Result<Kanban, Error> {
        let _lock = self.root.lock(LOCK).map_err(Error::Storage)?;
        self.load()
    }

    fn load(&self) -> Result<Kanban, Error> {
        let bytes = self.root.read(BOARD, MAX_BOARD_BYTES).map_err(Error::Storage)?;
        let board: Kanban =
            serde_json::from_slice(&bytes).map_err(|_| Error::Storage("kanban board is corrupt".into()))?;
        validate_board(&board).map_err(Error::Storage)?;
        Ok(board)
    }

    fn save(&self, board: &Kanban) -> Result<(), Error> {
        let bytes =
            serde_json::to_vec(board).map_err(|_| Error::Storage("cannot encode kanban board".into()))?;
        if bytes.len() > MAX_BOARD_BYTES {
            return Err(Error::Storage("kanban board byte capacity exceeded".into()));
        }
        self.root.atomic(BOARD, &bytes).map_err(Error::Storage)
    }

    /// Apply one typed operation as the given actor. The board is committed atomically first,
    /// then the op is logged; an op whose process dies before the log append is still live on
    /// reload (it simply cannot be undone). Undo is resolved here against the log.
    pub fn apply(&self, actor: Actor, op: Op) -> Result<Applied, Error> {
        let at = now_ms();
        let _lock = self.root.lock(LOCK).map_err(Error::Storage)?;
        let board = self.load()?;
        let outcome = match &op {
            Op::Undo { seq } => self.undo(&board, &actor, *seq, at)?,
            _ => ops::apply(&board, &actor, &op, at)?,
        };
        self.save(&outcome.board)?;
        let seq = outcome.board.revision;
        let entry = LogEntry {
            seq,
            at,
            actor,
            op,
            inverse: outcome.inverse,
        };
        self.append_log(&entry)?;
        self.append_archive(&outcome.archive)?;
        Ok(Applied { seq, revision: seq })
    }

    fn undo(&self, board: &Kanban, actor: &Actor, seq: u64, at: u64) -> Result<ops::Outcome, Error> {
        let entry = self.find_log(seq)?.ok_or_else(|| {
            Error::Refused("that op is not in the retained log; it is too old to undo".into())
        })?;
        if matches!(entry.op, Op::Undo { .. }) {
            return Err(Error::Refused("an undo cannot be undone".into()));
        }
        match actor {
            Actor::Owner => {}
            Actor::Lead if entry.actor == Actor::Lead => {}
            Actor::Lead => return Err(Error::Refused("the lead undoes only its own ops".into())),
            _ => return Err(Error::Refused("only the owner or lead undoes".into())),
        }
        ops::apply_inverse(board, actor, &entry.inverse, at)
    }

    fn append_log(&self, entry: &LogEntry) -> Result<(), Error> {
        let mut line =
            serde_json::to_vec(entry).map_err(|_| Error::Storage("cannot encode kanban op".into()))?;
        line.push(b'\n');
        self.root.append(OPS, &line).map_err(Error::Storage)?;
        if self.journal_len(OPS)? > MAX_OPS_BYTES {
            self.root.rotate(OPS, OPS_OLD).map_err(Error::Storage)?;
        }
        Ok(())
    }

    fn append_archive(&self, items: &[Item]) -> Result<(), Error> {
        if items.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for item in items {
            let mut line =
                serde_json::to_vec(item).map_err(|_| Error::Storage("cannot encode archived item".into()))?;
            line.push(b'\n');
            bytes.extend_from_slice(&line);
        }
        self.root.append(ARCHIVE, &bytes).map_err(Error::Storage)?;
        if self.journal_len(ARCHIVE)? > MAX_OPS_BYTES {
            self.root.rotate(ARCHIVE, ARCHIVE_OLD).map_err(Error::Storage)?;
        }
        Ok(())
    }

    fn journal_len(&self, leaf: &str) -> Result<u64, Error> {
        fs::metadata(self.dir.join(leaf))
            .map(|m| m.len())
            .map_err(|e| Error::Storage(format!("cannot inspect kanban journal: {:?}", e.kind())))
    }

    /// Create a journal if missing, and drop a torn trailing line left by a kill mid-append.
    fn ensure_journal(&self, leaf: &str) -> Result<(), Error> {
        if !self.root.exists(leaf).map_err(Error::Storage)? {
            return self.root.create(leaf, b"").map_err(Error::Storage);
        }
        let bytes = self.root.read(leaf, JOURNAL_READ_LIMIT).map_err(Error::Storage)?;
        let valid = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |pos| pos + 1);
        if valid < bytes.len() {
            self.root.truncate(leaf, valid as u64).map_err(Error::Storage)?;
        }
        Ok(())
    }

    fn find_log(&self, seq: u64) -> Result<Option<LogEntry>, Error> {
        for leaf in [OPS, OPS_OLD] {
            if !self.root.exists(leaf).map_err(Error::Storage)? {
                continue;
            }
            let bytes = self.root.read(leaf, JOURNAL_READ_LIMIT).map_err(Error::Storage)?;
            for line in bytes.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                // A torn tail line is skipped; it is recovered on the next open.
                if let Ok(entry) = serde_json::from_slice::<LogEntry>(line) {
                    if entry.seq == seq {
                        return Ok(Some(entry));
                    }
                }
            }
        }
        Ok(None)
    }
}
