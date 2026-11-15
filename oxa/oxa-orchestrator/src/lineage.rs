//! Read-only fan-in across an explicitly selected, exclusively owned run chain.
use crate::{
    a2a_recovery::{ExclusiveRun, Phase},
    a2a_run::A2ARunReport,
    resume::ResumeReceipt,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Collect every root worker's original report, or fail rather than omit an
/// unresolved worker and imply successful fan-in. Paths are never discovered.
/// Hold every owner for this call; no launch, agent access, mutation or clock
/// deadline check is needed to recover already-written results.
pub fn collect(owners: &[&ExclusiveRun]) -> io::Result<A2ARunReport> {
    if owners.is_empty() || owners.len() > 16 {
        return Err(invalid("select 1..16 run journals in root-to-leaf order"));
    }
    let mut seen = BTreeSet::new();
    for owner in owners {
        let snapshot = owner.inspected().snapshot();
        if snapshot.unlisted_workers != 0 || !seen.insert(snapshot.journal_sha256.as_str()) {
            return Err(invalid("lineage requires complete unique A2A manifests"));
        }
    }
    for edge in owners.windows(2) {
        verify_edge(edge[0], edge[1])?;
    }
    let root = owners[0].inspected().snapshot();
    let root_ids = root
        .workers
        .iter()
        .map(|w| w.worker.as_str())
        .collect::<BTreeSet<_>>();
    let mut reports = BTreeMap::new();
    for (index, owner) in owners.iter().enumerate() {
        for worker in &owner.inspected().snapshot().workers {
            match worker.phase {
                Phase::Finished => {
                    let report = owner
                        .inspected()
                        .load_checkpoint_at(&worker.worker, &owner.directory)?;
                    if !root_ids.contains(report.worker.as_str())
                        || reports.insert(report.worker.clone(), report).is_some()
                    {
                        return Err(invalid("duplicate or unrelated worker result in lineage"));
                    }
                }
                Phase::Queued if index + 1 < owners.len() => {}
                _ => return Err(invalid("lineage ends with unresolved worker outcomes")),
            }
        }
    }
    if reports.len() != root.expected_workers
        || reports.keys().map(String::as_str).collect::<BTreeSet<_>>() != root_ids
    {
        return Err(invalid("lineage does not account for every root worker"));
    }
    Ok(A2ARunReport {
        run: root.run.clone(),
        workers: reports.into_values().collect(),
    })
}

fn verify_edge(parent: &ExclusiveRun, child: &ExclusiveRun) -> io::Result<()> {
    let source = parent.inspected().snapshot();
    let target = child.inspected().snapshot();
    if source.recorded_run_finished || source.truncated_tail {
        return Err(invalid("completed or torn parent cannot delegate a queue"));
    }
    let bytes = crate::recovery::read_bounded_at(&parent.directory, "resume.json", 64 * 1024)?;
    let receipt: ResumeReceipt =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid resume receipt"))?;
    let actual_path = std::fs::read_link(directory(child))?;
    let destination_sha256 = format!("{:x}", Sha256::digest(actual_path.as_os_str().as_bytes()));
    if receipt.version != 1
        || receipt.journal_sha256 != source.journal_sha256
        || receipt.run != source.run
        || receipt.child_run != target.run
        || receipt.child_run != format!("resume-{}", &source.journal_sha256[..32])
        || receipt.destination_sha256 != destination_sha256
    {
        return Err(invalid(
            "resume receipt does not bind the selected parent and child",
        ));
    }
    let queued = source
        .workers
        .iter()
        .filter(|w| w.phase == Phase::Queued)
        .map(|w| (w.worker.as_str(), w))
        .collect::<BTreeMap<_, _>>();
    let receipt_ids = receipt
        .workers
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let child_ids = target
        .workers
        .iter()
        .map(|w| w.worker.as_str())
        .collect::<BTreeSet<_>>();
    if queued.is_empty()
        || receipt_ids.len() != receipt.workers.len()
        || queued.len() != target.expected_workers
        || queued.keys().copied().collect::<BTreeSet<_>>() != receipt_ids
        || receipt_ids != child_ids
    {
        return Err(invalid("resume receipt or child changed the queued manifest"));
    }
    let mut limits = source
        .resume_binding
        .clone()
        .ok_or_else(|| invalid("missing parent resume limits"))?;
    let mut reserved = 0usize;
    for worker in &target.workers {
        let original = queued[worker.worker.as_str()];
        let binding = original
            .resume_binding
            .as_ref()
            .ok_or_else(|| invalid("missing queued worker binding"))?;
        if worker.resume_binding.as_ref() != Some(binding) {
            return Err(invalid("child changed an original worker binding"));
        }
        reserved = reserved
            .checked_add(binding.response_bytes)
            .ok_or_else(|| invalid("lineage reservation overflow"))?;
    }
    limits.response_reservation = reserved;
    if receipt.limits != limits || target.resume_binding.as_ref() != Some(&limits) {
        return Err(invalid("child changed original run allowances or deadline"));
    }
    // The writer only delegates queued workers after all other workers have a
    // durable finish. A forged/inconsistent parent must not conceal ambiguity.
    if source.workers.iter().any(|w| {
        !matches!(w.phase, Phase::Queued | Phase::Finished)
            || (w.phase == Phase::Finished
                && (w.recorded_stop == Some(crate::a2a_run::StopConfirmation::Unconfirmed)
                    || (w.binding.is_some()
                        && !w
                            .recorded_task_state
                            .as_ref()
                            .is_some_and(oxa_a2a::wire::TaskState::is_terminal))))
    }) {
        return Err(invalid("parent has ambiguous worker lifecycle"));
    }
    Ok(())
}
fn directory(owner: &ExclusiveRun) -> PathBuf {
    format!("/proc/self/fd/{}", owner.directory.as_raw_fd()).into()
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
