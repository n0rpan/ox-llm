//! Read-only sampling: this runner has no executor or GitHub mutation interface.
use crate::board::{
    landing,
    landing::Next,
    queue::{NotificationTarget, RejectionEvidence},
    review, Board, Entry,
};
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

mod landing_history;
use landing_history::{AdmissionEvidence, Admissions};

pub const INTERVAL: Duration = Duration::from_secs(300);
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_LEAD_SAMPLE_BYTES: u64 = 1_048_576;

/// Sanitized remaining quota for one GitHub API resource.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubWindow {
    pub limit: u64,
    pub remaining: u64,
    pub resets_at: u64,
}

/// Only the authenticated App Reader can populate this sample field.
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppQuota {
    pub core: Option<GitHubWindow>,
    pub graphql: Option<GitHubWindow>,
}

/// Read the owner's explicit report policy through the private facts storage checks.
pub fn read_queue_report_policy(path: &Path) -> Result<Vec<u8>> {
    let path = std::path::absolute(path).map_err(|_| "invalid queue report policy path")?;
    let name = path
        .file_name()
        .and_then(|file| file.to_str())
        .ok_or("invalid queue report policy file")?;
    let parent = path
        .parent()
        .ok_or("queue report policy needs a private directory")?;
    crate::facts_io::Root::open(parent, false)?.read(name, 16_384)
}

#[derive(Serialize)]
struct Sample<'a> {
    schema_version: u32,
    observed_at: u64,
    board: &'a Board,
    would_take: Vec<&'a Entry>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    reviews: &'a BTreeMap<u64, review::Plan>,
    #[serde(skip_serializing_if = "<[RejectionEvidence]>::is_empty")]
    queue_rejections: &'a [RejectionEvidence],
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    matched_queue_admissions: &'a BTreeMap<u64, AdmissionEvidence>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    landing: &'a BTreeMap<u64, LandingDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    landing_main: Option<&'a str>,
    /// Raw lead-panels repository record: main required checks, the verified merge
    /// and queue membership at this observation. The protected Lead view validates
    /// and derives its alarms from it; this runner never interprets it (#1391).
    #[serde(skip_serializing_if = "Option::is_none")]
    panels: Option<&'a Value>,
    /// Complete GitHub Actions pages sampled with `status=queued`, for the
    /// queued-run stall signal. Incomplete or oversized captures stay absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    runs: Option<&'a [Value]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_quota: Option<AppQuota>,
    #[serde(skip_serializing_if = "<[Deferral]>::is_empty")]
    quota_deferred: &'a [Deferral],
}

pub(crate) const OWNER_FILE_FLAGS: libc::c_int = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

pub(crate) fn regular_file_owned_by(metadata: &Metadata, owner: u32) -> bool {
    metadata.is_file() && metadata.uid() == owner
}

/// Refuse links, shared files and concurrent writers before consulting GitHub.
pub fn open_log(path: &Path) -> Result<File> {
    // SAFETY: geteuid has no preconditions and returns this process's effective owner.
    open_log_for_owner(path, unsafe { libc::geteuid() })
}

/// An owner-private append log for a worker's stdout and stderr: created 0600, never opened
/// through a symlink, and refused unless it is an owner-only regular file with one link.
pub fn open_owner_log(path: &Path) -> Result<File> {
    // SAFETY: geteuid has no preconditions and returns this process's effective owner.
    let owner = unsafe { libc::geteuid() };
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OWNER_FILE_FLAGS)
        .open(path)
        .map_err(|_| "cannot open private worker log")?;
    let metadata = file.metadata().map_err(|_| "cannot inspect worker log")?;
    if !regular_file_owned_by(&metadata, owner)
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("worker log must be a private regular file with one link".into());
    }
    Ok(file)
}

fn open_log_for_owner(path: &Path, owner: u32) -> Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OWNER_FILE_FLAGS)
        .open(path)
        .map_err(|_| "cannot open private shadow log")?;
    let metadata = file.metadata().map_err(|_| "cannot inspect shadow log")?;
    if !regular_file_owned_by(&metadata, owner)
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("shadow log must be a private regular file with one link".into());
    }
    file.try_lock().map_err(|_| "shadow log is already in use")?;
    recover_tail(&mut file, path)?;
    Ok(file)
}

