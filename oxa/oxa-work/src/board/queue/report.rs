//! Resolve a CI report from the rejected group's authenticated GitHub metadata.
use super::{github_utc, positive_id, RejectionEvidence, WorkflowIdentity};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// Expected workflow and producer configuration supplied by the board owner.
pub struct ReportSource<'a> {
    pub workflow_path: &'a str,
    pub required_check_names: &'a BTreeSet<String>,
    pub producer_name: &'a str,
    pub artifact_name: &'a str,
    pub max_archive_bytes: u64,
}

/// Authenticated JSON from the existing GitHub Reader's Actions endpoints.
#[derive(Clone, Copy)]
pub struct ActionMetadata<'a> {
    pub attempt: &'a Value,
    pub jobs: &'a Value,
    pub artifact_listing: &'a Value,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReportArchive<'a> {
    pub artifact_id: u64,
    pub size_bytes: u64,
    pub sha256: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VerifiedArchive {
    pub id: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VerifiedReports {
    pub failed_check_runs: Vec<u64>,
    pub workflow_run: u64,
    pub run_attempt: u64,
    pub archives: Vec<VerifiedArchive>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_tests: Vec<String>,
}

/// The removal and exact job identities an earlier archive verification used.
#[derive(PartialEq, Eq)]
pub struct ReportAttemptIdentity {
    number: u64,
    group: String,
    removed_at: String,
    checks: Vec<(u64, WorkflowIdentity)>,
}

impl RejectionEvidence {
    pub fn removal_event(&self) -> &str {
        &self.removal_event
    }

    pub fn workflow_checks(&self) -> impl Iterator<Item = u64> + '_ {
        self.failed_required_workflows.keys().copied()
    }

    pub fn report_attempt_identity(&self, checks: &[u64]) -> Option<ReportAttemptIdentity> {
        let checks = checks
            .iter()
            .map(|id| {
                self.failed_required_workflows
                    .get(id)
                    .copied()
                    .map(|workflow| (*id, workflow))
            })
            .collect::<Option<_>>()?;
        Some(ReportAttemptIdentity {
            number: self.number,
            group: self.merge_group_head.clone(),
            removed_at: self.removed_at.clone(),
            checks,
        })
    }

    pub fn record_verified_reports(&mut self, report: VerifiedReports) {
        if !report.failed_check_runs.is_empty()
            && report.failed_check_runs.windows(2).all(|ids| ids[0] < ids[1])
            && report
                .failed_check_runs
                .iter()
                .all(|id| attempt_for(self, *id) == Some((report.workflow_run, report.run_attempt)))
        {
            self.verified_test_reports.push(report);
        }
    }
    /// Project test identities only when each failing required check is explained.
    pub(crate) fn reported_failure_names(&self) -> Vec<&str> {
        let mut checks = BTreeSet::new();
        let mut names = BTreeSet::new();
        for report in &self.verified_test_reports {
            if report.archives.is_empty()
                || report.failed_tests.is_empty()
                || report.failed_check_runs.iter().any(|id| !checks.insert(*id))
            {
                return Vec::new();
            }
            names.extend(report.failed_tests.iter().map(String::as_str));
        }
        if checks.iter().eq(&self.failed_required_check_runs) {
            names.into_iter().collect()
        } else {
            Vec::new()
        }
    }
}

/// Read addresses derived from the rejected group's verified check suite.
pub fn attempt_for(rejection: &RejectionEvidence, failed_check_id: u64) -> Option<(u64, u64)> {
    let expected = rejection.failed_required_workflows.get(&failed_check_id)?;
    Some((expected.workflow_run_id, expected.observed_run_attempt))
}

pub(in crate::board) fn unique_row<'a>(
    listing: &'a Value,
    key: &str,
    matches: impl Fn(&Value) -> bool,
) -> Option<&'a Value> {
    let rows = listing[key].as_array()?;
    if rows.len() > 100 || listing["total_count"].as_u64() != Some(rows.len() as u64) {
        return None;
    }
    let mut matching = rows.iter().filter(|row| matches(row));
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

