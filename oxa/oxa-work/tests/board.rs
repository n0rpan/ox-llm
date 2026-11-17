use oxa_work::board::*;
use std::collections::BTreeMap;

struct FakeGitHub {
    prs: Vec<PullRequest>,
    reads: usize,
}

impl GitHub for FakeGitHub {
    fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
        self.reads += 1;
        Ok(self.prs.clone())
    }
}

fn pr(number: u64) -> PullRequest {
    PullRequest {
        number,
        head: "synthetic-head".into(),
        release_pinned: false,
        findings: false,
        required_ci: RequiredCi::Passed,
        mergeability: Mergeability::Mergeable,
        body: String::new(),
        removes_source: false,
        draft: false,
        reviewed: false,
    }
}

fn board(prs: Vec<PullRequest>, state: BTreeMap<u64, AttemptState>, now: u64) -> Board {
    let mut github = FakeGitHub { prs, reads: 0 };
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    let result = Board::read(&mut github, &policy, &state, now).unwrap();
    assert_eq!(github.reads, 1);
    result
}

#[test]
fn disabled_by_default_without_github_reads() {
    let mut github = FakeGitHub {
        prs: vec![pr(1)],
        reads: 0,
    };
    let result = Board::read(&mut github, &Policy::default(), &BTreeMap::new(), 0).unwrap();
    assert!(result.entries.is_empty());
    assert_eq!(github.reads, 0);
}

#[test]
fn findings_precede_red_required_ci_and_conflicts() {
    let mut input = pr(1);
    input.findings = true;
    input.required_ci = RequiredCi::Failed;
    input.mergeability = Mergeability::Conflicting;
    let result = board(vec![input], BTreeMap::new(), 0);
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].action, Action::FixFindings);
    assert!(result.entries[0].eligible);
}

#[test]
fn red_required_ci_precedes_conflicts() {
    let mut input = pr(1);
    input.required_ci = RequiredCi::Failed;
    input.mergeability = Mergeability::Conflicting;
    assert_eq!(
        board(vec![input], BTreeMap::new(), 0).entries[0].action,
        Action::FixRequiredCi
    );
}

#[test]
fn conflict_and_conservative_fallback() {
    let mut input = pr(1);
    input.mergeability = Mergeability::Conflicting;
    let result = board(vec![pr(2), input], BTreeMap::new(), 0);
    assert_eq!(result.entries[0].action, Action::ResolveConflict);
    assert_eq!(result.entries[1].action, Action::WaitReview);
    assert!(!result.entries[1].eligible);
}

#[test]
fn release_priority_lease_expiry_and_daily_attempt_cap() {
    let mut pinned = pr(9);
    pinned.release_pinned = true;
    pinned.findings = true;
    let mut other = pr(1);
    other.findings = true;
    let mut state = BTreeMap::from([(
        9,
        AttemptState {
            utc_day: 1,
            attempts: 2,
            lease: Some(Lease {
                holder: "worker-a".into(),
                expires_at: 86_410,
            }),
        },
    )]);
    let result = board(vec![other.clone(), pinned.clone()], state.clone(), 86_409);
    assert_eq!(result.entries[0].number, 9);
    assert!(!result.entries[0].eligible);
    assert!(result.entries[1].eligible);
    assert!(board(vec![pinned.clone()], state.clone(), 86_410).entries[0].eligible);
    state.get_mut(&9).unwrap().attempts = 3;
    assert!(!board(vec![pinned.clone()], state.clone(), 86_410).entries[0].eligible);
    assert!(board(vec![pinned], state, 172_800).entries[0].eligible);
}

#[test]
fn source_removal_needs_a_nonempty_removes_line_before_promotion() {
    let mut input = pr(1);
    input.removes_source = true;
    input.draft = true;
    for body in ["", "Removes:  ", "prose mentions Removes: old module"] {
        input.body = body.into();
        assert_eq!(
            board(vec![input.clone()], BTreeMap::new(), 0).entries[0].action,
            Action::AddRemoves
        );
    }
    input.body = "Summary\nRemoves: obsolete module — replaced by the board\n".into();
    assert_eq!(
        board(vec![input], BTreeMap::new(), 0).entries[0].action,
        Action::PromoteDraft
    );
}

#[test]
fn promotion_waiting_and_readiness_require_current_checks_and_mergeability() {
    let mut input = pr(1);
    for reviewed in [false, true] {
        input.reviewed = reviewed;
        for draft in [false, true] {
            input.draft = draft;
            input.required_ci = RequiredCi::Pending;
            let result = board(vec![input.clone()], BTreeMap::new(), 0);
            assert_eq!(result.entries[0].action, Action::WaitRequiredCi);
            assert!(!result.entries[0].eligible);
            input.required_ci = RequiredCi::Passed;
            input.mergeability = Mergeability::Unknown;
            assert_eq!(
                board(vec![input.clone()], BTreeMap::new(), 0).entries[0].action,
                Action::WaitMergeability
            );
            input.mergeability = Mergeability::Mergeable;
            let expected = if draft {
                Action::PromoteDraft
            } else if reviewed {
                Action::Ready
            } else {
                Action::WaitReview
            };
            assert_eq!(
                board(vec![input.clone()], BTreeMap::new(), 0).entries[0].action,
                expected
            );
        }
    }
}

#[test]
fn invalid_snapshots_fail_instead_of_dropping_work() {
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    for prs in [vec![pr(1), pr(1)], vec![pr(0)]] {
        let mut github = FakeGitHub { prs, reads: 0 };
        assert!(Board::read(&mut github, &policy, &BTreeMap::new(), 0).is_err());
    }
}

#[test]
fn github_errors_propagate_without_an_empty_successful_board() {
    struct Unavailable;
    impl GitHub for Unavailable {
        fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
            Err("synthetic snapshot failure".into())
        }
    }
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    assert!(Board::read(&mut Unavailable, &policy, &BTreeMap::new(), 0).is_err());
}

#[test]
fn action_priority_is_stable_within_release_priority() {
    let mut findings = pr(9);
    findings.findings = true;
    let result = board(vec![pr(1), findings], BTreeMap::new(), 0);
    assert_eq!(result.entries[0].number, 9);
    let json = serde_json::to_string(&result).unwrap();
    let restored: Board = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.entries[0].action, Action::FixFindings);
}

#[test]
fn pull_request_debug_redacts_body_while_serde_preserves_policy_input() {
    for body in [
        "synthetic private report",
        "Removes: synthetic private report\nsecond line",
    ] {
        let mut input = pr(1);
        input.body = body.into();
        for rendered in [
            format!("{input:?}"),
            format!("{input:#?}"),
            format!("{:?}", vec![input.clone()]),
        ] {
            assert!(!rendered.contains("synthetic private report"));
            assert!(rendered.contains("number: 1"));
        }
        let restored: PullRequest = serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
        assert_eq!(restored.body, body);
    }
}
