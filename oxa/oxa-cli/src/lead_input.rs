//! Bounded, read-only access to existing board samples and typed runner situation exports.
//! These functions return facts, never raw file content or input-bearing diagnostics.
use oxa_proto::work::{Snapshot, MAX_SNAPSHOT_BYTES};
use oxa_work::board::{Action, Board, MAX_OPEN_PULL_REQUESTS};
use oxa_work::shadow::{AppQuota, MAX_LEAD_SAMPLE_BYTES};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

// Retain the previous board row ceiling for issue intake.
const MAX_ISSUE_ROWS: usize = 128;
type Result<T> = std::result::Result<T, &'static str>;

/// The caller supplies the owner's uid from its user configuration, never project metadata.
/// Sources must have private parent directories; producers may atomically replace files.
pub(crate) fn open(path: &Path, uid: u32) -> Result<File> {
    let parent = path.parent().ok_or("missing input directory")?;
    let directory = match fs::symlink_metadata(parent) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("input missing"),
        Err(_) => return Err("input directory unavailable"),
    };
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
        return Err("input directory must be owner-only");
    }
    let before = match fs::symlink_metadata(path) {
        Ok(before) => before,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("input missing"),
        Err(_) => return Err("input unavailable"),
    };
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

#[cfg(test)]
pub fn board(path: &Path, uid: u32) -> Result<Board> {
    board_and_app_quota(path, uid).map(|(board, _)| board)
}

/// Read the most recent complete Reader sample within the existing tail bound.
pub(crate) fn board_and_app_quota(path: &Path, uid: u32) -> Result<(Board, Option<AppQuota>)> {
    let mut file = open(path, uid)?;
    let length = file.metadata().map_err(|_| "cannot inspect board sample")?.len();
    let start = length.saturating_sub(MAX_LEAD_SAMPLE_BYTES + 1);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "cannot seek board sample")?;
    let mut tail = Vec::new();
    file.take(length - start)
        .read_to_end(&mut tail)
        .map_err(|_| "cannot read board sample")?;
    let mut end = tail
        .iter()
        .rposition(|b| *b == b'\n')
        .ok_or("no complete board sample")?;
    loop {
        let previous = tail[..end].iter().rposition(|b| *b == b'\n');
        if previous.is_none() && start != 0 {
            return Err("board sample exceeds limit");
        }
        let line = &tail[previous.map_or(0, |p| p + 1)..end];
        let (board, app_quota, publication) = decode_board_sample(line)?;
        if !publication {
            return Ok((board, app_quota));
        }
        // Publication rows carry a Board for journal replay, not a Reader observation.
        end = previous.ok_or("no Reader board sample")?;
    }
}

/// Fixed, path-free operator causes for a failed protected observation read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoardCause {
    Missing,
    Changed,
    Unreadable,
    Malformed,
    Oversized,
    Unsupported,
    Unavailable,
}

impl BoardCause {
    /// Authenticated operator status text. Never a path, file content or provider error.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Missing => "the protected Board observation is missing",
            Self::Changed => "the protected Board observation changed while it was being read",
            Self::Unreadable => "the protected Board observation is not readable by this owner",
            Self::Malformed => "the protected Board observation is not a valid board sample",
            Self::Oversized => "the protected Board observation exceeds the sample limit",
            Self::Unsupported => "the protected Board observation uses an unsupported version",
            Self::Unavailable => "the protected Board observation is unavailable",
        }
    }

    /// The operator's next step for this cause.
    pub(crate) fn hint(self) -> &'static str {
        match self {
            Self::Unreadable => {
                "check that the observation and its directory are owner-only, then rerun the Lead loop"
            }
            Self::Changed => "rerun the Lead loop to read the replacement observation",
            _ => "write a fresh protected Board observation and rerun the Lead loop",
        }
    }
}

/// Classify the resolver's fixed text for operator status. This match is the
/// allowlist: resolver texts are generated in this module, and any unrecognized
/// text stays a generic cause, so no raw diagnostic can reach output. Only an
/// explicit NotFound is reported as missing; ambiguous open failures are not.
pub(crate) fn board_cause(error: &'static str) -> BoardCause {
    match error {
        "input missing" => BoardCause::Missing,
        "input changed during open; retry later" => BoardCause::Changed,
        "input directory unavailable"
        | "input directory must be owner-only"
        | "input must be a private owner file"
        | "cannot inspect input"
        | "cannot inspect board sample"
        | "cannot seek board sample"
        | "cannot read board sample" => BoardCause::Unreadable,
        "no complete board sample"
        | "no Reader board sample"
        | "invalid board sample"
        | "invalid shared board"
        | "invalid board sample bounds"
        | "invalid board sample entries"
        | "invalid publication sample" => BoardCause::Malformed,
        "board sample exceeds limit" => BoardCause::Oversized,
        "unsupported board sample version" => BoardCause::Unsupported,
        _ => BoardCause::Unavailable,
    }
}