/// Verify the exact failed job and the report producer before selecting bytes.
/// The caller must still verify the downloaded archive against this digest.
pub fn select<'a>(
    rejection: &RejectionEvidence,
    failed_check_id: u64,
    source: &ReportSource<'_>,
    metadata: ActionMetadata<'a>,
) -> Option<ReportArchive<'a>> {
    let expected = rejection.failed_required_workflows.get(&failed_check_id)?;
    let head = rejection.merge_group_head.as_str();
    let run_id = expected.workflow_run_id;
    let attempt_number = expected.observed_run_attempt;
    let job_matches = |job: &Value| {
        job["run_id"].as_u64() == Some(run_id)
            && job["run_attempt"].as_u64() == Some(attempt_number)
            && job["head_sha"].as_str() == Some(head)
            && job["status"] == "completed"
    };
    let failed = unique_row(metadata.jobs, "jobs", |job| {
        job["id"].as_u64() == Some(failed_check_id)
    })?;
    let failed_at = github_utc(&failed["completed_at"])?;
    if !source.required_check_names.contains(failed["name"].as_str()?)
        || !job_matches(failed)
        || failed["conclusion"] != "failure"
        || failed_at > rejection.removed_at.as_str()
    {
        return None;
    }
    let attempt = metadata.attempt;
    if attempt["id"].as_u64() != Some(run_id)
        || attempt["run_attempt"].as_u64() != Some(attempt_number)
        || attempt["check_suite_id"].as_u64() != Some(expected.check_suite_id)
        || attempt["head_sha"].as_str() != Some(head)
        || attempt["event"] != "merge_group"
        || attempt["status"] != "completed"
        || attempt["path"].as_str() != Some(source.workflow_path)
    {
        return None;
    }
    let producer = unique_row(metadata.jobs, "jobs", |job| {
        job["name"].as_str() == Some(source.producer_name)
    })?;
    if !job_matches(producer) || positive_id(&producer["id"]).is_none() {
        return None;
    }
    let start = github_utc(&producer["started_at"])?;
    let finish = github_utc(&producer["completed_at"])?;
    if start < github_utc(&attempt["run_started_at"])? || finish > failed_at {
        return None;
    }

    let artifact = unique_row(metadata.artifact_listing, "artifacts", |artifact| {
        artifact["name"].as_str() == Some(source.artifact_name)
    })?;
    let created = github_utc(&artifact["created_at"])?;
    let updated = github_utc(&artifact["updated_at"])?;
    if artifact["expired"] != false
        || artifact["workflow_run"]["id"].as_u64() != Some(run_id)
        || artifact["workflow_run"]["head_sha"].as_str() != Some(head)
        || !(start <= created && created <= updated && updated <= finish)
    {
        return None;
    }
    let sha256 = artifact["digest"].as_str()?.strip_prefix("sha256:")?;
    if sha256.len() != 64 || !super::is_object_id(sha256) {
        return None;
    }
    Some(ReportArchive {
        artifact_id: positive_id(&artifact["id"])?,
        size_bytes: positive_id(&artifact["size_in_bytes"])
            .filter(|size| *size <= source.max_archive_bytes)?,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::queue::WorkflowIdentity;
    use serde_json::json;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn selects_only_the_exact_attempt_and_producer_archive() {
        let evidence = RejectionEvidence {
            number: 17,
            removal_event: "removal".into(),
            removed_at: "2026-01-01T00:09:00Z".into(),
            notification_target: None,
            publication: None,
            merge_group_head: HEAD.into(),
            failed_required_check_runs: vec![42],
            failed_required_workflows: [(
                42,
                WorkflowIdentity {
                    check_suite_id: 51,
                    workflow_run_id: 61,
                    observed_run_attempt: 2,
                },
            )]
            .into(),
            verified_test_reports: Vec::new(),
        };
        let source = ReportSource {
            workflow_path: ".github/workflows/checks.yml",
            required_check_names: &BTreeSet::from(["required aggregate".into()]),
            producer_name: "mandatory shard 2",
            artifact_name: "test-report-2",
            max_archive_bytes: 1024,
        };
        let identity = evidence.report_attempt_identity(&[42]).unwrap();
        assert!(evidence.report_attempt_identity(&[9]).is_none());
        let mut shifted = evidence.clone();
        shifted.merge_group_head = DIGEST.into();
        assert!(shifted.report_attempt_identity(&[42]).unwrap() != identity);
        shifted = evidence.clone();
        shifted.removed_at = "2026-01-01T00:08:58Z".into();
        assert!(shifted.report_attempt_identity(&[42]).unwrap() != identity);
        shifted = evidence.clone();
        shifted
            .failed_required_workflows
            .get_mut(&42)
            .unwrap()
            .check_suite_id = 52;
        assert!(shifted.report_attempt_identity(&[42]).unwrap() != identity);
        let failed = json!({"id":42,"run_id":61,"run_attempt":2,"head_sha":HEAD,
            "status":"completed","name":"required aggregate",
            "conclusion":"failure","completed_at":"2026-01-01T00:08:59Z"});
        let attempt = json!({"id":61,"run_attempt":2,"head_sha":HEAD,"check_suite_id":51,
            "event":"merge_group","status":"completed","path":source.workflow_path,"run_started_at":"2026-01-01T00:04:00Z"});
        let producer = json!({"id":44,"run_id":61,"run_attempt":2,"head_sha":HEAD,
            "status":"completed","name":source.producer_name,
            "started_at":"2026-01-01T00:06:00Z","completed_at":"2026-01-01T00:08:00Z"});
        let artifact = json!({"id":71,"name":source.artifact_name,"size_in_bytes":128,
            "digest":format!("sha256:{DIGEST}"),"expired":false,
            "created_at":"2026-01-01T00:07:59Z","updated_at":"2026-01-01T00:07:59Z",
            "workflow_run":{"id":61,"head_sha":HEAD}});
        let verify = |failed: &Value, attempt: &Value, producer: &Value, listing: &Value| {
            let mut jobs = producer.clone();
            jobs["jobs"].as_array_mut().unwrap().push(failed.clone());
            jobs["total_count"] = (producer["total_count"].as_u64().unwrap() + 1).into();
            select(
                &evidence,
                42,
                &source,
                ActionMetadata {
                    attempt,
                    jobs: &jobs,
                    artifact_listing: listing,
                },
            )
            .map(|archive| (archive.artifact_id, archive.size_bytes, archive.sha256.to_owned()))
        };
        let producers = json!({"total_count":1,"jobs":[producer]});
        let listing = json!({"total_count":1,"artifacts":[artifact]});
        assert_eq!(
            verify(&failed, &attempt, &producers, &listing),
            Some((71, 128, DIGEST.into()))
        );
        let mut replaced_job = failed.clone();
        replaced_job["run_attempt"] = 3.into();
        assert!(verify(&replaced_job, &attempt, &producers, &listing).is_none());
        replaced_job = failed.clone();
        replaced_job["completed_at"] = "2026-01-01T00:07:30Z".into();
        assert!(verify(&replaced_job, &attempt, &producers, &listing).is_none());
        let mut unrelated = failed.clone();
        unrelated["name"] = "unrelated required check".into();
        assert!(verify(&unrelated, &attempt, &producers, &listing).is_none());
        let mut replaced_producer = producers.clone();
        replaced_producer["jobs"][0]["run_attempt"] = 1.into();
        assert!(verify(&failed, &attempt, &replaced_producer, &listing).is_none());
        let mut earlier = listing.clone();
        earlier["artifacts"][0]["created_at"] = "2026-01-01T00:05:59Z".into();
        assert!(verify(&failed, &attempt, &producers, &earlier).is_none());
        let mut partial = listing.clone();
        partial["total_count"] = 2.into();
        assert!(verify(&failed, &attempt, &producers, &partial).is_none());
        let duplicate = json!({"total_count":2,"artifacts":[artifact,artifact]});
        assert!(verify(&failed, &attempt, &producers, &duplicate).is_none());
        let duplicate_producer = json!({"total_count":2,"jobs":[producer,producer]});
        assert!(verify(&failed, &attempt, &duplicate_producer, &listing).is_none());
    }
}
