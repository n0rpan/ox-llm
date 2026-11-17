//! Hold evidence from GitHub's authenticated Codex summary. Approval is a separate board gate.
use serde_json::Value;

const CONNECTOR_BOT_ID: u64 = 199_175_422;
const SUMMARY: &str = "<!-- codex-pull-request-review-summary -->";
const STATE_PREFIX: &str = "<!-- codex-review-state:v1 ";
const STATE_SUFFIX: &str = " -->";
const MAX_STATE_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Findings {
    Unknown,
    Unresolved,
    Clear,
}

/// The Reader derives these fields from GitHub metadata, never comment text.
/// A missing or unverified update time stays unknown.
pub struct Comment<'a> {
    pub bot_id: Option<u64>,
    pub updated_at: Option<u64>,
    pub body: &'a str,
}

/// Pass a complete comment snapshot, or `None` if any page is unavailable.
/// A newer summary replaces an older one, even if it refers to another head.
/// Conflicting summaries with the same update time cannot authorize landing.
pub fn latest_findings(comments: Option<&[Comment<'_>]>) -> Findings {
    let Some(comments) = comments else {
        return Findings::Unknown;
    };
    let mut latest = None;
    for comment in comments {
        if comment.bot_id != Some(CONNECTOR_BOT_ID) {
            continue;
        }
        let mut lines = comment.body.lines();
        if lines.next() != Some(SUMMARY) {
            continue;
        }
        let Some(updated_at) = comment.updated_at else {
            return Findings::Unknown;
        };
        let state = state_line(lines.next());
        match latest {
            Some((time, _)) if time > updated_at => {}
            Some((time, previous)) if time == updated_at => {
                latest = Some((time, combine_tied(previous, state)));
            }
            _ => latest = Some((updated_at, state)),
        }
    }
    latest.map_or(Findings::Unknown, |(_, state)| state)
}

fn combine_tied(left: Findings, right: Findings) -> Findings {
    if left == Findings::Unresolved || right == Findings::Unresolved {
        Findings::Unresolved
    } else if left == right {
        left
    } else {
        Findings::Unknown
    }
}

fn state_line(line: Option<&str>) -> Findings {
    let Some(state) = line
        .and_then(|line| line.strip_prefix(STATE_PREFIX))
        .and_then(|line| line.strip_suffix(STATE_SUFFIX))
        .filter(|json| json.len() <= MAX_STATE_BYTES)
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
    else {
        return Findings::Unknown;
    };
    let code = review(&state["code_review"]);
    let security = &state["security_review"];
    let security_review = review(security);
    match (code, security_review) {
        (Findings::Unresolved, _) | (_, Findings::Unresolved) => Findings::Unresolved,
        (Findings::Clear, Findings::Clear) => Findings::Clear,
        (Findings::Clear, _) if no_review(security) => Findings::Clear,
        _ => Findings::Unknown,
    }
}

fn no_review(status: &Value) -> bool {
    [
        "commit_sha",
        "status",
        "has_any_findings",
        "has_unresolved_findings",
    ]
    .iter()
    .all(|key| status.get(key).is_some_and(Value::is_null))
}

fn review(value: &Value) -> Findings {
    let valid_head = value["commit_sha"].as_str().is_some_and(|sha| {
        matches!(sha.len(), 40 | 64)
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    if !valid_head {
        return Findings::Unknown;
    }
    match (
        value["has_unresolved_findings"].as_bool(),
        value["status"].as_str(),
    ) {
        (Some(true), _) => Findings::Unresolved,
        (Some(false), Some("completed")) => Findings::Clear,
        _ => Findings::Unknown,
    }
}
