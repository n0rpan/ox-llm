use oxa_work::board::{assignment::*, Action, Entry};
use std::collections::BTreeSet;

fn worker(roles: &[Role]) -> Worker {
    Worker {
        id: "codex:node/slot-1".into(),
        kind: WorkerKind::Codex,
        skills: BTreeSet::from(["rust".into()]),
        roles: roles.iter().copied().collect(),
    }
}

#[test]
fn action_roles_and_owner_only_are_enforced_before_pins() {
    let mut policy = AssignmentPolicy::default();
    let builder = worker(&[Role::Builder]);
    assert!(policy.allows(&builder, Action::FixFindings));
    assert!(!policy.allows(&builder, Action::Ready));
    assert!(!policy.allows(&worker(&[Role::Reviewer]), Action::FixFindings));
    assert!(!policy.allows(&worker(&[Role::Builder, Role::Reviewer]), Action::FixFindings));
    assert!(policy.allows(&worker(&[Role::Orchestrator]), Action::Ready));
    assert!(!policy.allows(&builder, Action::WaitReview));
    policy
        .control(
            Some(ManualAuthority::Lead),
            Control::Pin(Pin::Worker(builder.id.clone())),
        )
        .unwrap();
    policy
        .control(Some(ManualAuthority::Owner), Control::OwnerOnly(true))
        .unwrap();
    assert!(!policy.allows(&builder, Action::FixFindings));
    assert!(policy.allows(&worker(&[Role::Owner]), Action::FixFindings));
}

#[test]
fn kinds_skills_and_workflow_role_cannot_be_bypassed_by_manual_pins() {
    let mut policy = AssignmentPolicy::default();
    let mut candidate = worker(&[Role::Builder]);
    let needs = Requirements {
        kinds: BTreeSet::from([WorkerKind::Codex]),
        skills: BTreeSet::from(["workflows".into()]),
        any_roles: BTreeSet::from([Role::Orchestrator]),
    };
    policy
        .control(Some(ManualAuthority::Lead), Control::Require(needs))
        .unwrap();
    policy
        .control(
            Some(ManualAuthority::Lead),
            Control::Pin(Pin::Kind(WorkerKind::Codex)),
        )
        .unwrap();
    assert!(!policy.allows(&candidate, Action::FixRequiredCi));
    candidate.skills.insert("workflows".into());
    assert!(!policy.allows(&candidate, Action::FixRequiredCi));
    candidate.roles.insert(Role::Orchestrator);
    assert!(policy.allows(&candidate, Action::FixRequiredCi));
    candidate.kind = WorkerKind::Claude;
    assert!(!policy.allows(&candidate, Action::FixRequiredCi));
}

#[test]
fn workers_cannot_change_manual_controls_and_bad_ids_are_refused() {
    let mut policy = AssignmentPolicy::default();
    assert!(policy.control(None, Control::OwnerOnly(true)).is_err());
    assert!(!policy.owner_only);
    assert!(policy
        .control(
            Some(ManualAuthority::Owner),
            Control::Pin(Pin::Worker("bad\nid".into()))
        )
        .is_err());
    assert!(policy.pin.is_none());
    policy
        .control(
            Some(ManualAuthority::Owner),
            Control::Pin(Pin::Kind(WorkerKind::Oxa)),
        )
        .unwrap();
    policy
        .control(Some(ManualAuthority::Lead), Control::Unpin)
        .unwrap();
    assert!(policy.pin.is_none());
}

#[test]
fn release_pins_then_manual_pins_rank_before_automatic_picks() {
    let worker = worker(&[Role::Builder]);
    let automatic = AssignmentPolicy::default();
    let mut pinned = automatic.clone();
    pinned
        .control(
            Some(ManualAuthority::Owner),
            Control::Pin(Pin::Worker(worker.id.clone())),
        )
        .unwrap();
    let mut entry = Entry {
        number: 1,
        head: "synthetic".into(),
        release_pinned: false,
        action: Action::FixFindings,
        lease: None,
        attempts_today: 0,
        eligible: true,
    };
    assert!(pinned.rank(&worker, &entry) < automatic.rank(&worker, &entry));
    let nonrelease = pinned.rank(&worker, &entry).unwrap();
    entry.release_pinned = true;
    assert!(automatic.rank(&worker, &entry).unwrap() < nonrelease);
    entry.eligible = false;
    assert!(pinned.rank(&worker, &entry).is_none());
}

#[test]
fn mirror_plans_clear_on_release_and_preserve_unrelated_labels() {
    let observed = MirrorObservation {
        labels: BTreeSet::from(["bug".into(), "worker:old".into()]),
        has_project: true,
    };
    let assigned = MirrorPlan::derive(Some("codex:node/slot-1"), &observed).unwrap();
    assert_eq!(assigned.add_labels, BTreeSet::from(["worker:slot-1".into()]));
    assert_eq!(assigned.remove_labels, BTreeSet::from(["worker:old".into()]));
    assert_eq!(assigned.project_worker, Some(Some("codex:node/slot-1".into())));
    let released = MirrorPlan::derive(None, &observed).unwrap();
    assert!(released.add_labels.is_empty());
    assert_eq!(released.remove_labels, assigned.remove_labels);
    assert_eq!(released.project_worker, Some(None));
    assert_eq!(
        MirrorPlan::derive(None, &MirrorObservation::default())
            .unwrap()
            .project_worker,
        None
    );
    assert!(MirrorPlan::derive(Some("bad\nworker"), &observed).is_err());
}
