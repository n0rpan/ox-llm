use super::{github_capture, LandingPublisher, Repository};
use crate::board::queue::{is_node_id, is_object_id};
use crate::{github_http_response, Result};
use serde::{Deserialize, Serialize};

/// One allowance per logical required job on this exact repository, head and run.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RerunKey {
    pub repository_id: String,
    pub head: String,
    pub workflow_run_id: u64,
    pub logical_job: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RerunIntent {
    pub key: RerunKey,
    pub job_id: u64,
    pub run_attempt: u64,
}

impl RerunIntent {
    pub(crate) fn valid(&self) -> bool {
        is_node_id(&self.key.repository_id)
            && is_object_id(&self.key.head)
            && self.key.workflow_run_id > 0
            && !self.key.logical_job.trim().is_empty()
            && self.key.logical_job.len() <= 512
            && !self.key.logical_job.chars().any(char::is_control)
            && self.job_id > 0
            && self.run_attempt > 0
    }
}

/// HTTP 201 proves that GitHub accepted this request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RerunAcknowledgment {
    pub intent: RerunIntent,
}

impl RerunAcknowledgment {
    /// The caller supplies a newly verified observation from the existing Reader.
    pub fn reconciles(&self, observed: &RerunIntent) -> bool {
        observed.valid()
            && observed.key == self.intent.key
            && observed.run_attempt > self.intent.run_attempt
            && observed.job_id != self.intent.job_id
    }
}

pub(crate) struct RerunTarget(Repository);

impl LandingPublisher<'_> {
    pub(crate) fn rerun_target(&self, intent: &RerunIntent, publisher_id: &str) -> Result<RerunTarget> {
        if !intent.valid() || !is_node_id(publisher_id) {
            return Err("invalid GitHub rerun binding".into());
        }
        Ok(RerunTarget(
            self.location(&intent.key.repository_id, publisher_id)?,
        ))
    }

    pub(crate) fn rerun_reserved(
        &self,
        target: RerunTarget,
        intent: RerunIntent,
    ) -> Result<RerunAcknowledgment> {
        let Repository {
            owner, name, host, ..
        } = target.0;
        let path = format!("repos/{owner}/{name}/actions/jobs/{}/rerun", intent.job_id);
        let response = github_capture(
            self.gh_binary,
            self.policy,
            self.repo,
            self.journal,
            &["api", "--hostname", &host, "--include", "--method", "POST", &path],
        )?;
        acknowledgment(intent, &response)
    }
}

fn acknowledgment(intent: RerunIntent, response: &str) -> Result<RerunAcknowledgment> {
    let (status, _) = github_http_response(response)?;
    if status != 201 {
        return Err("GitHub job rerun lacks a 201 acknowledgment".into());
    }
    Ok(RerunAcknowledgment { intent })
}

#[cfg(test)]
pub(crate) fn test_intent() -> RerunIntent {
    RerunIntent {
        key: RerunKey {
            repository_id: "R_repo".into(),
            head: "a".repeat(40),
            workflow_run_id: 4,
            logical_job: "required build".into(),
        },
        job_id: 9,
        run_attempt: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledgment_requires_a_201_envelope_and_separate_attempt_evidence() {
        let intent = test_intent();
        for response in ["", "HTTP/2 201", "HTTP/2 200\n\n201", "HTTP/2 202\n\n"] {
            assert!(acknowledgment(intent.clone(), response).is_err());
        }
        let ack = acknowledgment(intent.clone(), "HTTP/2 201\r\ncontent-length: 0\r\n\r\n").unwrap();
        assert!(!ack.reconciles(&intent));
        let mut later = RerunIntent {
            run_attempt: 3,
            job_id: 10,
            ..intent
        };
        assert!(ack.reconciles(&later));
        later.key.head = "b".repeat(40);
        assert!(!ack.reconciles(&later));
    }
}
