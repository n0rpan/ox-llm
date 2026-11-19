//! GitHub lane acceptance: push the saved head and wait for the lab's lane workflow.
//!
//! The lane workflow (`.github/workflows/lane.yml`) is the repository's own CI for a
//! branch diff. Acceptance here is its terminal conclusion for the exact accepted
//! commit, never a local substitute and never another head.
use crate::{command_runner_with_stdin, state, Result, RunPolicy, COMMAND_DEADLINE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const LANE_WORKFLOW: &str = "lane.yml";
pub const LANE_POLL: Duration = Duration::from_secs(15);
pub const LANE_WAIT: Duration = Duration::from_secs(75 * 60);
const MAX_LOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_FAILED_NAMES: usize = 64;
const MAX_NAME_CHARS: usize = 512;
const MAX_DETAIL_CHARS: usize = 8 * 1024;

/// Branch prefixes the lane workflow is triggered by. Any other branch would
/// wait forever for a run that cannot exist, so it is refused.
const LANE_BRANCHES: &[&str] = &["agent/", "codex/", "air/", "runner/"];

/// A terminal lane observation bound to one exact commit.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaneVerdict {
    pub head: String,
    pub run_id: u64,
    pub conclusion: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
}

impl LaneVerdict {
    pub fn passed(&self) -> bool {
        self.conclusion == "success"
    }
}

pub enum LaneAcceptance {
    Pass(LaneVerdict),
    /// A test or build failure: this spends the task's one repair round.
    Fail {
        verdict: LaneVerdict,
        failures: String,
    },
    /// An infrastructure result. It is never green and must not spend a repair.
    Blocked {
        verdict: LaneVerdict,
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Pass,
    Repair,
    Blocked,
}

/// Only a completed `failure` is the code's failure. Cancellation, timeouts and
/// startup errors are infrastructure and stay blocked with their own state.
fn disposition(conclusion: &str) -> Disposition {
    match conclusion {
        "success" => Disposition::Pass,
        "failure" => Disposition::Repair,
        _ => Disposition::Blocked,
    }
}

pub(crate) fn branch_is_laned(branch: &str) -> bool {
    LANE_BRANCHES.iter().any(|prefix| {
        branch
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.trim().is_empty())
    })
}

struct Run {
    id: u64,
    status: String,
    conclusion: Option<String>,
}

impl Run {
    fn complete(&self) -> bool {
        self.status == "completed"
    }
}

fn valid_status(status: &str) -> bool {
    matches!(
        status,
        "queued" | "in_progress" | "completed" | "waiting" | "requested" | "pending"
    )
}

fn valid_conclusion(conclusion: &str) -> bool {
    matches!(
        conclusion,
        "success"
            | "failure"
            | "cancelled"
            | "timed_out"
            | "action_required"
            | "neutral"
            | "skipped"
            | "stale"
            | "startup_failure"
    )
}

/// Select the newest push-triggered run for exactly this commit.
fn parse_runs(value: &Value, head: &str) -> Result<Option<Run>> {
    let rows = value.as_array().ok_or("lane run list is not an array")?;
    let mut selected: Option<Run> = None;
    for row in rows {
        let id = row["databaseId"]
            .as_u64()
            .filter(|id| *id != 0)
            .ok_or("lane run identity missing")?;
        let run_head = row["headSha"].as_str().ok_or("lane run head missing")?;
        let event = row["event"].as_str().ok_or("lane run event missing")?;
        let status = row["status"].as_str().ok_or("lane run status missing")?;
        if !valid_status(status) {
            return Err("invalid lane run status".into());
        }
        // `gh run list` reports an unfinished run as an empty string, not null.
        let conclusion = match &row["conclusion"] {
            Value::Null => None,
            Value::String(text) if text.is_empty() => None,
            Value::String(text) if valid_conclusion(text) => Some(text.clone()),
            _ => return Err("invalid lane run conclusion".into()),
        };
        if (status == "completed") != conclusion.is_some() {
            return Err("lane run status disagrees with its conclusion".into());
        }
        if run_head != head || event != "push" {
            continue;
        }
        let run = Run {
            id,
            status: status.into(),
            conclusion,
        };
        if selected.as_ref().is_none_or(|chosen| chosen.id < run.id) {
            selected = Some(run);
        }
    }
    Ok(selected)
}

