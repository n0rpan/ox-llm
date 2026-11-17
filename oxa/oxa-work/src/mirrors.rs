//! Phase-2 adapter interface only. The phase-1 board CLI never calls this module.
use crate::board::assignment::{valid_worker_id, ManualAuthority, MirrorObservation, MirrorPlan};
use crate::Result;

/// An authenticated orchestrator adapter supplies these operations, with scrubbed errors.
/// No HTTP/process implementation or credentials are installed by this interface.
pub trait AssignmentMirror {
    fn observe(&mut self, number: u64) -> Result<MirrorObservation>;
    fn project_worker(&mut self, number: u64) -> Result<Option<String>>;
    fn add_worker_label(&mut self, number: u64, label: &str) -> Result<()>;
    fn remove_worker_label(&mut self, number: u64, label: &str) -> Result<()>;
    fn set_project_worker(&mut self, number: u64, worker: Option<&str>) -> Result<()>;
}

/// `assignee` comes from authoritative core state, including None after lease end.
/// None authority is default-off: it performs neither reads nor writes.
/// On failure, retry from a fresh authoritative assignment and fresh observation.
pub fn reconcile(
    number: u64,
    assignee: Option<&str>,
    authority: Option<ManualAuthority>,
    sink: &mut impl AssignmentMirror,
) -> Result<Option<MirrorPlan>> {
    if authority.is_none() {
        return Ok(None);
    }
    if number == 0 || assignee.is_some_and(|id| !valid_worker_id(id)) {
        return Err("invalid mirror item or assignee".into());
    }
    let observed = sink.observe(number)?;
    let plan = MirrorPlan::derive(assignee, &observed)?;
    for label in &plan.remove_labels {
        sink.remove_worker_label(number, label)?;
    }
    for label in &plan.add_labels {
        sink.add_worker_label(number, label)?;
    }
    if let Some(worker) = &plan.project_worker {
        if sink.project_worker(number)?.as_deref() != worker.as_deref() {
            sink.set_project_worker(number, worker.as_deref())?;
        }
    }
    Ok(Some(plan))
}