/// Preserve interrupted bytes durably before removing them from the JSONL stream.
fn recover_tail(file: &mut File, path: &Path) -> Result<()> {
    let length = file.metadata().map_err(|_| "cannot inspect shadow log")?.len();
    if length > MAX_LOG_BYTES {
        return Err("shadow log reached its size limit; rotate it while stopped".into());
    }
    if length == 0 {
        return Ok(());
    }
    let mut cursor = length;
    let cut = loop {
        let start = cursor.saturating_sub(8192);
        let mut chunk = vec![0; (cursor - start) as usize];
        file.seek(SeekFrom::Start(start))
            .map_err(|_| "cannot seek shadow log")?;
        file.read_exact(&mut chunk)
            .map_err(|_| "cannot inspect shadow tail")?;
        if let Some(offset) = chunk.iter().rposition(|b| *b == b'\n') {
            break start + offset as u64 + 1;
        }
        if start == 0 {
            break 0;
        }
        cursor = start;
    };
    if cut == length {
        return Ok(());
    }
    let mut fragment = vec![0; (length - cut) as usize];
    file.seek(SeekFrom::Start(cut))
        .map_err(|_| "cannot seek shadow tail")?;
    file.read_exact(&mut fragment)
        .map_err(|_| "cannot read shadow tail")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "invalid clock")?
        .as_nanos();
    let quarantine = path.with_extension(format!("jsonl.incomplete-{}-{nonce}", std::process::id()));
    let mut saved = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(quarantine)
        .map_err(|_| "cannot preserve shadow tail")?;
    saved
        .write_all(&fragment)
        .and_then(|_| saved.sync_all())
        .map_err(|_| "cannot preserve shadow tail")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| "cannot sync preserved shadow tail")?;
    file.set_len(cut)
        .and_then(|_| file.sync_data())
        .map_err(|_| "cannot restore shadow JSONL boundary".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("shadow-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn owner_mismatch_is_refused_before_logging() {
        let path = path("owner");
        drop(open_log(&path).unwrap());
        let other = std::fs::metadata(&path).unwrap().uid().wrapping_add(1);
        let refused = open_log_for_owner(&path, other).is_err();
        std::fs::remove_file(path).unwrap();
        assert!(refused);
    }

    #[test]
    fn an_oversized_sample_drops_queued_run_pages_before_the_app_quota() {
        if crate::lock_process::in_parent() {
            return;
        }
        let path = path("degrade");
        let mut log = open_log(&path).unwrap();
        let window = GitHubWindow {
            limit: 5000,
            remaining: 4000,
            resets_at: 600,
        };
        let page = serde_json::json!({"workflow_runs": "x".repeat(120_000)});
        run_with_landing(
            &mut log,
            true,
            || {
                Ok(LandingSample {
                    runs: Some(vec![page.clone(); 9]),
                    app_quota: Some(AppQuota {
                        core: Some(window),
                        graphql: None,
                    }),
                    ..LandingSample::default()
                })
            },
            |_| {},
        )
        .unwrap();
        let line: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(line.get("runs").is_none());
        assert_eq!(line["app_quota"]["core"]["remaining"], 4000);
    }

    fn snapshot_bytes() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "observed_at": 0,
            "board": Board::default()
        }))
        .unwrap()
    }

    #[test]
    fn admission_replay_rejects_and_open_log_preserves_interrupted_tail_bytes() {
        if crate::lock_process::in_parent() {
            return;
        }
        let full = snapshot_bytes();
        for fragment in [full.as_slice(), &full[..full.len() / 2]] {
            let dir = path("interrupted-admission");
            std::fs::create_dir(&dir).unwrap();
            let journal = dir.join("owner.jsonl");
            let mut log = open_log(&journal).unwrap();
            log.write_all(&full).unwrap();
            log.write_all(b"\n").unwrap();
            log.write_all(fragment).unwrap();
            assert!(Admissions::load(&mut log).is_err());
            drop(log);

            let mut log = open_log(&journal).unwrap();
            assert!(Admissions::load(&mut log).is_ok());
            drop(log);
            assert_eq!(std::fs::read(&journal).unwrap(), [&full[..], b"\n"].concat());
            let tail = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path != &journal)
                .unwrap();
            assert_eq!(std::fs::read(&tail).unwrap(), fragment);
            std::fs::remove_file(tail).unwrap();
            std::fs::remove_file(journal).unwrap();
            std::fs::remove_dir(dir).unwrap();
        }
    }

    #[test]
    fn admission_replay_refuses_multiple_objects_on_one_complete_line() {
        if crate::lock_process::in_parent() {
            return;
        }
        let journal = path("corrupt-admission");
        let snapshot = snapshot_bytes();
        let line = [&snapshot[..], &snapshot[..], b"\n"].concat();
        let mut log = open_log(&journal).unwrap();
        log.write_all(&line).unwrap();
        drop(log);

        let mut log = open_log(&journal).unwrap();
        assert!(Admissions::load(&mut log).is_err());
        drop(log);
        assert_eq!(std::fs::read(&journal).unwrap(), line);
        std::fs::remove_file(journal).unwrap();
    }

    #[test]
    fn a_full_log_is_not_extended() {
        let path = path("limit");
        let mut log = open_log(&path).unwrap();
        log.set_len(MAX_LOG_BYTES).unwrap();
        let refused = run(
            &mut log,
            true,
            || Ok(Board::default()),
            |_| panic!("unexpected wait"),
        )
        .is_err();
        let unchanged = log.metadata().unwrap().len() == MAX_LOG_BYTES;
        drop(log);
        std::fs::remove_file(path).unwrap();
        assert!(refused && unchanged);
    }
}

