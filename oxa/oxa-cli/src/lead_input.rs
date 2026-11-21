//! Bounded, read-only access to existing board samples and typed runner situation exports.
//! These functions return facts, never raw file content or input-bearing diagnostics.
use oxa_proto::work::{PullRequest, Snapshot, MAX_SNAPSHOT_BYTES};
use oxa_work::board::{Action, Board, MAX_OPEN_PULL_REQUESTS};
use oxa_work::shadow::{AppQuota, ThreadHold, MAX_LEAD_SAMPLE_BYTES};
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

/// One complete Reader sample: the shared board plus every typed stuck-work
/// source that record carries. Each alarm field is `None` when its source was
/// not observed; the caller renders that state explicitly (#1391).
pub struct Sample {
    pub board: Board,
    pub app_quota: Option<AppQuota>,
    pub alarms: crate::lead_page::Alarms,
    /// Unresolved review threads per PR from the landing record; `None` when the
    /// sample carries no landing record.
    pub threads: Option<std::collections::BTreeMap<u64, ThreadHold>>,
    /// PRs whose review decision is `requester_rejected`: the reviewer refused the
    /// configured requester account.
    pub review_rejected: Vec<u64>,
    /// Optional GitHub work the owner's App reserve kept back from this sample.
    pub quota_deferred: Vec<oxa_work::shadow::Deferral>,
    /// Verified main merges observed across the complete Reader records in the
    /// same tail (#2346): the throughput projection's only input, no new read.
    pub merges: crate::lead_projection::MergeHistory,
    /// PRs held by a `Restricts:` line without the `owner-accepted` label, as the
    /// Reader recorded them. `None` when the record does not carry the field.
    pub restricts_held: Option<Vec<u64>>,
}

/// Read the most recent complete Reader sample within the existing tail bound.
/// The board and its bounds stay strict: a malformed board refuses the record.
/// Alarm sources fail per source, so one missing or invalid source never hides
/// the others or the board itself.
pub fn sample(path: &Path, uid: u32) -> Result<Sample> {
    let mut file = open(path, uid)?;
    let length = file.metadata().map_err(|_| "cannot inspect board sample")?.len();
    let start = length.saturating_sub(MAX_LEAD_SAMPLE_BYTES + 1);
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
    // Complete lines in order. A tail that starts mid-record drops its first
    // partial line; the coverage of the merge history starts at the first whole one.
    let offset = if start == 0 {
        0
    } else {
        tail.iter().position(|b| *b == b'\n').map_or(end + 1, |p| p + 1)
    };
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut pos = offset;
    while pos <= end {
        let nl = tail[pos..=end]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(end, |p| pos + p);
        lines.push(&tail[pos..nl]);
        pos = nl + 1;
    }
    // The latest complete Reader sample, with the same strict bounds as before:
    // a malformed record at or after it refuses the read; publication rows are
    // journal replays, never observations.
    let mut latest = None;
    for (index, line) in lines.iter().enumerate().rev() {
        if line.len() as u64 > MAX_LEAD_SAMPLE_BYTES {
            return Err("board sample exceeds limit");
        }
        let record: serde_json::Value = serde_json::from_slice(line).map_err(|_| "invalid board sample")?;
        let decoded = decode_board_sample(&record)?;
        if !decoded.publication {
            latest = Some((index, record, decoded));
            break;
        }
    }
    let (index, record, decoded) = match latest {
        Some(found) => found,
        // The tail began mid-record and no whole Reader sample fits in it: the
        // observation the page needs exceeds the retained window.
        None if start != 0 => return Err("board sample exceeds limit"),
        None => return Err("no Reader board sample"),
    };
    // Merge history from the same tail, one light parse per record and no new
    // read. Older records degrade per record: a malformed or partial one is
    // skipped for the history, never a refusal of the board the latest record
    // carries. The row deserializer ignores the large board and landing subtrees
    // without allocating them, so the history stays cheap on a full tail.
    #[derive(serde::Deserialize)]
    struct HistoryRow {
        schema_version: u64,
        observed_at: u64,
        #[serde(default)]
        landing_main: Option<String>,
        #[serde(default)]
        panels: Option<serde_json::Value>,
        #[serde(default)]
        landing_publication: Option<serde_json::Value>,
        /// Presence of a board object, without allocating its subtree.
        #[serde(default, deserialize_with = "board_object")]
        board: bool,
    }
    fn board_object<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<bool, D::Error> {
        struct Present;
        impl<'de> serde::de::Visitor<'de> for Present {
            type Value = bool;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a board object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<bool, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(true)
            }
        }
        deserializer.deserialize_any(Present)
    }
    let mut merges = crate::lead_projection::MergeHistory {
        observed_at: decoded.board.observed_at,
        ..crate::lead_projection::MergeHistory::default()
    };
    let mut seen = std::collections::BTreeSet::new();
    for line in &lines[..=index] {
        if line.len() as u64 > MAX_LEAD_SAMPLE_BYTES {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<HistoryRow>(line) else {
            continue;
        };
        if row.schema_version != 1 || row.landing_publication.is_some() || !row.board {
            continue;
        }
        if merges.covered_from.is_none() {
            merges.covered_from = Some(row.observed_at);
        }
        let Some(head) = row.landing_main.as_deref() else {
            continue;
        };
        merges.main_samples += 1;
        let Some(panels) = row.panels.as_ref() else {
            continue;
        };
        // Count a merge only when the record's own main head is the verified
        // merge commit; the shared validator keeps its exact-match rules.
        if panels["main"]["target"]["oid"].as_str() != Some(head) {
            continue;
        }
        let Some(merged_at) = crate::lead_panels::verified_main_merge_time(panels) else {
            continue;
        };
        if seen.insert(head.to_owned()) {
            merges.merges.push(crate::lead_projection::Merge {
                at: merged_at,
                head: head.to_owned(),
            });
        }
    }
    merges.merges.sort_by_key(|merge| merge.at);
    let (alarms, threads) = decode_alarms(&record, &decoded.board, decoded.board.observed_at);
    Ok(Sample {
        board: decoded.board,
        app_quota: decoded.app_quota,
        alarms,
        threads,
        review_rejected: record["reviews"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, plan)| plan["decision"].get("requester_rejected").is_some())
            .filter_map(|(number, _)| number.parse().ok())
            .collect(),
        quota_deferred: serde_json::from_value(record["quota_deferred"].clone()).unwrap_or_default(),
        merges,
        restricts_held: record
            .get("restricts_held")
            .and_then(serde_json::Value::as_array)
            .and_then(|rows| rows.iter().map(serde_json::Value::as_u64).collect()),
    })
}

