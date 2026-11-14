//! Deterministic review selection and explicitly approved candidate staging.
//! No source mutation, model call, subprocess, Git hook or automatic merge.
use crate::workspace::{Grant, RelativePath, ReviewBundle, Snapshot};
use oxa_a2a::wire::{Event, Part, TaskState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{
    ffi::OsStrExt,
    fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
};
use std::path::Path;
use std::sync::Arc;

const MAX_REPORT: usize = 16 * 1024 * 1024;
const MAX_SELECTED: usize = 16;

#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    /// Controller worker identity, distinct from the native RPC task's local ID.
    pub worker: String,
    pub run: String,
    pub task_id: String,
    pub context_id: String,
    pub review: ReviewBundle,
}
#[derive(Clone, Debug, Serialize)]
pub struct Conflict {
    pub path: RelativePath,
    pub workers: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub version: u32,
    pub approval_sha256: String,
    pub source_manifest_sha256: String,
    pub candidate_manifest_sha256: Option<String>,
    pub new_files: Vec<RelativePath>,
    pub proposals: Vec<Proposal>,
    pub conflicts: Vec<Conflict>,
}

/// Immutable prepared content. Approval never trusts a caller-edited preview.
pub struct PreparedReview {
    preview: Preview,
    candidate: Option<Snapshot>,
}
impl PreparedReview {
    pub fn prepare(
        snapshot: Arc<Snapshot>,
        mut new_files: Vec<RelativePath>,
        mut proposals: Vec<Proposal>,
    ) -> io::Result<Self> {
        if proposals.is_empty() || proposals.len() > MAX_SELECTED {
            return Err(invalid("select 1..16 proposals"));
        }
        new_files.sort();
        if new_files.windows(2).any(|p| p[0] == p[1]) {
            return Err(invalid("duplicate new-file scope"));
        }
        Grant::editing(snapshot.clone(), new_files.clone())?;
        proposals.sort_by(|a, b| a.worker.cmp(&b.worker));
        if proposals.windows(2).any(|p| p[0].worker == p[1].worker) {
            return Err(invalid("duplicate selected worker"));
        }
        let mut paths: BTreeMap<RelativePath, Vec<String>> = BTreeMap::new();
        let mut changes = Vec::new();
        for proposal in &mut proposals {
            if !valid_worker(&proposal.worker)
                || !valid_worker(&proposal.run)
                || !valid_task_id(&proposal.task_id)
                || !valid_task_id(&proposal.context_id)
            {
                return Err(invalid("invalid selected worker identity"));
            }
            proposal.review.verify(&snapshot, &new_files)?;
            if proposal.review.changes.is_empty() {
                return Err(invalid("selected proposal has no changes"));
            }
            proposal.review.changes.sort_by(|a, b| a.path.cmp(&b.path));
            for change in &proposal.review.changes {
                paths
                    .entry(change.path.clone())
                    .or_default()
                    .push(proposal.worker.clone());
                changes.push(change.clone());
            }
        }
        let conflicts = paths
            .into_iter()
            .filter(|(_, workers)| workers.len() > 1)
            .map(|(path, workers)| Conflict { path, workers })
            .collect::<Vec<_>>();
        // Even identical edits overlap: the operator must explicitly select one.
        let candidate = if conflicts.is_empty() {
            Some(snapshot.with_changes(&changes)?)
        } else {
            None
        };
        let mut preview = Preview {
            version: 1,
            approval_sha256: String::new(),
            source_manifest_sha256: snapshot.manifest_sha256().into(),
            candidate_manifest_sha256: candidate.as_ref().map(|s| s.manifest_sha256().into()),
            new_files,
            proposals,
            conflicts,
        };
        preview.approval_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&preview)?));
        Ok(Self { preview, candidate })
    }
    pub fn preview(&self) -> &Preview {
        &self.preview
    }

    /// Stage approved contents in a NEW directory, never into the source tree.
    /// Publication is a no-replace rename. No partial candidate is published.
    pub fn stage(&self, directory: &Path, approval_sha256: &str) -> io::Result<()> {
        if approval_sha256 != self.preview.approval_sha256 {
            return Err(invalid("approval does not match the exact prepared content"));
        }
        let candidate = self
            .candidate
            .as_ref()
            .ok_or_else(|| invalid("overlapping proposals require an explicit different selection"))?;
        let parent = directory
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?;
        let raw_name = directory
            .as_os_str()
            .as_bytes()
            .rsplit(|b| *b == b'/')
            .find(|part| !part.is_empty());
        if matches!(raw_name, Some(b"." | b"..")) {
            return Err(invalid("candidate directory needs a new name"));
        }
        let name = directory
            .file_name()
            .ok_or_else(|| invalid("candidate directory needs a new name"))?;
        if name == "." || name == ".." {
            return Err(invalid("invalid candidate directory name"));
        }
        let name = CString::new(name.as_bytes()).map_err(|_| invalid("invalid candidate directory name"))?;
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent)?;
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let temporary = format!(
            ".oxa-review-{}",
            random.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let temporary_c = CString::new(temporary.clone()).unwrap();
        let anchored = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let stage_path = anchored.join(&temporary);
        // A random collision is never treated as ownership of an existing path.
        // Create the root with create-only semantics before owning cleanup.
        fs::DirBuilder::new().mode(0o700).create(&stage_path)?;
        let mut cleanup = Staging {
            path: stage_path,
            published: false,
        };
        candidate.write_contents(&cleanup.path)?;
        let receipt = Receipt {
            version: 1,
            approval_sha256: &self.preview.approval_sha256,
            source_manifest_sha256: &self.preview.source_manifest_sha256,
            candidate_manifest_sha256: candidate.manifest_sha256(),
            selections: self
                .preview
                .proposals
                .iter()
                .map(|p| ReceiptSelection {
                    worker: &p.worker,
                    run: &p.run,
                    task_id: &p.task_id,
                    context_id: &p.context_id,
                    review_sha256: format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&p.review).expect("review is serializable"))
                    ),
                })
                .collect(),
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(cleanup.path.join(".oxa-review.json"))?;
        serde_json::to_writer(&mut file, &receipt)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        sync_directories(&cleanup.path)?;
        // SAFETY: owned parent FD, valid C names, Linux no-replace flag. The
        // destination is never followed or overwritten, including symlinks.
        let result = unsafe {
            libc::renameat2(
                parent.as_raw_fd(),
                temporary_c.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        cleanup.published = true;
        parent.sync_all()?;
        Ok(())
    }
}