fn completed_run(value: &Value, head: &str) -> Result<Option<Run>> {
    match parse_runs(value, head)? {
        Some(run) if run.complete() => Ok(Some(run)),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

fn sanitized_name(name: &str) -> Option<String> {
    let name: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn push_failed(names: &mut Vec<String>, name: &str) {
    if names.len() >= MAX_FAILED_NAMES {
        return;
    }
    if let Some(name) = sanitized_name(name) {
        if !names.iter().any(|existing| existing == &name) {
            names.push(name);
        }
    }
}

/// Failed job and step identities from `gh run view --json jobs`.
fn failed_steps(value: &Value, names: &mut Vec<String>) {
    let Some(jobs) = value["jobs"].as_array() else {
        return;
    };
    for job in jobs {
        let failed = job["conclusion"].as_str() == Some("failure");
        let job_name = job["name"].as_str().unwrap_or_default();
        if failed {
            push_failed(names, job_name);
        }
        let Some(steps) = job["steps"].as_array() else {
            continue;
        };
        for step in steps
            .iter()
            .filter(|step| step["conclusion"].as_str() == Some("failure"))
        {
            let step_name = step["name"].as_str().unwrap_or_default();
            push_failed(names, &format!("{job_name}: {step_name}"));
        }
    }
}

/// The lane result is only a gate while the gate's own definition stays fixed.
/// A head that edits the lane workflow or its runner could report itself green.
const PROTECTED_LANE_CONTROLS: &[&str] = &[".github/workflows/", "tools/ci/"];

fn protected_controls_unchanged(policy: &RunPolicy, repo: &Path, base: &str, head: &str) -> Result<()> {
    let changed = crate::checked(crate::git(
        policy,
        repo,
        &[
            "diff",
            "--name-only",
            base,
            head,
            "--",
            PROTECTED_LANE_CONTROLS[0],
            PROTECTED_LANE_CONTROLS[1],
        ],
    ))?;
    // The saved base can be ahead of what the destination has, so the controls of every
    // commit the push would add are checked against the destination's own tips too.
    let unpushed = crate::publish::pushed_path_changes(policy, repo, head, PROTECTED_LANE_CONTROLS)?;
    if changed.trim().is_empty() && unpushed.trim().is_empty() {
        Ok(())
    } else {
        Err(
            "lane acceptance refuses a head that changes GitHub workflows or the lane runner; land those separately"
                .into(),
        )
    }
}

/// Remove the `gh run view --log-failed` envelope: `<job>\t<step>\t<timestamp>`.
fn log_message(line: &str) -> &str {
    let mut rest = line;
    if let Some((_, after_job)) = line.split_once('\t') {
        if let Some((_, after_step)) = after_job.split_once('\t') {
            rest = after_step;
        }
    }
    let rest = rest.trim_start();
    if let Some((stamp, message)) = rest.split_once(' ') {
        if stamp.len() >= 20 && stamp.contains('T') && stamp.ends_with('Z') {
            return message.trim_start();
        }
    }
    rest
}

/// Test identities from a bounded failure log. Nextest prints one `FAIL` line per
/// test; the classic harness prints `test NAME ... FAILED`. Anything else is left
/// to the job and step names rather than guessed.
fn failed_tests(log: &str, names: &mut Vec<String>) {
    for line in log.lines() {
        let line = log_message(line);
        if let Some(rest) = line.strip_prefix("FAIL ") {
            if let Some(name) = rest.split_whitespace().last() {
                push_failed(names, name);
            }
        } else if let Some(rest) = line.strip_prefix("test ") {
            if let Some(name) = rest.strip_suffix(" ... FAILED") {
                push_failed(names, name);
            }
        }
    }
}

fn failure_detail(run: &Run, names: &[String]) -> String {
    let mut detail = format!(
        "lane CI run {} concluded {} at the accepted commit. Repair once.",
        run.id,
        run.conclusion.as_deref().unwrap_or("unknown")
    );
    if !names.is_empty() {
        detail.push_str("\nFailed names:");
        for name in names {
            detail.push_str("\n- ");
            let room = MAX_DETAIL_CHARS.saturating_sub(detail.len());
            if room == 0 {
                break;
            }
            detail.extend(name.chars().take(room));
        }
    }
    detail
}

fn log_failed(
    gh: &Path,
    policy: &RunPolicy,
    repo: &Path,
    lock: &std::fs::File,
    repository: &str,
    run_id: u64,
) -> Option<String> {
    let mut command = Command::new(gh);
    crate::isolate_git_environment(&mut command);
    crate::publish::isolate_github_environment(&mut command);
    command
        .env("PATH", &policy.host_path)
        .env("GH_PROMPT_DISABLED", "1")
        .current_dir(repo)
        .args([
            "run",
            "view",
            &run_id.to_string(),
            "--repo",
            repository,
            "--log-failed",
        ]);
    let (code, text) = command_runner_with_stdin(
        command,
        Some(MAX_LOG_BYTES),
        Stdio::from(lock.try_clone().ok()?),
        COMMAND_DEADLINE,
    )?;
    (code == 0).then_some(text)
}

fn wait_with(
    mut list: impl FnMut() -> Result<Option<Run>>,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> Result<Run> {
    let mut polls = 0u32;
    loop {
        if let Some(run) = list()? {
            if run.complete() {
                return Ok(run);
            }
        }
        if elapsed() >= LANE_WAIT {
            return Err(format!(
                "lane CI did not conclude within {} minutes for the accepted commit",
                LANE_WAIT.as_secs() / 60
            ));
        }
        // A frozen clock must still terminate a stuck wait.
        polls += 1;
        if polls > 10_000 {
            return Err("lane CI wait exceeded its bounded poll count".into());
        }
        sleep(LANE_POLL);
    }
}

/// Push the saved head and bind lane acceptance to its exact workflow run.
pub(crate) fn accept(
    repo: &Path,
    binary: &Path,
    policy: &RunPolicy,
    lock: &std::fs::File,
    saved: &state::State,
) -> Result<LaneAcceptance> {
    let task = &saved.task;
    if !task.lane_ci {
        return Err("this task does not opt into lane acceptance".into());
    }
    if !branch_is_laned(&task.branch) {
        return Err(format!(
            "lane acceptance needs a branch the lane workflow runs on ({})",
            LANE_BRANCHES.join(", ")
        ));
    }
    if policy.offline {
        return Err("lane acceptance is unavailable in offline mode".into());
    }
    if !policy.push_approved {
        return Err(
            "lane acceptance pushes the accepted commit; the owner must confirm that push first".into(),
        );
    }
    let gh = policy
        .gh_binary
        .as_deref()
        .ok_or("lane acceptance needs a trusted gh executable")?;
    let head = saved
        .record
        .head_sha
        .as_deref()
        .ok_or("lane acceptance needs the saved head")?;
    let base = saved
        .record
        .base_sha
        .as_deref()
        .ok_or("lane acceptance needs the saved base")?;
    // Reuse the existing publication proof: a clean, object-verified export whose
    // captured checks run for this commit. Nothing is pushed before it passes.
    let proof = crate::committed::verify_commit_with(repo, binary, policy, saved, lock, true)?;
    proof.verify(policy, head, base)?;
    protected_controls_unchanged(policy, repo, base, head)?;
    let repository = crate::publish::origin_repository(repo, gh, policy, lock)?;
    // The same content scan `--publish` runs, before anything is sent.
    crate::publish::scan_push(policy, repo, head)?;
    // Queue membership is the final remote decision before the push, avoiding
    // a scan-sized race in which an open PR can enter the merge queue.
    if let Some(number) =
        crate::publish::queued_pull_request(repo, gh, policy, lock, &repository, &task.branch)?
    {
        return Err(format!(
            "PR #{number} is in the merge queue; use a new task branch for follow-up lane work"
        ));
    }
    crate::publish::push_head(policy, repo, head, &task.branch, lock)?;
    let started = Instant::now();
    let run = wait_with(
        || {
            let text = crate::publish::gh(
                gh,
                policy,
                repo,
                lock,
                &[
                    "run",
                    "list",
                    "--repo",
                    &repository.qualified,
                    "--workflow",
                    LANE_WORKFLOW,
                    "--commit",
                    head,
                    "--limit",
                    "20",
                    "--json",
                    "databaseId,headSha,status,conclusion,event",
                ],
            )?;
            let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            completed_run(&value, head)
        },
        || started.elapsed(),
        |wait| std::thread::sleep(wait),
    )?;
    let conclusion = run.conclusion.clone().unwrap_or_else(|| "unknown".into());
    let disposition = disposition(&conclusion);
    let mut failed = Vec::new();
    if disposition == Disposition::Repair {
        if let Ok(jobs) = crate::publish::gh(
            gh,
            policy,
            repo,
            lock,
            &[
                "run",
                "view",
                &run.id.to_string(),
                "--repo",
                &repository.qualified,
                "--json",
                "jobs",
            ],
        )
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()))
        {
            failed_steps(&jobs, &mut failed);
        }
        if let Some(log) = log_failed(gh, policy, repo, lock, &repository.qualified, run.id) {
            failed_tests(&log, &mut failed);
        }
    }
    let verdict = LaneVerdict {
        head: head.into(),
        run_id: run.id,
        conclusion,
        failed: failed.clone(),
    };
    match disposition {
        Disposition::Pass => Ok(LaneAcceptance::Pass(verdict)),
        Disposition::Repair => {
            let failures = failure_detail(&run, &failed);
            Ok(LaneAcceptance::Fail { verdict, failures })
        }
        Disposition::Blocked => Ok(LaneAcceptance::Blocked {
            detail: format!(
                "lane CI run {} concluded {}; an infrastructure result is not a repair",
                run.id, verdict.conclusion
            ),
            verdict,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(id: u64, head: &str, status: &str, conclusion: Value) -> Value {
        json!({
            "databaseId": id,
            "headSha": head,
            "status": status,
            "conclusion": conclusion,
            "event": "push"
        })
    }

    #[test]
    fn only_lane_triggering_branches_are_accepted() {
        for branch in ["agent/1-work", "codex/task", "air/lane", "runner/one"] {
            assert!(branch_is_laned(branch), "{branch}");
        }
        for branch in ["main", "feature/x", "agent/", "codex", "Agent/x", "oxa/issue-1"] {
            assert!(!branch_is_laned(branch), "{branch}");
        }
    }

    #[test]
    fn a_run_must_match_the_exact_head_and_a_push_event() {
        let head = "a".repeat(40);
        let other = "b".repeat(40);
        let value = json!([
            run(9, &other, "completed", json!("success")),
            run(10, &head, "completed", json!("failure")),
            json!({"databaseId":11,"headSha":head,"status":"completed",
                   "conclusion":"success","event":"pull_request"}),
        ]);
        let chosen = parse_runs(&value, &head).unwrap().unwrap();
        assert_eq!(chosen.id, 10);
        assert_eq!(chosen.conclusion.as_deref(), Some("failure"));
        assert!(parse_runs(&value, &"c".repeat(40)).unwrap().is_none());
    }

    #[test]
    fn the_newest_run_at_the_head_wins_and_inconsistent_shapes_are_refused() {
        let head = "a".repeat(40);
        let value = json!([
            run(10, &head, "completed", json!("failure")),
            run(12, &head, "completed", json!("success")),
            run(11, &head, "completed", json!("failure")),
        ]);
        assert_eq!(parse_runs(&value, &head).unwrap().unwrap().id, 12);
        assert!(parse_runs(&json!([run(1, &head, "completed", Value::Null)]), &head).is_err());
        assert!(parse_runs(&json!([run(1, &head, "in_progress", json!("success"))]), &head).is_err());
        assert!(parse_runs(&json!([run(1, &head, "unknown", Value::Null)]), &head).is_err());
        // The CLI reports an unfinished run as an empty conclusion string.
        assert!(
            completed_run(&json!([run(1, &head, "in_progress", json!(""))]), &head)
                .unwrap()
                .is_none()
        );
        assert!(parse_runs(&json!([run(0, &head, "queued", Value::Null)]), &head).is_err());
    }

    #[test]
    fn failed_test_names_are_extracted_deduplicated_and_sanitized() {
        let log = "\
 error: test run failed
FAIL [   0.004s] oxa-work::lane::tests::a_case
FAIL [   0.005s] oxa-work::lane::tests::a_case
test tools::other ... FAILED
test \u{1b}[31mcontrol\u{1b}[0m ... FAILED
lane (oxa, ubuntu-24.04)\tAffected crates\t2026-11-18T17:45:18.3848701Z         FAIL [   0.044s] ( 699/1728) oxa-cli::work_lane_ci prefixed_case
";
        let mut names = Vec::new();
        failed_tests(log, &mut names);
        assert_eq!(
            names,
            vec![
                "oxa-work::lane::tests::a_case",
                "tools::other",
                "[31mcontrol[0m",
                "prefixed_case"
            ]
        );
    }

    #[test]
    fn failed_names_are_bounded() {
        let mut names = Vec::new();
        for index in 0..(MAX_FAILED_NAMES + 10) {
            push_failed(&mut names, &format!("case_{index}"));
        }
        assert_eq!(names.len(), MAX_FAILED_NAMES);
        let huge = "x".repeat(MAX_NAME_CHARS + 50);
        let mut one = Vec::new();
        push_failed(&mut one, &huge);
        assert_eq!(one[0].chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn waiting_accepts_only_a_terminal_run_and_times_out() {
        use std::cell::Cell;
        let mut pending_reads = 0;
        let clock = Cell::new(Duration::ZERO);
        let run = wait_with(
            || {
                pending_reads += 1;
                if pending_reads < 3 {
                    Ok(Some(Run {
                        id: 1,
                        status: "in_progress".into(),
                        conclusion: None,
                    }))
                } else {
                    Ok(Some(Run {
                        id: 7,
                        status: "completed".into(),
                        conclusion: Some("success".into()),
                    }))
                }
            },
            || clock.get(),
            |step| clock.set(clock.get() + step),
        )
        .unwrap();
        assert_eq!(run.id, 7);
        assert_eq!(run.conclusion.as_deref(), Some("success"));
        let frozen = Cell::new(Duration::ZERO);
        let error = wait_with(
            || Ok(None),
            || frozen.get(),
            |step| frozen.set(frozen.get() + step),
        )
        .err()
        .unwrap();
        assert!(error.contains("did not conclude"), "{error}");
    }

    #[test]
    fn only_a_completed_failure_spends_a_repair() {
        assert_eq!(disposition("success"), Disposition::Pass);
        assert_eq!(disposition("failure"), Disposition::Repair);
        for conclusion in [
            "cancelled",
            "timed_out",
            "startup_failure",
            "action_required",
            "stale",
            "neutral",
            "skipped",
        ] {
            assert_eq!(disposition(conclusion), Disposition::Blocked, "{conclusion}");
        }
    }

    #[test]
    fn only_success_is_a_pass() {
        let head = "a".repeat(40);
        for conclusion in ["failure", "cancelled", "timed_out", "skipped", "neutral"] {
            let verdict = LaneVerdict {
                head: head.clone(),
                run_id: 3,
                conclusion: conclusion.into(),
                failed: vec![],
            };
            assert!(!verdict.passed(), "{conclusion}");
        }
        let verdict = LaneVerdict {
            head,
            run_id: 3,
            conclusion: "success".into(),
            failed: vec![],
        };
        assert!(verdict.passed());
    }
}