/// Read the most recent complete Reader sample's board and App quota.
pub(crate) fn board_and_app_quota(path: &Path, uid: u32) -> Result<(Board, Option<AppQuota>)> {
    let record = sample(path, uid)?;
    Ok((record.board, record.app_quota))
}

/// Fixed, path-free operator causes for a failed protected observation read.
/// Exposed to the view (#2342) so every gap names its cause from this allowlist,
/// never a raw diagnostic, path or file content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardCause {
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
    pub fn text(self) -> &'static str {
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
    pub fn hint(self) -> &'static str {
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

struct DecodedSample {
    board: Board,
    app_quota: Option<AppQuota>,
    publication: bool,
}

/// One landing record row as the protected view needs it: the queue-age alarm the
/// landing policy already decided, and the queue observation that established it.
#[derive(serde::Deserialize)]
struct LandingRow {
    queue_alarm: bool,
    queue: Option<oxa_work::shadow::QueueObservation>,
    #[serde(default)]
    threads: Option<ThreadHold>,
}

/// Decode every alarm source one record carries. Each source is independent: a
/// missing or invalid record leaves that source unobserved (`None`) instead of
/// failing the board, and the shared parsers keep their own policy results
/// without this module recomputing any age.
fn decode_alarms(
    record: &serde_json::Value,
    board: &Board,
    observed_at: u64,
) -> (
    crate::lead_page::Alarms,
    Option<std::collections::BTreeMap<u64, ThreadHold>>,
) {
    let panels = record
        .get("panels")
        .filter(|value| !value.is_null())
        .and_then(|value| crate::lead_panels::parse_repository(value, None).ok());
    let (main_red, main_merge_stalled, queue) = match &panels {
        None => (None, None, None),
        Some(panels) => (
            Some(panels.main_red),
            panels
                .main_merged_at()
                .map(|_| panels.main_merge_stalled(observed_at)),
            panels.queue.clone(),
        ),
    };
    // Bind each queue-age alarm to the exact PR head of the same record's Board
    // entry, and require the landing record's own queue observation.
    let rows: Option<std::collections::BTreeMap<u64, LandingRow>> = record
        .get("landing")
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let overdue_queue = rows.as_ref().map(|rows| {
        board
            .entries
            .iter()
            .filter(|entry| {
                rows.get(&entry.number)
                    .is_some_and(|row| row.queue_alarm && row.queue.is_some())
            })
            .map(|entry| PullRequest {
                number: entry.number,
                head: entry.head.clone(),
            })
            .collect()
    });
    let threads = rows.map(|rows| {
        rows.into_iter()
            .filter_map(|(number, row)| Some((number, row.threads?)))
            .collect()
    });
    let stalled_runs = record
        .get("runs")
        .and_then(serde_json::Value::as_array)
        .and_then(|pages| crate::lead_runs::stalled(pages, observed_at).ok());
    let alarms = crate::lead_page::Alarms {
        main_red,
        main_merge_stalled,
        stalled_runs,
        queue,
        overdue_queue,
    };
    (alarms, threads)
}

