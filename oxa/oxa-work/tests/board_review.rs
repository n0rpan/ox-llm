use oxa_work::board::{review::*, RequiredCi};

const PATHS: &[PathRule<'_>] = &[
    PathRule {
        path: "engine",
        tier: Tier::Core,
    },
    PathRule {
        path: "engine/src/sandbox.rs",
        tier: Tier::Protected,
    },
];

const SOURCE: Change<'static> = Change {
    path: "oxa/src/main.rs",
    previous_path: None,
    kind: ChangeKind::Source,
    deleted_lines: 0,
    deleted_file: false,
};

fn facts(changes: &[Change<'static>]) -> Facts<'_> {
    Facts {
        changes,
        two_review: false,
        required_ci: RequiredCi::Passed,
        draft: false,
        review_admitted: true,
        unresolved_findings: false,
        body: "",
        head: "new-head",
        patch_id: Some("own-patch"),
        main_merge_predecessors: &[],
        requested: &[],
        approvals: &[],
    }
}

fn decide(facts: &Facts<'_>) -> Decision {
    Policy {
        enabled: true,
        paths: PATHS,
    }
    .decide(facts)
}

#[test]
fn requires_opt_in_green_required_ci_and_review_admission() {
    let mut input = facts(&[SOURCE]);
    assert_eq!(Policy::default().decide(&input), Decision::Disabled);
    for ci in [RequiredCi::Pending, RequiredCi::Failed] {
        input.required_ci = ci;
        assert_eq!(decide(&input), Decision::Wait);
    }
    input.required_ci = RequiredCi::Passed;
    input.review_admitted = false;
    assert_eq!(decide(&input), Decision::Wait);
    input.review_admitted = true;
    input.patch_id = None;
    assert_eq!(decide(&input), Decision::Wait);
}

#[test]
fn path_tiers_ignore_routine_tests_and_include_both_paths_of_renames() {
    let tests = [Change {
        path: "engine/tests/behavior.rs",
        kind: ChangeKind::Test,
        ..SOURCE
    }];
    let mut input = facts(&tests);
    assert_eq!(
        decide(&input),
        Decision::Review {
            tier: Tier::DocsAndTests,
            request: vec![],
            outstanding: vec![],
        }
    );
    input.two_review = true;
    assert_eq!(
        decide(&input),
        Decision::Review {
            tier: Tier::Core,
            request: vec![Reviewer::Codex, Reviewer::Security],
            outstanding: vec![Reviewer::Codex, Reviewer::Security],
        }
    );
    let renamed = [Change {
        path: "engine-copy/src/sandbox.rs",
        previous_path: Some("engine/src/sandbox.rs"),
        ..SOURCE
    }];
    assert_eq!(
        decide(&facts(&renamed)),
        Decision::Review {
            tier: Tier::Protected,
            request: vec![Reviewer::Codex, Reviewer::Security],
            outstanding: vec![Reviewer::Codex, Reviewer::Security, Reviewer::Owner],
        }
    );
}

#[test]
fn request_receipts_deduplicate_and_only_pure_merges_carry_approvals() {
    let changes = [Change {
        path: "engine/src/sandbox.rs",
        ..SOURCE
    }];
    let mut input = facts(&changes);
    input.requested = &[RequestReceipt {
        reviewer: Reviewer::Codex,
        patch_id: "own-patch",
    }];
    input.approvals = &[
        Approval {
            reviewer: Reviewer::Codex,
            head: "old-head",
            patch_id: "own-patch",
        },
        Approval {
            reviewer: Reviewer::Security,
            head: "old-head",
            patch_id: "different-patch",
        },
    ];
    input.main_merge_predecessors = &["old-head"];
    assert_eq!(
        decide(&input),
        Decision::Review {
            tier: Tier::Protected,
            request: vec![Reviewer::Security],
            outstanding: vec![Reviewer::Security, Reviewer::Owner],
        }
    );
    input.main_merge_predecessors = &[];
    assert_eq!(
        decide(&input),
        Decision::Review {
            tier: Tier::Protected,
            request: vec![Reviewer::Security],
            outstanding: vec![Reviewer::Codex, Reviewer::Security, Reviewer::Owner],
        }
    );
    input.patch_id = Some("next-own-patch");
    assert!(matches!(
        decide(&input),
        Decision::Review { request, .. } if request == [Reviewer::Codex, Reviewer::Security]
    ));
}

#[test]
fn removals_count_source_lines_in_aggregate_and_any_deleted_file() {
    let changes = [
        Change {
            deleted_lines: 30,
            ..SOURCE
        },
        Change {
            deleted_lines: 31,
            ..SOURCE
        },
    ];
    let mut input = facts(&changes);
    assert_eq!(decide(&input), Decision::AddRemoves);
    input.body = "Removes:   ";
    assert_eq!(decide(&input), Decision::AddRemoves);
    input.body = "Removes: stale local fallback";
    assert!(matches!(decide(&input), Decision::Review { .. }));
    input.unresolved_findings = true;
    assert_eq!(decide(&input), Decision::ResolveFindings);

    let exactly_sixty = [Change {
        deleted_lines: 60,
        ..SOURCE
    }];
    assert!(matches!(decide(&facts(&exactly_sixty)), Decision::Review { .. }));
    let deleted_doc = [Change {
        kind: ChangeKind::Documentation,
        deleted_file: true,
        ..SOURCE
    }];
    assert_eq!(decide(&facts(&deleted_doc)), Decision::AddRemoves);
}