/// Sample immediately, then at five-minute intervals. Fail closed on read/log errors.
/// The callback returns the shared board, including its eligibility decisions.
pub fn run(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<Board>,
    wait: impl FnMut(Duration),
) -> Result<()> {
    run_with_reviews(log, once, || Ok((snapshot()?, BTreeMap::new())), wait)
}

pub fn run_with_reviews(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<(Board, BTreeMap<u64, review::Plan>)>,
    wait: impl FnMut(Duration),
) -> Result<()> {
    run_with_landing(
        log,
        once,
        || {
            let (board, reviews) = snapshot()?;
            Ok(LandingSample {
                board,
                reviews,
                ..LandingSample::default()
            })
        },
        wait,
    )
}

/// Append verified Reader evidence in the existing private owner journal, in
/// the same durable sample as the board that supplied the read fences.
pub fn run_with_queue_evidence(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<(Board, Vec<RejectionEvidence>)>,
    wait: impl FnMut(Duration),
) -> Result<()> {
    run_with_landing(
        log,
        once,
        || {
            let (board, queue_rejections) = snapshot()?;
            Ok(LandingSample {
                board,
                queue_rejections,
                ..LandingSample::default()
            })
        },
        wait,
    )
}

#[derive(Serialize)]
pub struct LandingDecision {
    pub next: Next,
    pub queue_alarm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_intervention: Option<landing::HoldIntervention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueObservation>,
    #[serde(skip)]
    pub latest_reviews: landing::LatestReviews,
    /// Review threads read in the same batch as the review comments (#1549).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<ThreadHold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub would_notify_rejection: Option<RejectionNotice>,
}

/// The unresolved review threads of one PR, as read by the Reader. `incomplete` is
/// reported, never treated as zero: the page was cut off or a node was malformed.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ThreadHold {
    pub unresolved: u32,
    /// Unresolved and not outdated: the threads that hold landing.
    pub current: u32,
    /// Creation time (unix seconds) of the oldest unresolved thread; 0 when unknown.
    pub oldest_at: u64,
    /// Distinct thread authors, at most four.
    pub authors: Vec<String>,
    pub incomplete: bool,
}

/// Optional GitHub work the owner's App reserve kept back in one sample (#1413): a
/// recorded, non-fatal deferral that says why a source is absent.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Deferral {
    pub work: String,
    pub resource: String,
    /// The window reset the work waits for; absent when no window was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_observation: bool,
}

