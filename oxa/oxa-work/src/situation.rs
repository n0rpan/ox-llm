//! Bounded projection of typed facts. This module never reads or writes runner task state.
use crate::Result;
use oxa_proto::work::{Ask, Event, Fact, Incident, Snapshot, TaskStep, MAX_ITEMS};

pub(crate) fn empty() -> Snapshot {
    Snapshot {
        as_of_seq: 0,
        at_ms: 0,
        standing_revision: 0,
        goal: None,
        running: vec![],
        local_accepted: vec![],
        published: vec![],
        landed: vec![],
        blocked: vec![],
        asks: vec![],
        incidents: vec![],
        history_truncated: false,
    }
}

pub(crate) fn apply(snapshot: &mut Snapshot, event: &Event) -> Result<()> {
    if snapshot.as_of_seq.checked_add(1) != Some(event.seq) {
        return Err("situation replay has a sequence gap".into());
    }
    match &event.fact {
        Fact::TaskStarted(task) => {
            snapshot.running.retain(|row| row.task != task.task);
            snapshot.running.push(task.clone());
        }
        Fact::TaskTransition(change) => {
            if change.step == TaskStep::Finished {
                snapshot.running.retain(|row| row.task != change.task);
            } else {
                let task = snapshot
                    .running
                    .iter_mut()
                    .find(|row| row.task == change.task)
                    .ok_or("task transition has no running location")?;
                task.step = change.step;
                task.session_id = change.session_id.clone();
            }
        }
        Fact::WorkerFinished(worker) => {
            if let Some(task) = snapshot.running.iter_mut().find(|row| row.task == worker.task) {
                task.session_id = worker.session_id.clone();
            }
        }
        Fact::LocalAcceptance(accepted) => {
            snapshot.local_accepted.retain(|row| row.task != accepted.task);
            snapshot.local_accepted.push(accepted.clone());
        }
        Fact::Published(publication) => {
            snapshot.published.retain(|row| row.task != publication.task);
            snapshot.published.push(publication.clone());
        }
        Fact::PrMerged(landed) => {
            let tasks: Vec<_> = snapshot
                .published
                .iter()
                .filter(|row| row.pr == landed.pr)
                .map(|row| row.task.clone())
                .collect();
            snapshot
                .local_accepted
                .retain(|row| !(tasks.contains(&row.task) && row.commit == landed.pr.head));
            snapshot.published.retain(|row| row.pr != landed.pr);
            snapshot.landed.retain(|row| row.pr.number != landed.pr.number);
            snapshot.landed.push(landed.clone());
        }
        Fact::Blocked(block) => {
            snapshot.blocked.retain(|row| row.subject != block.subject);
            snapshot.blocked.push(block.clone());
        }
        Fact::Unblocked(subject) => snapshot.blocked.retain(|row| &row.subject != subject),
        Fact::AskOpened(ask) => open_ask(snapshot, ask)?,
        Fact::AskClosed(ask) => snapshot.asks.retain(|row| row.id != ask.id),
        Fact::Incident(incident) => add_incident(snapshot, incident)?,
        Fact::StandingChanged(change) => {
            if change.revision <= snapshot.standing_revision {
                return Err("stale standing projection revision".into());
            }
            snapshot.standing_revision = change.revision;
            snapshot.goal = change.goal.clone();
            if let Some(ask) = &change.ask_opened {
                open_ask(snapshot, ask)?;
            }
            if let Some(ask) = &change.ask_closed {
                snapshot.asks.retain(|row| row.id != ask.id);
            }
            if let Some(incident) = &change.incident {
                add_incident(snapshot, incident)?;
            }
        }
        Fact::ExternalAcceptance(_) | Fact::Enqueued(_) | Fact::Mechanical(_) => {}
    }
    const HISTORY: usize = 16;
    for removed in [
        snapshot
            .landed
            .drain(..snapshot.landed.len().saturating_sub(HISTORY))
            .count(),
        snapshot
            .incidents
            .drain(..snapshot.incidents.len().saturating_sub(HISTORY))
            .count(),
    ] {
        snapshot.history_truncated |= removed > 0;
    }
    while snapshot.running.len()
        + snapshot.local_accepted.len()
        + snapshot.published.len()
        + snapshot.landed.len()
        + snapshot.blocked.len()
        + snapshot.asks.len()
        + snapshot.incidents.len()
        > MAX_ITEMS
    {
        if !discard_history(snapshot) {
            return Err("active situation capacity exceeded".into());
        }
    }
    snapshot.as_of_seq = event.seq;
    snapshot.at_ms = event.at_ms;
    while let Err(error) = snapshot.to_json() {
        if !discard_history(snapshot) {
            return Err(error.to_string());
        }
    }
    Ok(())
}

fn open_ask(snapshot: &mut Snapshot, ask: &Ask) -> Result<()> {
    if let Some(existing) = snapshot.asks.iter().find(|row| row.id == ask.id) {
        if existing != ask {
            return Err("ask id has conflicting contents".into());
        }
    } else {
        snapshot.asks.push(ask.clone());
    }
    Ok(())
}
fn add_incident(snapshot: &mut Snapshot, incident: &Incident) -> Result<()> {
    if let Some(existing) = snapshot.incidents.iter().find(|row| row.id == incident.id) {
        if existing != incident {
            return Err("incident id has conflicting contents".into());
        }
    } else {
        snapshot.incidents.push(incident.clone());
    }
    Ok(())
}

fn discard_history(snapshot: &mut Snapshot) -> bool {
    if !snapshot.landed.is_empty() {
        snapshot.landed.remove(0);
    } else if !snapshot.incidents.is_empty() {
        snapshot.incidents.remove(0);
    } else {
        return false;
    }
    snapshot.history_truncated = true;
    true
}
