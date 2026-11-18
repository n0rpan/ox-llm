//! Recovery policy for contradictory required check observations.

use super::queue::{github_utc, is_object_id, positive_id, WorkflowIdentity};
use serde_json::Value;

#[derive(Clone, Copy)]
pub enum Trigger {
    PullRequest,
    MergeGroup,
}

/// The head, workflow path and required name come from the caller's current
/// protected Board and configured workflow policy.
#[derive(Clone, Copy)]
pub struct Evidence<'a> {
    pub repository: &'a str,
    pub head: &'a str,
    pub workflow_path: &'a str,
    pub required_name: &'a str,
    pub trigger: Trigger,
    pub check: &'a Value,
    pub attempt: &'a Value,
    pub job: &'a Value,
}

pub struct Verified<'a> {
    pub workflow_run_id: u64,
    pub required_name: &'a str,
    pub observation: Observation<'a>,
}

/// Join the required check with the exact Actions attempt and job. The
/// check-run link is compared as text, never followed.
pub fn verify(evidence: Evidence<'_>) -> Option<Verified<'_>> {
    let Evidence {
        repository,
        head,
        workflow_path,
        required_name,
        trigger,
        check,
        attempt,
        job,
    } = evidence;
    if !is_object_id(head)
        || required_name.is_empty()
        || check["__typename"] != "CheckRun"
        || check["isRequired"] != true
        || check["name"].as_str() != Some(required_name)
        || check["status"] != "IN_PROGRESS"
    {
        return None;
    }
    let completed_at = github_utc(&check["completedAt"])?;
    let conclusion = check["conclusion"].as_str()?;
    let event = match trigger {
        Trigger::PullRequest => "pull_request",
        Trigger::MergeGroup => "merge_group",
    };
    let identity = WorkflowIdentity::for_event(check, head, event)?;
    let check_id = positive_id(&check["databaseId"])?;
    let job_id = positive_id(&job["id"])?;
    let check_url = format!("https://api.github.com/repos/{repository}/check-runs/{check_id}");
    if attempt["id"].as_u64() != Some(identity.workflow_run_id)
        || attempt["run_attempt"].as_u64() != Some(identity.observed_run_attempt)
        || attempt["check_suite_id"].as_u64() != Some(identity.check_suite_id)
        || attempt["head_sha"].as_str() != Some(head)
        || attempt["event"] != event
        || attempt["path"].as_str() != Some(workflow_path)
        || job["run_id"].as_u64() != Some(identity.workflow_run_id)
        || job["run_attempt"].as_u64() != Some(identity.observed_run_attempt)
        || job["head_sha"].as_str() != Some(head)
        || job["name"].as_str() != Some(required_name)
        || job["check_run_url"].as_str() != Some(check_url.as_str())
    {
        return None;
    }
    Some(Verified {
        workflow_run_id: identity.workflow_run_id,
        required_name,
        observation: Observation {
            required: true,
            status: "in_progress",
            conclusion: Some(conclusion),
            completed_at: Some(completed_at),
            job_id,
            run_attempt: identity.observed_run_attempt,
        },
    })
}

/// A single check and its exact, independently verified Actions job attempt.
pub struct Observation<'a> {
    pub required: bool,
    pub status: &'a str,
    pub conclusion: Option<&'a str>,
    pub completed_at: Option<&'a str>,
    pub job_id: u64,
    pub run_attempt: u64,
}

