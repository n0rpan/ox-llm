//! Recovery policy for contradictory required check observations.

use super::queue::{github_utc, is_node_id, is_object_id, positive_id, report::unique_row, WorkflowIdentity};
use serde_json::Value;

mod current;
pub use current::Current;

#[derive(Clone, Copy)]
pub enum Trigger {
    PullRequest,
    MergeGroup,
}

/// The head, workflow path and required name come from the caller's current
/// protected Board and configured workflow policy.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub repository: &'a str,
    pub repository_id: &'a str,
    pub head: &'a str,
    pub workflow_path: &'a str,
    pub required_name: &'a str,
    pub trigger: Trigger,
    pub check: &'a Value,
}

#[derive(Clone, Copy)]
pub struct Evidence<'a, 'data> {
    pub source: Source<'a>,
    pub attempt: &'data Value,
    pub job: &'data Value,
}

pub struct Verified<'a> {
    pub attempt: JobAttempt<'a>,
    pub observation: Observation<'a>,
}

/// The unchanged identity across the original Actions job and its replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobLineage<'a> {
    pub repository: &'a str,
    pub repository_id: &'a str,
    pub head: &'a str,
    pub workflow_path: &'a str,
    pub required_name: &'a str,
    pub check_suite_id: u64,
    pub workflow_run_id: u64,
}

/// Exact verified observation. The publisher owns the durable dispatch record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobAttempt<'a> {
    pub lineage: JobLineage<'a>,
    pub check_run_id: u64,
    pub job_id: u64,
    pub run_attempt: u64,
}

impl JobAttempt<'_> {
    /// A later observed replacement is distinct from acceptance of a request.
    pub fn replaces(&self, original: &JobAttempt<'_>) -> bool {
        self.lineage == original.lineage
            && self.run_attempt > original.run_attempt
            && self.job_id != original.job_id
            && self.check_run_id != original.check_run_id
    }
}

/// Join the required check with the exact Actions attempt and job. The
/// check-run link is compared as text, never followed.
pub fn verify<'a>(evidence: Evidence<'a, '_>) -> Option<Verified<'a>> {
    let Evidence { source, attempt, job } = evidence;
    let Source {
        repository,
        repository_id,
        head,
        workflow_path,
        required_name,
        trigger,
        check,
    } = source;
    let status = check["status"].as_str()?;
    let completed_at = github_utc(&check["completedAt"]);
    let conclusion = check["conclusion"].as_str();
    let identity = source.identity()?;
    let event = trigger.event();
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
        attempt: JobAttempt {
            lineage: JobLineage {
                repository,
                repository_id,
                head,
                workflow_path,
                required_name,
                check_suite_id: identity.check_suite_id,
                workflow_run_id: identity.workflow_run_id,
            },
            check_run_id: check_id,
            job_id,
            run_attempt: identity.observed_run_attempt,
        },
        observation: Observation {
            required: true,
            status,
            conclusion,
            completed_at,
            job_id,
            run_attempt: identity.observed_run_attempt,
        },
    })
}

impl Trigger {
    fn event(self) -> &'static str {
        match self {
            Self::PullRequest => "pull_request",
            Self::MergeGroup => "merge_group",
        }
    }
}

impl Source<'_> {
    fn identity(&self) -> Option<WorkflowIdentity> {
        if !is_node_id(self.repository_id)
            || !is_object_id(self.head)
            || self.required_name.is_empty()
            || self.check["__typename"] != "CheckRun"
            || self.check["isRequired"] != true
            || self.check["name"].as_str() != Some(self.required_name)
        {
            return None;
        }
        WorkflowIdentity::for_event(self.check, self.head, self.trigger.event())
    }
}

