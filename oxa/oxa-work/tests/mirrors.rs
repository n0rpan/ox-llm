use oxa_work::board::assignment::{ManualAuthority, MirrorObservation, ProjectWorker};
use oxa_work::mirrors::{reconcile, AssignmentMirror};
use std::collections::BTreeSet;

#[derive(Default)]
struct FakeMirror {
    labels: BTreeSet<String>,
    has_project: bool,
    worker: Option<String>,
    reads: usize,
    writes: usize,
    fail: bool,
}

impl AssignmentMirror for FakeMirror {
    fn observe(&mut self, number: u64) -> oxa_work::Result<MirrorObservation> {
        assert_eq!(number, 42);
        self.reads += 1;
        if self.fail {
            return Err("synthetic unavailable mirror".into());
        }
        Ok(MirrorObservation {
            labels: self.labels.clone(),
            has_project: self.has_project,
        })
    }
    fn project_worker(&mut self, _: u64) -> oxa_work::Result<Option<String>> {
        Ok(self.worker.clone())
    }
    fn add_worker_label(&mut self, _: u64, label: &str) -> oxa_work::Result<()> {
        assert!(label.starts_with("worker:"));
        self.writes += 1;
        self.labels.insert(label.into());
        Ok(())
    }
    fn remove_worker_label(&mut self, _: u64, label: &str) -> oxa_work::Result<()> {
        assert!(label.starts_with("worker:"));
        self.writes += 1;
        self.labels.remove(label);
        Ok(())
    }
    fn set_project_worker(&mut self, _: u64, worker: Option<&str>) -> oxa_work::Result<()> {
        assert!(self.has_project);
        self.writes += 1;
        self.worker = worker.map(str::to_owned);
        Ok(())
    }
}

#[test]
fn phase_one_has_no_mirror_reads_or_writes() {
    let mut sink = FakeMirror {
        fail: true,
        ..FakeMirror::default()
    };
    assert!(reconcile(42, Some("codex:fixture/worker-a"), None, &mut sink)
        .unwrap()
        .is_none());
    assert_eq!((sink.reads, sink.writes), (0, 0));
}

#[test]
fn authoritative_reassignment_preserves_other_labels_and_is_idempotent() {
    let mut sink = FakeMirror {
        labels: ["worker:old", "release-pinned", "review:t3"]
            .map(str::to_owned)
            .into(),
        has_project: true,
        worker: Some("codex:fixture/old".into()),
        ..FakeMirror::default()
    };
    reconcile(
        42,
        Some("codex:fixture/worker-a"),
        Some(ManualAuthority::Owner),
        &mut sink,
    )
    .unwrap();
    assert_eq!(
        sink.labels,
        ["worker:worker-a", "release-pinned", "review:t3"]
            .map(str::to_owned)
            .into()
    );
    assert_eq!(sink.worker.as_deref(), Some("codex:fixture/worker-a"));
    assert_eq!(sink.writes, 3);
    let plan = reconcile(
        42,
        Some("codex:fixture/worker-a"),
        Some(ManualAuthority::Owner),
        &mut sink,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.project_worker, ProjectWorker::Unchanged);
    assert_eq!(sink.writes, 3);
    reconcile(42, None, Some(ManualAuthority::Lead), &mut sink).unwrap();
    assert_eq!(
        sink.labels,
        ["release-pinned", "review:t3"].map(str::to_owned).into()
    );
    assert!(sink.worker.is_none());
}

#[test]
fn no_project_needs_no_field_operation_and_read_failure_changes_nothing() {
    let mut sink = FakeMirror::default();
    reconcile(
        42,
        Some("oxa:fixture/worker-b"),
        Some(ManualAuthority::Lead),
        &mut sink,
    )
    .unwrap();
    assert_eq!(sink.writes, 1);
    sink.fail = true;
    assert!(reconcile(42, None, Some(ManualAuthority::Owner), &mut sink).is_err());
    assert_eq!(sink.writes, 1);
}

#[test]
fn overlong_mirror_alias_is_refused_before_io() {
    let mut sink = FakeMirror::default();
    let id = format!("codex:fixture/{}", "a".repeat(44));
    assert!(reconcile(42, Some(&id), Some(ManualAuthority::Owner), &mut sink).is_err());
    assert_eq!((sink.reads, sink.writes), (0, 0));
}
