use super::*;
use crate::board::assignment::{ManualAuthority, Role, Worker, WorkerKind};
use crate::test_dir::PrivateDir;
use crate::{blocked_record, Task, WorkerOutcome, WorkerResult};

pub(in crate::board::claims) fn fixture() -> (PrivateDir, Claims, ItemLease, NativeBinding) {
    let dir = PrivateDir::new("lease-activity");
    let claims = Claims::open(dir.path()).unwrap();
    let receipt = reserve(&claims, 10);
    let binding = NativeBinding::new(1, &"a".repeat(64), &receipt.lease, "agent/fixture").unwrap();
    assert!(claims.bind_native("worker-a", &binding).unwrap());
    (dir, claims, receipt, binding)
}

pub(in crate::board::claims) fn reserve(claims: &Claims, now: u64) -> ItemLease {
    let entry = Entry {
        number: 1,
        head: "synthetic-source".into(),
        release_pinned: false,
        action: Action::BuildIssue,
        lease: None,
        attempts_today: 0,
        eligible: true,
        evidence_incomplete: None,
    };
    let worker = Worker {
        id: "worker-a".into(),
        kind: WorkerKind::Codex,
        skills: Default::default(),
        roles: [Role::Builder].into(),
    };
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    claims
        .claim_item_for(&entry, &policy, &worker, "host-a", (now, 100), |_| Ok(None))
        .unwrap()
        .unwrap()
}

fn milestone(binding: &NativeBinding) -> Record {
    let task: Task = serde_json::from_value(serde_json::json!({
        "id":binding.native_task_id,"model":"fixture","branch":binding.branch,"prompt":"Synthetic task"
    }))
    .unwrap();
    let mut record = blocked_record(&task, "synthetic private detail".into());
    record.checkpoint_revision = 7;
    record.checkpoint_at_ms = 12_999;
    record.checkpoint_kind = CheckpointKind::WorkerFinished;
    record.step = Step::Acceptance;
    record.worker = Some(WorkerResult {
        outcome: WorkerOutcome::Done,
        exit_code: 0,
        error_class: None,
        interruption: None,
        tokens: None,
    });
    record
}

#[test]
fn heartbeat_never_masks_silence_and_recovery_cannot_refresh_action_time() {
    let (dir, claims, receipt, binding) = fixture();
    let observe = |now, signal: LeaseSignal<'_>| {
        claims.observe_item_for("worker-a", "host-a", &binding, &receipt, now, signal)
    };
    assert!(observe(11, LeaseSignal::Heartbeat).unwrap());
    assert!(!observe(11, LeaseSignal::Heartbeat).unwrap());
    let mut record = milestone(&binding);
    assert!(observe(13, LeaseSignal::Milestone(&record, 0)).is_err());
    assert!(observe(13, LeaseSignal::Milestone(&record, 7)).is_err());
    assert!(observe(13, LeaseSignal::Milestone(&record, 5)).unwrap());
    assert!(observe(30, LeaseSignal::Heartbeat).unwrap());
    assert!(observe(29, LeaseSignal::Heartbeat).is_err());
    assert!(!observe(35, LeaseSignal::Milestone(&record, 5)).unwrap());
    record.checkpoint_revision = 8;
    record.checkpoint_at_ms = 35_000;
    assert!(!observe(35, LeaseSignal::Milestone(&record, 5)).unwrap());
    record.checkpoint_revision = 10;
    record.checkpoint_at_ms = 36_000;
    assert!(observe(35, LeaseSignal::Milestone(&record, 9)).is_err());
    record.checkpoint_at_ms = 35_000;
    record.checkpoint_kind = CheckpointKind::TaskTransition;
    assert!(!observe(35, LeaseSignal::Milestone(&record, 9)).unwrap());
    record.checkpoint_kind = CheckpointKind::WorkerFinished;
    record.id = "foreign-attempt".into();
    assert!(!observe(35, LeaseSignal::Milestone(&record, 9)).unwrap());
    let rows = Claims::open(dir.path()).unwrap().assignments().unwrap();
    let row = &rows[&1];
    let activity = row.data.lease_activity.as_ref().unwrap();
    assert_eq!(activity.heartbeat_at, Some(30));
    assert_eq!(
        activity.last_action,
        Some(VerifiedMilestone {
            launch_revision: 5,
            revision: 7,
            at_ms: 12_999
        })
    );
    assert!(!row.stalled(22, Some(10)).unwrap());
    assert!(row.stalled(23, Some(10)).unwrap());
    assert!(!row.stalled(100, None).unwrap());
    assert!(row.stalled(23, Some(0)).is_err());
    assert!(row.stalled(11, Some(10)).is_err());
    assert!(row.stalled(23, Some(u64::MAX)).is_err());
    assert!(!serde_json::to_string(activity)
        .unwrap()
        .contains("synthetic private detail"));
    record.id.clone_from(&binding.native_task_id);
    record.checkpoint_revision = 12;
    record.checkpoint_at_ms = 40_000;
    assert!(observe(40, LeaseSignal::Milestone(&record, 11)).unwrap());
    assert!(!claims.assignments().unwrap()[&1].stalled(49, Some(10)).unwrap());
    assert!(
        claims
            .native_binding(&binding.native_task_id, &binding.branch)
            .unwrap()
            == Some(binding)
    );
}

