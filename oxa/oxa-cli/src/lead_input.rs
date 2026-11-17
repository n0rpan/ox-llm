//! Bounded, read-only access to existing board samples and typed runner situation exports.
//! These functions return facts, never raw file content or input-bearing diagnostics.
use oxa_proto::work::{MAX_ITEMS, MAX_SNAPSHOT_BYTES, Snapshot};
use oxa_work::board::Board;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

const MAX_SAMPLE: u64 = 1_048_576;
type Result<T> = std::result::Result<T, &'static str>;

/// The caller supplies the owner's uid from its user configuration, never project metadata.
/// Sources must have private parent directories; producers may atomically replace files.
pub(crate) fn open(path: &Path, uid: u32) -> Result<File> {
    let parent = path.parent().ok_or("missing input directory")?;
    let directory = fs::symlink_metadata(parent).map_err(|_| "input directory unavailable")?;
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
        return Err("input directory must be owner-only");
    }
    let before = fs::symlink_metadata(path).map_err(|_| "input unavailable")?;
    if !before.is_file() || before.uid() != uid || before.nlink() != 1 || before.mode() & 0o077 != 0 {
        return Err("input must be a private owner file");
    }
    let file = File::open(path).map_err(|_| "cannot open input")?;
    let after = file.metadata().map_err(|_| "cannot inspect input")?;
    if !after.is_file() || (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err("input changed during open; retry later");
    }
    Ok(file)
}

/// Read the most recent complete shared Board sample from the shadow producer's JSONL.
/// Ignore an in-progress/torn final append, but never skip a corrupt complete record.
pub fn board(path: &Path, uid: u32) -> Result<Board> {
    let mut file = open(path, uid)?;
    let length = file.metadata().map_err(|_| "cannot inspect board sample")?.len();
    let start = length.saturating_sub(MAX_SAMPLE + 1);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "cannot seek board sample")?;
    let mut tail = Vec::new();
    file.take(length - start)
        .read_to_end(&mut tail)
        .map_err(|_| "cannot read board sample")?;
    let end = tail
        .iter()
        .rposition(|b| *b == b'\n')
        .ok_or("no complete board sample")?;
    let previous = tail[..end].iter().rposition(|b| *b == b'\n');
    if previous.is_none() && start != 0 {
        return Err("board sample exceeds limit");
    }
    let line = &tail[previous.map_or(0, |p| p + 1)..end];
    if line.len() as u64 > MAX_SAMPLE {
        return Err("board sample exceeds limit");
    }
    let sample: serde_json::Value = serde_json::from_slice(line).map_err(|_| "invalid board sample")?;
    if sample["schema_version"] != 1 {
        return Err("unsupported board sample version");
    }
    let board: Board = serde_json::from_value(sample["board"].clone()).map_err(|_| "invalid shared board")?;
    if sample["observed_at"].as_u64() != Some(board.observed_at) || board.entries.len() > MAX_ITEMS {
        return Err("invalid board sample bounds");
    }
    let mut seen = std::collections::BTreeSet::new();
    if board
        .entries
        .iter()
        .any(|row| row.number == 0 || !seen.insert(row.number))
    {
        return Err("invalid board sample entries");
    }
    Ok(board)
}

/// Consume the runner's existing versioned situation document, not worker transcripts/logs.
pub fn situation(path: &Path, uid: u32) -> Result<Snapshot> {
    let file = open(path, uid)?;
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read situation")?;
    Snapshot::from_json(&bytes).map_err(|_| "invalid runner situation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture(env: &TestEnv, content: &[u8]) -> (std::path::PathBuf, u32) {
        let path = env.agent_dir().join("inputs/board.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = fs::metadata(env.home()).unwrap().uid();
        (path, uid)
    }
    #[test]
    fn uses_latest_shared_sample_and_tolerates_incomplete_append() {
        let env = TestEnv::new();
        let (path, uid) = fixture(
            &env,
            br#"{"schema_version":1,"observed_at":10,"board":{"observed_at":10,"entries":[]},"would_take":[]}
{"schema_version":1,"observed_at":20,"board":{"observed_at":20,"entries":[]},"would_take":[]}
{"schema_version":1,"observed_at":30,"board":{"#,
        );
        assert_eq!(board(&path, uid).unwrap().observed_at, 20);
        fs::write(&path, b"broken complete sample\n").unwrap();
        assert_eq!(board(&path, uid).unwrap_err(), "invalid board sample");
    }
    #[test]
    fn refuses_shared_links_wrong_owner_and_unbounded_inputs() {
        let env = TestEnv::new();
        let (path, uid) = fixture(&env, b"{}\n");
        assert!(board(&path, uid.wrapping_add(1)).is_err());
        let link = path.with_file_name("link");
        symlink(&path, &link).unwrap();
        assert!(board(&link, uid).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(board(&path, uid).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut oversized = vec![b'x'; MAX_SAMPLE as usize + 2];
        oversized.push(b'\n');
        fs::write(&path, oversized).unwrap();
        assert!(board(&path, uid).is_err());
        assert!(situation(&path, uid).is_err());
    }
    #[test]
    fn reads_existing_runner_wire_format_without_inventing_state() {
        let env = TestEnv::new();
        let source = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl");
        let snapshot = source
            .lines()
            .find_map(|s| Snapshot::from_json(s.as_bytes()).ok())
            .unwrap();
        let (path, uid) = fixture(&env, &snapshot.to_json().unwrap());
        assert_eq!(situation(&path, uid).unwrap(), snapshot);
        fs::write(&path, b"sensitive synthetic malformed data").unwrap();
        assert_eq!(situation(&path, uid).unwrap_err(), "invalid runner situation");
    }
}
