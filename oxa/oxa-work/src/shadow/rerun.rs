//! Reserve one exact job rerun in the existing owner journal before publishing.
use super::{append_publication, open_log_with_create, Admissions, Board, File};
use crate::{LandingPublisher, RerunAcknowledgment, RerunIntent, RerunKey, Result, RunPolicy};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Default)]
pub struct Policy {
    pub enabled: bool,
    pub publisher_id: String,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Status {
    Skipped,
    Reserved,
    Acknowledged(RerunAcknowledgment),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "state", content = "binding", rename_all = "snake_case")]
pub(super) enum Record {
    Reserved(RerunIntent),
    Acknowledged(RerunAcknowledgment),
}

#[derive(Default)]
pub(super) struct History(BTreeMap<RerunKey, Record>);

impl History {
    pub(super) fn observe(&mut self, record: Record) -> Result<()> {
        let intent = match &record {
            Record::Reserved(intent) => intent,
            Record::Acknowledged(ack) => &ack.intent,
        };
        let valid_transition = match (&record, self.0.get(&intent.key)) {
            (Record::Reserved(_), None) => true,
            (Record::Acknowledged(_), Some(Record::Reserved(before))) => before == intent,
            _ => false,
        };
        if !intent.valid() || !valid_transition {
            return Err("invalid rerun journal transition".into());
        }
        self.0.insert(intent.key.clone(), record);
        Ok(())
    }

    fn status(&self, key: &RerunKey) -> Option<Status> {
        self.0.get(key).map(|record| match record {
            Record::Reserved(_) => Status::Reserved,
            Record::Acknowledged(ack) => Status::Acknowledged(ack.clone()),
        })
    }
}

/// The caller retains eligibility policy and supplies the exact intent again
/// from the existing Reader under the owner journal lock.
pub fn run_once(
    policy: &Policy,
    journal_path: &Path,
    repo: &Path,
    run: &RunPolicy,
    gh_binary: &Path,
    intent: RerunIntent,
    read: impl FnOnce() -> Result<(Board, Option<RerunIntent>)>,
) -> Result<Status> {
    if !policy.enabled || run.offline {
        return Ok(Status::Skipped);
    }
    let mut log = open_log_with_create(journal_path, false)?;
    if log
        .metadata()
        .map_err(|_| "cannot inspect the owner journal")?
        .len()
        == 0
    {
        return Err("rerun history is missing from the owner journal".into());
    }
    let history = Admissions::load(&mut log)?;
    if let Some(status) = history.reruns.status(&intent.key) {
        return Ok(status);
    }
    let inherited = log
        .try_clone()
        .map_err(|_| "cannot retain the owner journal lease")?;
    let publisher = LandingPublisher::new(repo, run, gh_binary, &inherited);
    let target = publisher.rerun_target(&intent, &policy.publisher_id)?;
    dispatch(&mut log, intent, read, |intent| {
        publisher.rerun_reserved(target, intent)
    })
}

fn dispatch(
    log: &mut File,
    intent: RerunIntent,
    read: impl FnOnce() -> Result<(Board, Option<RerunIntent>)>,
    send: impl FnOnce(RerunIntent) -> Result<RerunAcknowledgment>,
) -> Result<Status> {
    let (board, fresh) = read()?;
    if fresh.as_ref() != Some(&intent) {
        return Ok(Status::Skipped);
    }
    append_publication(log, &board, None, Some(&Record::Reserved(intent.clone())))?;
    let ack = send(intent.clone())?;
    if ack.intent != intent {
        return Err("rerun acknowledgment changed the reserved binding".into());
    }
    append_publication(log, &board, None, Some(&Record::Acknowledged(ack.clone())))?;
    Ok(Status::Acknowledged(ack))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_attempt_refuses_dispatch_and_ambiguous_request_stays_reserved_after_restart() {
        let temp = crate::test_dir::PrivateDir::new("rerun-journal");
        let path = temp.path().join("owner.jsonl");
        let intent = crate::publish::rerun::test_intent();
        let run = RunPolicy {
            git_binary: temp.path().join("git"),
            host_path: Default::default(),
            terminate_binary: None,
            offline: false,
            runner_owns_github: false,
            storage: temp.path().to_owned(),
        };
        let launch = |policy: &Policy| {
            run_once(
                policy,
                &path,
                temp.path(),
                &run,
                &temp.path().join("gh"),
                intent.clone(),
                || panic!("disabled or previously reserved request must not read"),
            )
        };
        assert_eq!(launch(&Policy::default()).unwrap(), Status::Skipped);
        assert!(!path.exists());
        {
            let mut log = super::super::open_log(&path).unwrap();
            let moved = RerunIntent {
                run_attempt: 3,
                ..intent.clone()
            };
            assert_eq!(
                dispatch(
                    &mut log,
                    intent.clone(),
                    || Ok((Board::default(), Some(moved))),
                    |_| panic!("changed attempt must not publish")
                )
                .unwrap(),
                Status::Skipped
            );
            assert!(dispatch(
                &mut log,
                intent.clone(),
                || Ok((Board::default(), Some(intent.clone()))),
                |_| Err("connection lost".into())
            )
            .is_err());
        }
        let policy = Policy {
            enabled: true,
            publisher_id: "U_publisher".into(),
        };
        assert_eq!(launch(&policy).unwrap(), Status::Reserved);
    }
}