#[test]
fn renewal_release_and_reassignment_fence_the_original_observer_receipt() {
    let (dir, claims, receipt, binding) = fixture();
    let heartbeat = |worker: &str, host: &str, receipt: &ItemLease, now| {
        claims.observe_item_for(worker, host, &binding, receipt, now, LeaseSignal::Heartbeat)
    };
    assert!(heartbeat("worker-b", "host-a", &receipt, 11).is_err());
    assert!(heartbeat("worker-a", "host-b", &receipt, 11).is_err());
    let mut different = binding.clone();
    different.reviewed_head = Some("b".repeat(40));
    assert!(!claims
        .observe_item_for(
            "worker-a",
            "host-a",
            &different,
            &receipt,
            11,
            LeaseSignal::Heartbeat
        )
        .unwrap());
    let renewed = claims
        .renew_item_for(1, "worker-a", "host-a", &receipt, 20, 200)
        .unwrap()
        .unwrap();
    assert!(!heartbeat("worker-a", "host-a", &receipt, 21).unwrap());
    assert!(heartbeat("worker-a", "host-a", &renewed, 21).unwrap());
    assert!(claims
        .release_for(1, "worker-a", &receipt.lease, 22, "finished")
        .unwrap());
    assert!(!heartbeat("worker-a", "host-a", &renewed, 23).unwrap());
    let data = claims.assignments().unwrap().remove(&1).unwrap().data;
    assert!(data.lease_activity.is_some());
    let next = reserve(&claims, 23);
    assert_ne!(next.lease.generation, receipt.lease.generation);
    assert!(Claims::open(dir.path()).unwrap().assignments().unwrap()[&1]
        .data
        .lease_activity
        .is_none());
    assert!(!heartbeat("worker-a", "host-a", &renewed, 24).unwrap());
}

#[test]
fn stalled_is_not_gone_and_the_existing_verified_reclaim_is_still_required() {
    let (_dir, claims, receipt, binding) = fixture();
    assert!(claims.assignments().unwrap()[&1].stalled(30, Some(10)).unwrap());
    let heartbeat = |now| {
        claims.observe_item_for(
            "worker-a",
            "host-a",
            &binding,
            &receipt,
            now,
            LeaseSignal::Heartbeat,
        )
    };
    assert!(heartbeat(11).unwrap());
    assert!(claims.assignments().unwrap()[&1].stalled(30, Some(10)).unwrap());
    assert!(!claims
        .reclaim_item(1, Some(ManualAuthority::Lead), &receipt, 30, |_| panic!(
            "not expired"
        ))
        .unwrap());
    assert!(!heartbeat(110).unwrap());
    assert!(!claims
        .reclaim_item(1, Some(ManualAuthority::Lead), &receipt, 110, |_| Ok(false))
        .unwrap());
    assert!(claims.assignments().unwrap()[&1].state.lease.is_some());
    assert!(claims
        .reclaim_item(1, Some(ManualAuthority::Lead), &receipt, 111, |_| Ok(true))
        .unwrap());
    assert!(claims.assignments().unwrap()[&1].state.lease.is_none());
    assert!(
        claims
            .native_binding(&binding.native_task_id, &binding.branch)
            .unwrap()
            == Some(binding)
    );
}