#[derive(Serialize)]
pub struct RejectionNotice {
    pr_head: String,
    merge_group_head: String,
    removal_event: String,
    target: NotificationTarget,
    failed_tests: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct QueueObservation {
    pub enqueued_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_head: Option<String>,
}

#[derive(Default)]
pub struct LandingSample {
    pub board: Board,
    pub reviews: BTreeMap<u64, review::Plan>,
    pub queue_rejections: Vec<RejectionEvidence>,
    pub landing: BTreeMap<u64, LandingDecision>,
    /// Main can advance independently of the queue or its merge-group commits.
    pub landing_main: Option<String>,
    pub panels: Option<Value>,
    pub runs: Option<Vec<Value>>,
    pub app_quota: Option<AppQuota>,
    pub quota_deferred: Vec<Deferral>,
}

/// Persist the board and its projections in one owner-only sample.
pub fn run_with_landing(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<LandingSample>,
    mut wait: impl FnMut(Duration),
) -> Result<()> {
    let mut history = None;
    loop {
        let started = Instant::now();
        let LandingSample {
            board,
            reviews,
            queue_rejections,
            mut landing,
            landing_main,
            panels,
            runs,
            app_quota,
            quota_deferred,
        } = snapshot()?;
        let matched_queue_admissions = if landing.is_empty() {
            BTreeMap::new()
        } else {
            if history.is_none() {
                history = Some(Admissions::load(log)?);
            }
            history.as_mut().expect("owner journal was loaded").matches(
                &board,
                landing_main.as_deref(),
                queue_rejections.iter().map(RejectionEvidence::admission_identity),
            )
        };
        plan_matched_rejections(
            &board,
            &mut landing,
            landing_main.as_deref().unwrap_or_default(),
            &queue_rejections,
            &matched_queue_admissions,
        );
        let mut sample = Sample {
            schema_version: 1,
            observed_at: board.observed_at,
            would_take: board.entries.iter().filter(|entry| entry.eligible).collect(),
            board: &board,
            reviews: &reviews,
            queue_rejections: &queue_rejections,
            matched_queue_admissions: &matched_queue_admissions,
            landing: &landing,
            landing_main: landing_main.as_deref(),
            panels: panels.as_ref(),
            runs: runs.as_deref(),
            app_quota,
            quota_deferred: &quota_deferred,
        };
        let mut line = serde_json::to_vec(&sample).map_err(|_| "cannot encode shadow board")?;
        // The Lead sample bound is fixed: the largest optional records degrade
        // first, and a degraded record is an explicit partial observation.
        // The App quota is small and decides whether the rest may be read: it goes last.
        for step in 0..4 {
            if line.len() as u64 <= MAX_LEAD_SAMPLE_BYTES {
                break;
            }
            match step {
                0 => sample.quota_deferred = &[],
                1 => sample.runs = None,
                2 => sample.panels = None,
                _ => sample.app_quota = None,
            }
            line = serde_json::to_vec(&sample).map_err(|_| "cannot encode shadow board")?;
        }
        if line.len() as u64 > MAX_LEAD_SAMPLE_BYTES {
            return Err("shadow sample exceeds the Lead sample limit".into());
        }
        line.push(b'\n');
        let length = log.metadata().map_err(|_| "cannot inspect shadow log")?.len();
        if length.saturating_add(line.len() as u64) > MAX_LOG_BYTES {
            return Err("shadow log reached its size limit; rotate it while stopped".into());
        }
        log.write_all(&line).map_err(|_| "cannot append shadow board")?;
        log.sync_data().map_err(|_| "cannot sync shadow board")?;
        if let Some(history) = &mut history {
            history.observe(
                &board,
                landing
                    .iter()
                    .map(|(number, decision)| (*number, decision.queue.as_ref())),
            );
        }
        if once {
            return Ok(());
        }
        wait(INTERVAL.saturating_sub(started.elapsed()));
    }
}

fn plan_matched_rejections(
    board: &Board,
    decisions: &mut BTreeMap<u64, LandingDecision>,
    main: &str,
    rejections: &[RejectionEvidence],
    admissions: &BTreeMap<u64, AdmissionEvidence>,
) {
    let rejections: BTreeMap<_, _> = rejections
        .iter()
        .map(|rejection| (rejection.admission_identity().number, rejection))
        .collect();
    for entry in &board.entries {
        let Some(decision) = decisions
            .get_mut(&entry.number)
            .filter(|decision| decision.queue.is_none())
        else {
            continue;
        };
        let (Some(admission), Some(rejection)) =
            (admissions.get(&entry.number), rejections.get(&entry.number))
        else {
            continue;
        };
        let failed_tests = rejection.reported_failure_names();
        let plan = landing::Policy { enabled: true }.plan(
            entry,
            decision.latest_reviews,
            landing::Queue::Rejected(landing::Rejection {
                head: &entry.head,
                main: admission.baseline_main(),
                merge_group_head: rejection.admission_identity().group,
                removal_event: rejection.removal_event(),
                cause: if failed_tests.is_empty() {
                    landing::RejectionCause::Unknown
                } else {
                    landing::RejectionCause::FailedTests(&failed_tests)
                },
                notified_event: None,
                retry: landing::Retry::Unknown,
            }),
            main,
            board.observed_at,
        );
        decision.next = plan.next;
        if let (
            Some(landing::Notice {
                head,
                merge_group_head,
                removal_event,
                cause: landing::RejectionCause::FailedTests(tests),
            }),
            Some(target),
        ) = (plan.notify_rejection, rejection.notification_target())
        {
            decision.would_notify_rejection = Some(RejectionNotice {
                pr_head: head.to_owned(),
                merge_group_head: merge_group_head.to_owned(),
                removal_event: removal_event.to_owned(),
                target: target.clone(),
                failed_tests: tests.iter().map(|name| (*name).to_owned()).collect(),
            });
        }
    }
}