/// The caller scopes durable history to the repository, exact head, workflow
/// run and logical job. A missing or unreadable journal is unknown.
#[derive(Clone, Copy)]
pub enum History {
    Unknown,
    Unused,
    Reserved,
    Requested { before_attempt: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Quiet,
    WaitForReceipt,
    RerunJob(u64),
    Alarm,
}

const STALE_CHECK_SECONDS: u64 = 10 * 60;

pub fn decide(enabled: bool, check: &Observation<'_>, now: u64, history: History) -> Decision {
    if !enabled
        || !check.required
        || check.status != "in_progress"
        || check.job_id == 0
        || check.run_attempt == 0
        || !check.conclusion.is_some_and(|conclusion| {
            [
                "success",
                "failure",
                "neutral",
                "cancelled",
                "skipped",
                "timed_out",
                "action_required",
                "stale",
                "startup_failure",
            ]
            .iter()
            .any(|known| known.eq_ignore_ascii_case(conclusion))
        })
    {
        return Decision::Quiet;
    }
    let age = check
        .completed_at
        .and_then(oxa_session::parse_utc_seconds)
        .and_then(|completed_at| now.checked_sub(completed_at));
    if !age.is_some_and(|age| age > STALE_CHECK_SECONDS) {
        return Decision::Quiet;
    }

    match history {
        History::Unused => Decision::RerunJob(check.job_id),
        History::Requested { before_attempt } if before_attempt > 0 && check.run_attempt > before_attempt => {
            Decision::Alarm
        }
        History::Unknown | History::Reserved | History::Requested { .. } => Decision::WaitForReceipt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_check_and_actions_metadata_must_identify_one_exact_attempt() {
        let head = "a".repeat(40);
        let repo = "example/project";
        let path = ".github/workflows/checks.yml";
        let name = "gate";
        let completed = "1970-01-02T00:00:00Z";
        let check = json!({
            "__typename": "CheckRun", "name": name, "isRequired": true, "status": "IN_PROGRESS",
            "databaseId": 31, "completedAt": completed, "conclusion": "SUCCESS",
            "checkSuite": {"databaseId": 19, "commit": {"oid": head},
                "workflowRun": {"databaseId": 12, "runAttempt": 2, "event": "merge_group"}}
        });
        let attempt = json!({
            "id": 12, "check_suite_id": 19, "head_sha": head, "run_attempt": 2,
            "event": "merge_group", "path": path
        });
        let job = json!({
            "id": 23, "run_id": 12, "run_attempt": 2, "head_sha": head, "name": name,
            "check_run_url": "https://api.github.com/repos/example/project/check-runs/31"
        });
        let evidence = Evidence {
            repository: repo,
            head: &head,
            workflow_path: path,
            required_name: name,
            trigger: Trigger::MergeGroup,
            check: &check,
            attempt: &attempt,
            job: &job,
        };
        let matched = verify(evidence).unwrap();
        assert_eq!(matched.workflow_run_id, 12);
        assert_eq!(matched.required_name, name);
        assert_eq!(
            decide(true, &matched.observation, 87_001, History::Unused),
            Decision::RerunJob(23)
        );
        for (field, value) in [
            (
                "check_run_url",
                json!("https://api.github.com/repos/wrong/repo/check-runs/31"),
            ),
            ("run_attempt", json!(1)),
            ("head_sha", json!("b".repeat(40))),
            ("name", json!("different check")),
        ] {
            let mut changed = job.clone();
            changed[field] = value;
            assert!(
                verify(Evidence {
                    job: &changed,
                    ..evidence
                })
                .is_none(),
                "{field}"
            );
        }
        assert!(verify(Evidence {
            trigger: Trigger::PullRequest,
            ..evidence
        })
        .is_none());
    }

    fn check() -> Observation<'static> {
        Observation {
            required: true,
            status: "in_progress",
            conclusion: Some("success"),
            completed_at: Some("1970-01-02T00:00:00Z"),
            job_id: 23,
            run_attempt: 2,
        }
    }

    #[test]
    fn only_a_required_in_progress_check_with_completion_evidence_is_eligible() {
        let check = check();
        assert_eq!(decide(false, &check, 87_001, History::Unused), Decision::Quiet);
        for other in [
            Observation {
                required: false,
                ..check
            },
            Observation {
                status: "completed",
                ..check
            },
            Observation {
                status: "queued",
                ..check
            },
            Observation {
                conclusion: None,
                ..check
            },
            Observation {
                conclusion: Some(""),
                ..check
            },
            Observation {
                completed_at: None,
                ..check
            },
            Observation {
                completed_at: Some("bad date"),
                ..check
            },
            Observation {
                completed_at: Some("1970-01-03T00:00:00Z"),
                ..check
            },
            Observation { job_id: 0, ..check },
            Observation {
                run_attempt: 0,
                ..check
            },
        ] {
            assert_eq!(decide(true, &other, 87_001, History::Unused), Decision::Quiet);
        }
        assert_eq!(decide(true, &check, 87_000, History::Unused), Decision::Quiet);
        assert_eq!(
            decide(true, &check, 87_001, History::Unused),
            Decision::RerunJob(23)
        );
    }

    #[test]
    fn stale_receipts_and_ambiguous_writes_cannot_authorize_another_rerun() {
        let check = check();
        for history in [
            History::Unknown,
            History::Reserved,
            History::Requested { before_attempt: 2 },
        ] {
            assert_eq!(decide(true, &check, 87_001, history), Decision::WaitForReceipt);
        }
        assert_eq!(
            decide(true, &check, 87_001, History::Requested { before_attempt: 1 }),
            Decision::Alarm
        );
        assert_eq!(
            decide(true, &check, 87_000, History::Requested { before_attempt: 1 }),
            Decision::Quiet
        );
    }
}
