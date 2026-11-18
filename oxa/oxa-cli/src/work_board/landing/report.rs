//! Download an exact Actions report through the existing Board Reader.
use super::Reader;
use oxa_work::board::queue::{
    report::{
        attempt_for, select, ActionMetadata, ReportArchive, ReportSource, VerifiedArchive, VerifiedReports,
    },
    RejectionEvidence,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

pub(in crate::work_board) struct Settings {
    workflow_path: String,
    required_check_names: BTreeSet<String>,
    reports: Vec<(String, String)>,
    verified: BTreeMap<(String, u64, u64), VerifiedReports>,
}

impl Settings {
    pub(in crate::work_board) fn read(path: &Path) -> Result<Self, String> {
        let input = oxa_work::shadow::read_queue_report_policy(path)?;
        let invalid = "invalid queue report policy";
        let policy: Value = serde_json::from_slice(&input).map_err(|_| invalid)?;
        if policy.as_object().is_none_or(|map| map.len() != 3) {
            return Err(invalid.into());
        }
        let workflow_path = policy["workflow_path"]
            .as_str()
            .filter(|path| path.starts_with(".github/workflows/") && !path.chars().any(char::is_control))
            .ok_or(invalid)?
            .to_owned();
        let reports = policy["reports"]
            .as_array()
            .filter(|rows| !rows.is_empty())
            .ok_or(invalid)?;
        let label = |value: &Value| {
            value
                .as_str()
                .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or(invalid)
        };
        let checks = policy["required_check_names"]
            .as_array()
            .filter(|rows| !rows.is_empty())
            .ok_or(invalid)?;
        let required_check_names = checks.iter().map(label).collect::<Result<BTreeSet<_>, _>>()?;
        if required_check_names.len() != checks.len() {
            return Err(invalid.into());
        }
        let mut names = BTreeSet::new();
        let mut sources = Vec::new();
        for row in reports {
            if row.as_object().is_none_or(|map| map.len() != 2) {
                return Err(invalid.into());
            }
            let producer = label(&row["producer_name"])?;
            let artifact = label(&row["artifact_name"])?;
            if !names.insert(artifact.clone()) {
                return Err(invalid.into());
            }
            sources.push((producer, artifact));
        }
        Ok(Self {
            workflow_path,
            required_check_names,
            reports: sources,
            verified: BTreeMap::new(),
        })
    }
}

impl Reader<'_> {
    fn actions_get(&self, path: &str) -> Command {
        let mut command = self.api_command();
        command.args(["--method", "GET", &format!("repos/{}/actions/{path}", self.repo)]);
        command
    }

    fn actions_json(&self, path: &str, observed_bytes: &mut usize) -> Result<Value, String> {
        let (code, text) = oxa_work::command_runner(
            self.actions_get(path),
            Some(crate::work_board::MAX_RESPONSE),
            oxa_work::COMMAND_DEADLINE,
        )
        .ok_or("GitHub Actions response exceeds its limit or timed out")?;
        if code != 0 {
            return Err("GitHub Actions metadata unavailable".into());
        }
        crate::work_board::charge(observed_bytes, text.len())?;
        serde_json::from_str(&text).map_err(|_| "invalid GitHub Actions metadata".into())
    }

    pub(in crate::work_board) fn sample_test_reports(
        &self,
        rejections: &mut [RejectionEvidence],
        settings: &mut Settings,
    ) -> Result<(), String> {
        let mut previous = std::mem::take(&mut settings.verified);
        let mut observed_bytes = 0;
        for rejection in rejections {
            let mut attempts = BTreeMap::<_, Vec<_>>::new();
            for id in rejection.workflow_checks() {
                if let Some(attempt) = attempt_for(rejection, id) {
                    attempts.entry(attempt).or_default().push(id);
                }
            }
            for ((run, attempt), checks) in attempts {
                let key = (rejection.removal_event().to_owned(), run, attempt);
                let proof = match previous.remove(&key) {
                    Some(saved) if saved.failed_check_runs == checks => Some(saved),
                    _ => self.verify_attempt_reports(
                        rejection,
                        checks,
                        (run, attempt),
                        settings,
                        &mut observed_bytes,
                    )?,
                };
                if let Some(proof) = proof {
                    rejection.record_verified_reports(proof.clone());
                    settings.verified.insert(key, proof);
                }
            }
        }
        Ok(())
    }

    fn verify_attempt_reports(
        &self,
        rejection: &RejectionEvidence,
        checks: Vec<u64>,
        (run, attempt): (u64, u64),
        settings: &Settings,
        observed_bytes: &mut usize,
    ) -> Result<Option<VerifiedReports>, String> {
        let workflow = self.actions_json(&format!("runs/{run}/attempts/{attempt}"), observed_bytes)?;
        if workflow["path"].as_str() != Some(settings.workflow_path.as_str()) {
            return Ok(None);
        }
        let producer_jobs = self.actions_json(
            &format!("runs/{run}/attempts/{attempt}/jobs?per_page=100"),
            observed_bytes,
        )?;
        let artifacts = self.actions_json(&format!("runs/{run}/artifacts?per_page=100"), observed_bytes)?;
        let metadata = ActionMetadata {
            attempt: &workflow,
            jobs: &producer_jobs,
            artifact_listing: &artifacts,
        };
        let mut archives = Vec::new();
        for (producer_name, artifact_name) in &settings.reports {
            let source = ReportSource {
                workflow_path: &settings.workflow_path,
                required_check_names: &settings.required_check_names,
                producer_name,
                artifact_name,
                max_archive_bytes: crate::work_board::MAX_RESPONSE as u64,
            };
            let mut selected = checks.iter().map(|id| select(rejection, *id, &source, metadata));
            let Some(Some(first)) = selected.next() else {
                return Ok(None);
            };
            if selected.any(|archive| archive.as_ref() != Some(&first)) {
                return Ok(None);
            }
            archives.push(first);
        }
        let mut verified = Vec::new();
        for report in archives {
            self.verify_archive(&report, observed_bytes)?;
            verified.push(VerifiedArchive {
                id: report.artifact_id,
                sha256: report.sha256.to_owned(),
            });
        }
        Ok(Some(VerifiedReports {
            failed_check_runs: checks,
            workflow_run: run,
            run_attempt: attempt,
            archives: verified,
        }))
    }

    fn verify_archive(&self, selected: &ReportArchive<'_>, observed_bytes: &mut usize) -> Result<(), String> {
        let size = usize::try_from(selected.size_bytes).map_err(|_| "invalid GitHub archive size")?;
        crate::work_board::charge(observed_bytes, size)?;
        let (code, bytes) = oxa_work::command_output_bytes(
            self.actions_get(&format!("artifacts/{}/zip", selected.artifact_id)),
            Some(size),
            oxa_work::COMMAND_DEADLINE,
        )
        .ok_or("GitHub test archive exceeds its limit or timed out")?;
        if code != 0 || bytes.len() != size {
            return Err("GitHub test archive unavailable or incomplete".into());
        }
        if format!("{:x}", Sha256::digest(&bytes)) != selected.sha256 {
            return Err("GitHub test archive digest mismatch".into());
        }
        Ok(())
    }
}