fn decode_board_sample(line: &[u8]) -> Result<(Board, Option<AppQuota>, bool)> {
    if line.len() as u64 > MAX_LEAD_SAMPLE_BYTES {
        return Err("board sample exceeds limit");
    }
    let sample: serde_json::Value = serde_json::from_slice(line).map_err(|_| "invalid board sample")?;
    if sample["schema_version"] != 1 {
        return Err("unsupported board sample version");
    }
    let board: Board = serde_json::from_value(sample["board"].clone()).map_err(|_| "invalid shared board")?;
    let issue_rows = board
        .entries
        .iter()
        .filter(|row| matches!(row.action, Action::BuildIssue | Action::WaitIssue))
        .count();
    if sample["observed_at"].as_u64() != Some(board.observed_at)
        || issue_rows > MAX_ISSUE_ROWS
        || board.entries.len() - issue_rows > MAX_OPEN_PULL_REQUESTS
    {
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
    let app_quota = sample
        .get("app_quota")
        .and_then(|quota| serde_json::from_value(quota.clone()).ok());
    let publication = match sample.get("landing_publication") {
        Some(record) if record.is_object() => true,
        Some(_) => return Err("invalid publication sample"),
        None => false,
    };
    Ok((board, app_quota, publication))
}

/// Consume the runner's existing versioned situation document, not worker transcripts/logs.
pub fn situation(path: &Path, uid: u32) -> Result<Snapshot> {
    let file = open(path, uid)?;
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read situation")?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err("runner situation exceeds limit");
    }
    Snapshot::from_json(&bytes).map_err(|_| "invalid runner situation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::{symlink, PermissionsExt};

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
    fn publication_rows_do_not_refresh_or_hide_reader_samples() {
        let env = TestEnv::new();
        let reader = br#"{"schema_version":1,"observed_at":20,"board":{"observed_at":20,"entries":[]},"app_quota":{"core":{"limit":5000,"remaining":123,"resets_at":600}}}
"#;
        let publications = br#"{"schema_version":1,"observed_at":30,"board":{"observed_at":30,"entries":[]},"landing_publication":{"step":"delivered"}}
"#;
        let (path, uid) = fixture(&env, &[reader.as_slice(), publications.as_slice()].concat());
        let (board, quota) = board_and_app_quota(&path, uid).unwrap();
        assert_eq!(board.observed_at, 20);
        assert_eq!(quota.unwrap().core.unwrap().remaining, 123);

        let without_quota = br#"{"schema_version":1,"observed_at":25,"board":{"observed_at":25,"entries":[]}}
"#;
        fs::write(
            &path,
            &[
                reader.as_slice(),
                without_quota.as_slice(),
                publications.as_slice(),
            ]
            .concat(),
        )
        .unwrap();
        let (board, quota) = board_and_app_quota(&path, uid).unwrap();
        assert_eq!(board.observed_at, 25);
        assert!(quota.is_none());

        fs::write(&path, publications).unwrap();
        assert_eq!(
            board_and_app_quota(&path, uid).err(),
            Some("no Reader board sample")
        );

        let padded_publication = format!(
            "{}\n",
            serde_json::json!({
                "schema_version": 1,
                "observed_at": 40,
                "board": {"observed_at": 40, "entries": []},
                "landing_publication": {"step": "delivered"},
                "padding": "x".repeat(MAX_LEAD_SAMPLE_BYTES as usize * 3 / 5),
            })
        );
        fs::write(
            &path,
            &[
                reader.as_slice(),
                padded_publication.as_bytes(),
                padded_publication.as_bytes(),
            ]
            .concat(),
        )
        .unwrap();
        assert!(board_and_app_quota(&path, uid).is_err());
    }

    #[test]
    fn reads_a_full_paginated_shadow_board() {
        use oxa_work::board::Entry;

        let env = TestEnv::new();
        let (path, uid) = fixture(&env, b"");
        let mut snapshot = Board {
            observed_at: 20,
            entries: (1..=1_000)
                .map(|number| Entry {
                    number,
                    head: "a".repeat(40),
                    action: Action::Ready,
                    release_pinned: false,
                    lease: None,
                    attempts_today: 0,
                    eligible: true,
                })
                .collect(),
        };
        let write_sample = |snapshot: &Board| {
            let mut log = oxa_work::shadow::open_log(&path).unwrap();
            oxa_work::shadow::run(&mut log, true, || Ok(snapshot.clone()), |_| unreachable!()).unwrap();
        };
        write_sample(&snapshot);

        let read = board(&path, uid).expect("complete board sample is readable by lead");
        assert_eq!(read.observed_at, 20);
        assert_eq!(read.entries.len(), 1_000);
        assert_eq!(read.entries.last().unwrap().number, 1_000);

        let template = snapshot.entries[0].clone();
        snapshot.entries.extend((1_001..=1_128).map(|number| Entry {
            number,
            action: Action::BuildIssue,
            ..template.clone()
        }));
        write_sample(&snapshot);
        assert_eq!(board(&path, uid).unwrap().entries.len(), 1_128);

        snapshot.entries.push(Entry {
            number: 1_129,
            action: Action::WaitIssue,
            ..template.clone()
        });
        write_sample(&snapshot);
        assert_eq!(board(&path, uid).unwrap_err(), "invalid board sample bounds");

        snapshot.entries.last_mut().unwrap().action = Action::Ready;
        write_sample(&snapshot);
        assert_eq!(board(&path, uid).unwrap_err(), "invalid board sample bounds");
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
        let mut oversized = vec![b'x'; MAX_LEAD_SAMPLE_BYTES as usize + 2];
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

    #[test]
    fn situation_reports_inputs_over_the_size_limit() {
        let env = TestEnv::new();
        let source = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl");
        let expected = source
            .lines()
            .find_map(|line| Snapshot::from_json(line.as_bytes()).ok())
            .unwrap();
        let mut bytes = expected.to_json().unwrap();
        bytes.resize(MAX_SNAPSHOT_BYTES, b' ');
        let (path, uid) = fixture(&env, &bytes);
        assert_eq!(situation(&path, uid).unwrap(), expected);
        bytes.push(b' ');
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            situation(&path, uid).unwrap_err(),
            "runner situation exceeds limit"
        );
    }
}