fn decode_board_sample(sample: &serde_json::Value) -> Result<DecodedSample> {
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
    Ok(DecodedSample {
        board,
        app_quota,
        publication,
    })
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
            [
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
            [
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
                    evidence_incomplete: None,
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
    fn merge_history_comes_from_the_same_tail_and_counts_each_head_once() {
        use serde_json::json;
        let iso = |secs: u64| format!("{}Z", &oxa_session::iso_from_ms(secs * 1000)[..19]);
        let head_a = "a".repeat(40);
        let head_b = "b".repeat(40);
        let record = |at: u64, head: &str, merged: Option<u64>| {
            let mut rec = json!({
                "schema_version": 1,
                "observed_at": at,
                "board": {"observed_at": at, "entries": []},
                "landing_main": head,
            });
            if let Some(merged) = merged {
                rec["panels"] = json!({
                    "id": "example-repo",
                    "main": {"target": {"oid": head, "associatedPullRequests": {
                        "pageInfo": {"hasNextPage": false},
                        "nodes": [{"repository": {"id": "example-repo"}, "baseRefName": "main",
                                   "mergedAt": iso(merged), "mergeCommit": {"oid": head}}]
                    }}},
                });
            }
            rec
        };
        let env = TestEnv::new();
        let lines = [
            record(36_000, &head_a, Some(35_500)),
            record(39_600, &head_a, Some(35_500)), // same head: not a second merge
            record(43_200, &head_b, Some(42_700)),
            record(46_800, &head_b, None), // panels absent: still a main sample
        ];
        let content = lines.iter().map(|rec| format!("{rec}\n")).collect::<String>();
        let (path, uid) = fixture(&env, content.as_bytes());
        let sample = sample(&path, uid).unwrap();
        assert_eq!(sample.board.observed_at, 46_800);
        assert_eq!(sample.merges.covered_from, Some(36_000));
        assert_eq!(sample.merges.observed_at, 46_800);
        assert_eq!(sample.merges.main_samples, 4);
        assert_eq!(
            sample.merges.merges,
            vec![
                crate::lead_projection::Merge {
                    at: 35_500,
                    head: head_a.clone()
                },
                crate::lead_projection::Merge {
                    at: 42_700,
                    head: head_b.clone()
                },
            ]
        );
        // A record whose panels name a different head is not a verified merge of it.
        let mut mismatched = record(50_400, &head_a, Some(49_000));
        mismatched["panels"]["main"]["target"]["oid"] = json!(head_b);
        let content = format!("{content}{mismatched}\n");
        fs::write(&path, &content).unwrap();
        let sample = sample(&path, uid).unwrap();
        assert_eq!(sample.merges.merges.len(), 2);
        assert_eq!(sample.merges.main_samples, 5);
    }

    #[test]
    fn history_skips_publication_and_malformed_older_records_without_refusing() {
        let env = TestEnv::new();
        let reader = br#"{"schema_version":1,"observed_at":20,"board":{"observed_at":20,"entries":[]},"landing_main":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
"#;
        // An older malformed record never refuses the board the latest record carries.
        let content = [b"broken older record\n".as_slice(), reader].concat();
        let (path, uid) = fixture(&env, &content);
        let sample = sample(&path, uid).unwrap();
        assert_eq!(sample.board.observed_at, 20);
        assert_eq!(sample.merges.covered_from, Some(20));
        assert_eq!(sample.merges.main_samples, 1);
        assert!(sample.merges.merges.is_empty());
        // Publication rows are journal replays: no merge history from them.
        let publication = br#"{"schema_version":1,"observed_at":30,"board":{"observed_at":30,"entries":[]},"landing_publication":{"step":"delivered"},"landing_main":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
"#;
        fs::write(&path, [reader, publication].concat()).unwrap();
        let sample = sample(&path, uid).unwrap();
        assert_eq!(sample.board.observed_at, 20);
        assert_eq!(sample.merges.main_samples, 1);
    }

    #[test]
    fn restricts_held_is_observed_only_when_the_record_carries_it() {
        let env = TestEnv::new();
        // The root object stays open so each case can append its own field.
        let base = r#"{"schema_version":1,"observed_at":20,"board":{"observed_at":20,"entries":[]}"#;
        let (path, uid) = fixture(&env, format!("{base}}}\n").as_bytes());
        assert_eq!(sample(&path, uid).unwrap().restricts_held, None);
        let with_holds = format!("{base},\"restricts_held\":[7,9]}}\n");
        fs::write(&path, &with_holds).unwrap();
        assert_eq!(sample(&path, uid).unwrap().restricts_held, Some(vec![7, 9]));
        let empty = format!("{base},\"restricts_held\":[]}}\n");
        fs::write(&path, &empty).unwrap();
        assert_eq!(sample(&path, uid).unwrap().restricts_held, Some(vec![]));
        // A malformed list degrades this one source to unobserved, not a refusal.
        let invalid = format!("{base},\"restricts_held\":[7,\"x\"]}}\n");
        fs::write(&path, &invalid).unwrap();
        assert_eq!(sample(&path, uid).unwrap().restricts_held, None);
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
