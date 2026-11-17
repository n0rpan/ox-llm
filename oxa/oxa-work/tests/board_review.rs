use oxa_work::board::{review::*, RequiredCi};
use serde_json::{json, Value};

const PATCH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEXT_PATCH: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

fn facts<'a>(changes: &'a [Change<'static>]) -> Facts<'a> {
    Facts {
        changes,
        two_review: false,
        required_ci: RequiredCi::Passed,
        draft: false,
        review_admitted: true,
        unresolved_findings: false,
        body: "",
        head: "new-head",
        patch_id: Some(PATCH),
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
            request: vec![Reviewer::Codex],
            outstanding: vec![Reviewer::Codex],
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
fn docs_need_codex_and_agent_instructions_are_at_least_product() {
    for (path, previous_path, tier) in [
        ("docs/guide.md", None, Tier::DocsAndTests),
        ("AGENTS.md", None, Tier::Product),
        ("docs/tools/AGENTS.md", None, Tier::Product),
        ("skills/check/SKILL.md", None, Tier::Product),
        ("skills/check/checklist.md", None, Tier::Product),
        ("oxa/oxa-resources/prompts/default.md", None, Tier::Product),
        (
            "oxa/oxa-resources/builtin-skills/review/SKILL.md",
            None,
            Tier::Product,
        ),
        ("docs/old-guide.md", Some("docs/tools/AGENTS.md"), Tier::Product),
    ] {
        let changes = [Change {
            path,
            previous_path,
            kind: ChangeKind::Documentation,
            ..SOURCE
        }];
        let mut input = facts(&changes);
        assert_eq!(
            decide(&input),
            Decision::Review {
                tier,
                request: vec![Reviewer::Codex],
                outstanding: vec![Reviewer::Codex],
            },
            "{path}"
        );
        input.approvals = &[Approval {
            reviewer: Reviewer::Codex,
            head: "new-head",
            patch_id: PATCH,
        }];
        assert_eq!(
            decide(&input),
            Decision::Review {
                tier,
                request: vec![],
                outstanding: vec![]
            },
            "{path}"
        );
    }
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
        patch_id: PATCH,
    }];
    input.approvals = &[
        Approval {
            reviewer: Reviewer::Codex,
            head: "old-head",
            patch_id: PATCH,
        },
        Approval {
            reviewer: Reviewer::Security,
            head: "old-head",
            patch_id: NEXT_PATCH,
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
    input.patch_id = Some(NEXT_PATCH);
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

#[test]
fn board_holds_stale_evidence_and_uses_current_pull_request_facts() {
    use oxa_work::board::{Action, Board, GitHub, Mergeability, PullRequest};
    use std::collections::BTreeMap;

    struct Snapshot(PullRequest);
    impl GitHub for Snapshot {
        fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
            Ok(vec![self.0.clone()])
        }
    }

    let changes = [Change {
        deleted_lines: 61,
        ..SOURCE
    }];
    let mut evidence = facts(&changes);
    let mut github = Snapshot(PullRequest {
        number: 23,
        head: "new-head".into(),
        release_pinned: false,
        findings: false,
        required_ci: RequiredCi::Passed,
        mergeability: Mergeability::Mergeable,
        body: String::new(),
        removes_source: false,
        draft: false,
        reviewed: true,
    });
    let read = |github: &mut Snapshot, evidence| {
        let (board, plans) = Board::read_with_review_policy(
            github,
            &oxa_work::board::Policy {
                enabled: true,
                ..Default::default()
            },
            &BTreeMap::new(),
            0,
            &Policy {
                enabled: true,
                paths: PATHS,
            },
            &BTreeMap::from([(23, evidence)]),
        )
        .unwrap();
        (
            board.entries[0].action,
            plans.into_values().next().unwrap().decision,
        )
    };

    assert_eq!(
        read(&mut github, evidence),
        (Action::AddRemoves, Decision::AddRemoves)
    );
    github.0.body = "Removes: obsolete fallback".into();
    github.0.required_ci = RequiredCi::Pending;
    assert_eq!(
        read(&mut github, evidence),
        (Action::WaitRequiredCi, Decision::Wait)
    );
    github.0.required_ci = RequiredCi::Passed;
    evidence.head = "old-head";
    assert_eq!(read(&mut github, evidence), (Action::WaitReview, Decision::Wait));
    evidence.head = "new-head";
    github.0.draft = true;
    evidence.review_admitted = false;
    assert_eq!(read(&mut github, evidence), (Action::WaitReview, Decision::Wait));
    evidence.review_admitted = true;
    assert_eq!(
        read(&mut github, evidence),
        (Action::PromoteDraft, Decision::Wait)
    );
    github.0.draft = false;
    evidence.approvals = &[Approval {
        reviewer: Reviewer::Codex,
        head: "new-head",
        patch_id: PATCH,
    }];
    assert_eq!(
        read(&mut github, evidence),
        (
            Action::Ready,
            Decision::Review {
                tier: Tier::Product,
                request: vec![],
                outstanding: vec![],
            }
        )
    );
}

const CONNECTOR: u64 = 199_175_422;
const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn review(head: &str, unresolved: bool) -> Value {
    json!({"commit_sha":head, "status":"completed", "has_any_findings":unresolved, "has_unresolved_findings":unresolved})
}

fn absent() -> Value {
    json!({"commit_sha":null, "status":null, "has_any_findings":null, "has_unresolved_findings":null})
}

fn summary(code: Value, security: Value) -> String {
    format!(
        "<!-- codex-pull-request-review-summary -->\n<!-- codex-review-state:v1 {} -->\n",
        json!({"code_review":code, "security_review":security})
    )
}

fn connector(body: &str, updated_at: u64) -> Comment<'_> {
    Comment {
        bot_id: Some(CONNECTOR),
        updated_at: Some(updated_at),
        body,
    }
}

#[test]
fn latest_unresolved_summary_holds_even_when_it_reviews_an_older_head() {
    let clean = summary(review(NEW, false), absent());
    let unresolved = summary(review(OLD, true), absent());
    assert_eq!(
        latest_findings(Some(&[connector(&unresolved, 14), connector(&clean, 12)])),
        Findings::Unresolved
    );
    assert_eq!(
        latest_findings(Some(&[connector(&clean, 15), connector(&unresolved, 14)])),
        Findings::Clear
    );
    assert_eq!(
        latest_findings(Some(&[connector(&clean, 14), connector(&unresolved, 14)])),
        Findings::Unresolved
    );
}

#[test]
fn code_review_cannot_clear_unresolved_or_pending_security_evidence() {
    let clean_code = review(NEW, false);
    let text = summary(clean_code.clone(), review(OLD, true));
    assert_eq!(
        latest_findings(Some(&[connector(&text, 11)])),
        Findings::Unresolved
    );
    let mut pending_security = review(NEW, false);
    pending_security["status"] = "queued".into();
    let text = summary(clean_code.clone(), pending_security);
    assert_eq!(latest_findings(Some(&[connector(&text, 12)])), Findings::Unknown);
    let text = summary(clean_code, json!({}));
    assert_eq!(latest_findings(Some(&[connector(&text, 13)])), Findings::Unknown);
}

#[test]
fn untrusted_comments_and_incomplete_or_invalid_summaries_cannot_clear_a_hold() {
    let clean = summary(review(NEW, false), absent());
    let unresolved = summary(review(OLD, true), absent());
    let ordinary = Comment {
        bot_id: None,
        ..connector(&clean, 24)
    };
    let different_bot = Comment {
        bot_id: Some(17),
        ..connector(&clean, 25)
    };
    assert_eq!(
        latest_findings(Some(&[connector(&unresolved, 12), ordinary, different_bot])),
        Findings::Unresolved
    );
    assert_eq!(latest_findings(None), Findings::Unknown);
    let malformed = "<!-- codex-pull-request-review-summary -->\n<!-- codex-review-state:v1 {}";
    assert_eq!(
        latest_findings(Some(&[connector(&clean, 12), connector(malformed, 14)])),
        Findings::Unknown
    );
    assert_eq!(
        latest_findings(Some(&[Comment {
            updated_at: None,
            ..connector(&clean, 12)
        }])),
        Findings::Unknown
    );
}
