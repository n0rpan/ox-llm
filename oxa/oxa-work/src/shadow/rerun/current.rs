use super::{append_publication, locked_history, publish, Board, Policy, Record, Result, RunPolicy, Status};
use crate::board::check_recovery::{decide, Current, Decision, History};
use crate::RerunIntent;
use serde_json::Value;
use std::path::Path;

/// A selected check and its snapshot from the protected Board reader.
pub struct CurrentSample<'a> {
    pub board: &'a Board,
    pub target: Current<'a>,
}

/// Use both read callbacks from the same bounded Board Reader. This entry point
/// has no production caller. The publisher and the journal stay unchanged when off.
pub fn run_current_once(
    policy: &Policy,
    journal_path: &Path,
    repo: &Path,
    run: &RunPolicy,
    gh_binary: &Path,
    sample: CurrentSample<'_>,
    reader: (
        impl FnMut(&str) -> Result<Value>,
        impl FnMut(&str) -> Result<Value>,
    ),
) -> Result<Status> {
    if !policy.enabled || run.offline {
        return Ok(Status::Skipped);
    }
    let (mut log, history) = locked_history(journal_path)?;
    let (mut repository_read, mut actions_json) = reader;
    let mut observe = || {
        sample
            .target
            .observe(&mut repository_read, &mut actions_json, |verified| {
                verified.map(|verified| {
                    let intent = RerunIntent::from(verified.attempt);
                    let next = decide(
                        true,
                        &verified.observation,
                        sample.board.observed_at,
                        History::Unused,
                    );
                    let eligible = next == Decision::RerunJob(intent.job_id);
                    (intent, eligible)
                })
            })
    };
    let Some((intent, eligible)) = observe()? else {
        return Ok(Status::Skipped);
    };
    match history.reruns.status(&intent.key) {
        Some(Status::Acknowledged(acknowledgment)) if intent.replaces(&acknowledgment.intent) => {
            let record = Record::Reconciled {
                acknowledgment: acknowledgment.clone(),
                replacement: intent.clone(),
            };
            append_publication(&mut log, sample.board, None, Some(&record))?;
            Ok(Status::Reconciled {
                acknowledgment,
                replacement: intent,
            })
        }
        Some(status) => Ok(status),
        None if !eligible => Ok(Status::Skipped),
        None => publish(&mut log, policy, repo, run, gh_binary, intent, || {
            let fresh = observe()?
                .filter(|(_, eligible)| *eligible)
                .map(|(intent, _)| intent);
            Ok((sample.board.clone(), fresh))
        }),
    }
}
