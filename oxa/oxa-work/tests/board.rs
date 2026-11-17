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