#[derive(Serialize)]
struct Receipt<'a> {
    version: u32,
    approval_sha256: &'a str,
    source_manifest_sha256: &'a str,
    candidate_manifest_sha256: &'a str,
    selections: Vec<ReceiptSelection<'a>>,
}
#[derive(Serialize)]
struct ReceiptSelection<'a> {
    worker: &'a str,
    run: &'a str,
    task_id: &'a str,
    context_id: &'a str,
    review_sha256: String,
}
struct Staging {
    path: std::path::PathBuf,
    published: bool,
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
fn sync_directories(directory: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_directories(&entry.path())?;
        }
    }
    File::open(directory)?.sync_all()
}

#[derive(Deserialize)]
struct Report {
    run: String,
    workers: Vec<ReportWorker>,
}
#[derive(Deserialize)]
struct ReportWorker {
    worker: String,
    outcome: crate::Outcome,
    task: Option<Event>,
    stop: String,
}

/// Only explicitly selected, completed controller tasks can enter review.
/// The saved report remains untrusted; prepare verifies every content/scope hash.
pub fn proposals_from_report(path: &Path, selected: &[String]) -> io::Result<Vec<Proposal>> {
    if selected.is_empty()
        || selected.len() > MAX_SELECTED
        || selected.iter().any(|w| !valid_worker(w))
        || selected.iter().collect::<BTreeSet<_>>().len() != selected.len()
    {
        return Err(invalid("select 1..16 distinct worker IDs"));
    }
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let before = handle.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.len() > MAX_REPORT as u64 {
        return Err(invalid("report must be a bounded plain single-link file"));
    }
    // Reopen only the validated regular inode; no device/FIFO open side effect.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_REPORT as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > MAX_REPORT
        || (
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
            before.nlink(),
        ) != (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
            after.nlink(),
        )
    {
        return Err(invalid("report changed during capture or exceeded its bound"));
    }
    let report: Report =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid saved A2A run report"))?;
    if !valid_worker(&report.run) || report.workers.len() > 128 {
        return Err(invalid("too many workers in saved report"));
    }
    let mut seen = BTreeSet::new();
    let mut proposals = BTreeMap::new();
    for worker in report.workers {
        if !valid_worker(&worker.worker) || !seen.insert(worker.worker.clone()) {
            return Err(invalid("invalid or duplicate report worker"));
        }
        if !selected.contains(&worker.worker) {
            continue;
        }
        if worker.outcome != crate::Outcome::Completed
            || !matches!(worker.stop.as_str(), "not_needed" | "confirmed")
        {
            return Err(invalid("selected worker did not complete with a known stop"));
        }
        let Some(Event::Task(task)) = worker.task else {
            return Err(invalid("selected worker has no A2A task"));
        };
        if task.status.state != TaskState::Completed {
            return Err(invalid("selected task is not completed"));
        }
        let mut found = None;
        for artifact in task.artifacts {
            for part in artifact.parts {
                if let Part::Data { data } = part {
                    if data.get("kind").and_then(|v| v.as_str()) != Some("oxa.workspace.review.v1") {
                        continue;
                    }
                    if found.is_some() {
                        return Err(invalid("selected worker has multiple review artifacts"));
                    }
                    let review: ReviewBundle = serde_json::from_value(
                        data.get("review")
                            .ok_or_else(|| invalid("review artifact has no content"))?
                            .clone(),
                    )
                    .map_err(|_| invalid("invalid workspace review artifact"))?;
                    found = Some(review);
                }
            }
        }
        let review = found.ok_or_else(|| invalid("selected worker has no review artifact"))?;
        proposals.insert(
            worker.worker.clone(),
            Proposal {
                worker: worker.worker,
                run: report.run.clone(),
                task_id: task.id,
                context_id: task.context_id,
                review,
            },
        );
    }
    selected
        .iter()
        .map(|worker| {
            proposals
                .remove(worker)
                .ok_or_else(|| invalid("selected worker is absent from report"))
        })
        .collect()
}
fn valid_task_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
fn valid_worker(worker: &str) -> bool {
    !worker.is_empty()
        && worker.len() <= 64
        && worker
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn path(s: &str) -> RelativePath {
        RelativePath::new(s.into()).unwrap()
    }
    fn snapshot(env: &TestEnv) -> Arc<Snapshot> {
        fs::write(env.root().join("a.txt"), "original a\n").unwrap();
        fs::write(env.root().join("b.txt"), "original b\n").unwrap();
        Arc::new(Snapshot::approve(env.root(), vec![path("a.txt"), path("b.txt")]).unwrap())
    }
    fn proposal(
        env: &TestEnv,
        snapshot: Arc<Snapshot>,
        worker: &str,
        changes: &[(&str, Option<&str>)],
    ) -> Proposal {
        let grant = Arc::new(Grant::editing(snapshot, vec![path("new.txt")]).unwrap());
        let copy = grant
            .materialize(&env.root().join(format!("copy-{worker}")))
            .unwrap();
        for (name, after) in changes {
            match after {
                Some(text) => fs::write(copy.path().join(name), text).unwrap(),
                None => fs::remove_file(copy.path().join(name)).unwrap(),
            }
        }
        Proposal {
            worker: worker.into(),
            run: "test-run".into(),
            task_id: format!("task-{worker}"),
            context_id: "test-context".into(),
            review: copy.collect("worker").unwrap(),
        }
    }
    fn no_staging(env: &TestEnv) {
        assert!(!fs::read_dir(env.root()).unwrap().any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".oxa-review-")));
    }
    #[test]
    fn disjoint_selected_changes_stage_only_after_exact_approval() {
        let env = TestEnv::new();
        let snapshot = snapshot(&env);
        let first = proposal(
            &env,
            snapshot.clone(),
            "one",
            &[("a.txt", Some("after a\n")), ("new.txt", Some("new\n"))],
        );
        let second = proposal(&env, snapshot.clone(), "two", &[("b.txt", None)]);
        let prepared = PreparedReview::prepare(
            snapshot.clone(),
            vec![path("new.txt")],
            vec![second.clone(), first.clone()],
        )
        .unwrap();
        let ordered = PreparedReview::prepare(snapshot, vec![path("new.txt")], vec![first, second]).unwrap();
        assert_eq!(
            prepared.preview().approval_sha256,
            ordered.preview().approval_sha256
        );
        assert!(prepared.preview().conflicts.is_empty());
        let target = env.root().join("candidate");
        assert!(prepared.stage(&target, &"0".repeat(64)).is_err());
        assert!(!target.exists());
        no_staging(&env);
        prepared
            .stage(&target, &prepared.preview().approval_sha256)
            .unwrap();
        assert_eq!(fs::read_to_string(target.join("a.txt")).unwrap(), "after a\n");
        assert_eq!(fs::read_to_string(target.join("new.txt")).unwrap(), "new\n");
        assert!(!target.join("b.txt").exists());
        assert_eq!(
            fs::read_to_string(env.root().join("a.txt")).unwrap(),
            "original a\n"
        );
        assert_eq!(
            fs::read_to_string(env.root().join("b.txt")).unwrap(),
            "original b\n"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(target.join(".oxa-review.json")).unwrap()).unwrap();
        assert_eq!(receipt["approval_sha256"], prepared.preview().approval_sha256);
        assert_eq!(receipt["selections"][0]["run"], "test-run");
        assert_eq!(receipt["selections"][0]["task_id"], "task-one");
        let actual = Snapshot::approve(&target, vec![path("a.txt"), path("new.txt")]).unwrap();
        assert_eq!(receipt["candidate_manifest_sha256"], actual.manifest_sha256());
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(
            fs::metadata(target.join("a.txt")).unwrap().permissions().mode() & 0o177,
            0
        );
        assert!(prepared
            .stage(&target, &prepared.preview().approval_sha256)
            .is_err());
        no_staging(&env);
        symlink(&target, env.root().join("linked-candidate")).unwrap();
        assert!(prepared
            .stage(
                &env.root().join("linked-candidate"),
                &prepared.preview().approval_sha256
            )
            .is_err());
        no_staging(&env);
    }
    #[test]
    fn overlaps_even_identical_edits_require_a_different_explicit_selection() {
        let env = TestEnv::new();
        let snapshot = snapshot(&env);
        let first = proposal(&env, snapshot.clone(), "one", &[("a.txt", Some("same\n"))]);
        let second = proposal(&env, snapshot.clone(), "two", &[("a.txt", Some("same\n"))]);
        let prepared =
            PreparedReview::prepare(snapshot.clone(), vec![], vec![first.clone(), second]).unwrap();
        assert_eq!(prepared.preview().conflicts.len(), 1);
        assert_eq!(prepared.preview().conflicts[0].workers, vec!["one", "two"]);
        assert!(prepared.preview().candidate_manifest_sha256.is_none());
        assert!(prepared
            .stage(
                &env.root().join("conflicted"),
                &prepared.preview().approval_sha256
            )
            .is_err());
        assert!(!env.root().join("conflicted").exists());
        let single = PreparedReview::prepare(snapshot, vec![], vec![first]).unwrap();
        assert_ne!(
            single.preview().approval_sha256,
            prepared.preview().approval_sha256
        );
        single
            .stage(
                &env.root().join("selected-one"),
                &single.preview().approval_sha256,
            )
            .unwrap();
    }
    #[test]
    fn tampered_content_stale_sources_and_scope_changes_cannot_reuse_approval() {
        let env = TestEnv::new();
        let snapshot = snapshot(&env);
        let original = proposal(&env, snapshot.clone(), "one", &[("a.txt", Some("after\n"))]);
        let prepared = PreparedReview::prepare(snapshot.clone(), vec![], vec![original.clone()]).unwrap();
        let mut forged = original.clone();
        forged.review.changes[0].after = Some("forged\n".into());
        assert!(PreparedReview::prepare(snapshot.clone(), vec![], vec![forged]).is_err());
        let wider =
            PreparedReview::prepare(snapshot.clone(), vec![path("new.txt")], vec![original.clone()]).unwrap();
        assert_ne!(
            prepared.preview().approval_sha256,
            wider.preview().approval_sha256
        );
        assert!(wider
            .stage(&env.root().join("wider"), &prepared.preview().approval_sha256)
            .is_err());
        let mut reattributed = original.clone();
        reattributed.task_id = "other-task".into();
        assert_ne!(
            prepared.preview().approval_sha256,
            PreparedReview::prepare(snapshot, vec![], vec![reattributed])
                .unwrap()
                .preview()
                .approval_sha256
        );
        fs::write(env.root().join("b.txt"), "concurrent context edit\n").unwrap();
        let fresh = Arc::new(Snapshot::approve(env.root(), vec![path("a.txt"), path("b.txt")]).unwrap());
        assert!(PreparedReview::prepare(fresh, vec![], vec![original]).is_err());
        assert!(!env.root().join("wider").exists());
        no_staging(&env);
    }
    fn report_worker(proposal: &Proposal) -> serde_json::Value {
        serde_json::json!({"worker":proposal.worker,"outcome":"completed","stop":"not_needed","task":{"kind":"task","id":proposal.task_id,"contextId":proposal.context_id,"status":{"state":"completed"},"artifacts":[{"artifactId":"review","parts":[{"kind":"data","data":{"kind":"oxa.workspace.review.v1","review":proposal.review}}]}]}})
    }
    #[test]
    fn saved_reports_require_selected_completed_unique_attributed_reviews() {
        let env = TestEnv::new();
        let snapshot = snapshot(&env);
        let proposal = proposal(&env, snapshot.clone(), "one", &[("a.txt", Some("after\n"))]);
        let worker = report_worker(&proposal);
        let path = env.root().join("report.json");
        let write = |workers: Vec<serde_json::Value>| {
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"run":"test-run","workers":workers})).unwrap(),
            )
            .unwrap()
        };
        write(vec![worker.clone()]);
        let selected = vec!["one".into()];
        let extracted = proposals_from_report(&path, &selected).unwrap();
        assert_eq!(extracted[0].task_id, "task-one");
        PreparedReview::prepare(snapshot, vec![], extracted).unwrap();
        assert!(proposals_from_report(&path, &["absent".into()]).is_err());
        write(vec![worker.clone(), worker.clone()]);
        assert!(proposals_from_report(&path, &selected).is_err());
        let mut canceled = worker.clone();
        canceled["outcome"] = serde_json::json!("canceled");
        write(vec![canceled]);
        assert!(proposals_from_report(&path, &selected).is_err());
        let mut incomplete = worker.clone();
        incomplete["task"]["status"]["state"] = serde_json::json!("working");
        write(vec![incomplete]);
        assert!(proposals_from_report(&path, &selected).is_err());
        let mut unknown = worker.clone();
        unknown["stop"] = serde_json::json!("unconfirmed");
        write(vec![unknown]);
        assert!(proposals_from_report(&path, &selected).is_err());
        let mut multiple = worker.clone();
        let artifact = multiple["task"]["artifacts"][0].clone();
        multiple["task"]["artifacts"]
            .as_array_mut()
            .unwrap()
            .push(artifact);
        write(vec![multiple]);
        assert!(proposals_from_report(&path, &selected).is_err());
        write(vec![worker]);
        symlink(&path, env.root().join("report-link")).unwrap();
        assert!(proposals_from_report(&env.root().join("report-link"), &selected).is_err());
        fs::hard_link(&path, env.root().join("report-hard")).unwrap();
        assert!(proposals_from_report(&path, &selected).is_err());
    }
}