/// Obtain one complete attempt/job collection via the existing budgeted Actions
/// Reader. The caller supplies a selected exact-current check from its Board
/// snapshot and uses the same lock and current-head revalidation before dispatch.
pub fn read<'a>(
    source: Source<'a>,
    mut actions_json: impl FnMut(&str) -> Result<Value, String>,
) -> Result<Option<Verified<'a>>, String> {
    let Some(identity) = source.identity() else {
        return Ok(None);
    };
    let Some(check_id) = positive_id(&source.check["databaseId"]) else {
        return Ok(None);
    };
    let address = format!(
        "runs/{}/attempts/{}",
        identity.workflow_run_id, identity.observed_run_attempt
    );
    let attempt = actions_json(&address)?;
    let jobs = actions_json(&format!("{address}/jobs?per_page=100"))?;
    let link = format!(
        "https://api.github.com/repos/{}/check-runs/{check_id}",
        source.repository
    );
    let Some(job) = unique_row(&jobs, "jobs", |job| {
        job["check_run_url"].as_str() == Some(link.as_str())
    }) else {
        return Ok(None);
    };
    Ok(verify(Evidence {
        source,
        attempt: &attempt,
        job,
    }))
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
        || !check.status.eq_ignore_ascii_case("in_progress")
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
        let source = Source {
            repository: repo,
            repository_id: "R_repo",
            head: &head,
            workflow_path: path,
            required_name: name,
            trigger: Trigger::MergeGroup,
            check: &check,
        };
        let evidence = Evidence {
            source,
            attempt: &attempt,
            job: &job,
        };
        let matched = verify(evidence).unwrap();
        assert_eq!(matched.attempt.lineage.workflow_run_id, 12);
        assert_eq!(matched.attempt.lineage.required_name, name);
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
            source: Source {
                trigger: Trigger::PullRequest,
                ..source
            },
            ..evidence
        })
        .is_none());

        let mut addresses = Vec::new();
        let readback = read(source, |address| {
            addresses.push(address.to_owned());
            match address {
                "runs/12/attempts/2" => Ok(attempt.clone()),
                "runs/12/attempts/2/jobs?per_page=100" => Ok(json!({"total_count":1,"jobs":[job]})),
                _ => panic!("unexpected Actions address"),
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(readback.attempt, matched.attempt);
        assert_eq!(
            addresses,
            ["runs/12/attempts/2", "runs/12/attempts/2/jobs?per_page=100"]
        );
        for listing in [
            json!({"total_count":2,"jobs":[job]}),
            json!({"total_count":2,"jobs":[job,job]}),
        ] {
            assert!(read(source, |address| Ok(if address.ends_with("/2") {
                attempt.clone()
            } else {
                listing.clone()
            }))
            .unwrap()
            .is_none());
        }

        assert!(!matched.attempt.replaces(&matched.attempt));
        assert_eq!(
            decide(true, &matched.observation, 87_001, History::Reserved),
            Decision::WaitForReceipt
        );
        let mut replacement = check.clone();
        replacement["databaseId"] = 32.into();
        replacement["status"] = "COMPLETED".into();
        replacement["checkSuite"]["workflowRun"]["runAttempt"] = 3.into();
        let mut later_attempt = attempt.clone();
        later_attempt["run_attempt"] = 3.into();
        let mut later_job = job.clone();
        later_job["id"] = 24.into();
        later_job["run_attempt"] = 3.into();
        later_job["check_run_url"] = "https://api.github.com/repos/example/project/check-runs/32".into();
        let later = verify(Evidence {
            source: Source {
                check: &replacement,
                ..source
            },
            attempt: &later_attempt,
            job: &later_job,
            ..evidence
        })
        .unwrap();
        assert!(later.attempt.replaces(&matched.attempt));
        assert!(!JobAttempt {
            lineage: JobLineage {
                repository_id: "R_other",
                ..later.attempt.lineage
            },
            ..later.attempt
        }
        .replaces(&matched.attempt));
        let reconciled = History::Requested {
            before_attempt: matched.attempt.run_attempt,
        };
        assert_eq!(
            decide(true, &later.observation, 87_001, reconciled),
            Decision::Quiet
        );

        let mut stale_again = replacement.clone();
        stale_again["status"] = "IN_PROGRESS".into();
        let later = verify(Evidence {
            source: Source {
                check: &stale_again,
                ..source
            },
            attempt: &later_attempt,
            job: &later_job,
            ..evidence
        })
        .unwrap();
        assert_eq!(
            decide(true, &later.observation, 87_001, reconciled),
            Decision::Alarm
        );
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
