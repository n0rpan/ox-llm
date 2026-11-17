use oxa_work::board::review::{latest_findings, Comment, Findings};
use serde_json::{json, Value};

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
