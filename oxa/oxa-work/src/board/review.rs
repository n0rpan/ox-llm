//! Read-only decisions for the #307 auto-approve row. The runner supplies complete
//! diff facts and authenticated receipts. This module cannot post or approve reviews.

use super::RequiredCi;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod git;
pub use git::{GitDiff, GitFileChange};
mod receipts;
pub use receipts::GitHubRequestReceipt;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    DocsAndTests,
    Product,
    Core,
    Protected,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reviewer {
    Codex,
    Security,
    Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Documentation,
    Test,
    Source,
    Other,
}

pub struct Change<'a> {
    pub path: &'a str,
    pub previous_path: Option<&'a str>,
    /// For a rename, Source if either side is source and Other if either side
    /// is unclassified. Filter as documentation or test only if both sides qualify.
    pub kind: ChangeKind,
    pub deleted_lines: u64,
    pub deleted_file: bool,
}

pub struct RequestReceipt<'a> {
    pub reviewer: Reviewer,
    pub patch_id: &'a str,
}

pub struct Approval<'a> {
    pub reviewer: Reviewer,
    pub head: &'a str,
    pub patch_id: &'a str,
}

#[derive(Clone, Copy)]
pub struct Facts<'a> {
    pub changes: &'a [Change<'a>],
    pub two_review: bool,
    pub required_ci: RequiredCi,
    pub draft: bool,
    /// Known same-repo author or an explicit maintainer admission.
    pub review_admitted: bool,
    pub unresolved_findings: bool,
    pub body: &'a str,
    pub head: &'a str,
    /// Stable patch ID of the complete own diff. Missing evidence fails closed.
    pub patch_id: Option<&'a str>,
    /// Reviewed heads linked to this head by verified pure main merges.
    pub main_merge_predecessors: &'a [&'a str],
    /// Owner-approved heads with only verified pure main merges and ordinary
    /// documentation and test changes since approval.
    pub owner_approval_predecessors: &'a [&'a str],
    pub requested: &'a [RequestReceipt<'a>],
    pub approvals: &'a [Approval<'a>],
    /// The connector refused the review requester after the newest trusted request
    /// for this patch and has not reviewed since ([`requester_rejected`]).
    pub requester_rejected: bool,
}

impl<'a> Facts<'a> {
    pub(super) fn verified_patch_id(&self) -> Option<&'a str> {
        self.patch_id.filter(|id| git::object_id(id))
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Disabled,
    ResolveFindings,
    AddRemoves,
    Wait,
    /// The reviewer account refused the configured requester: nothing is re-sent under
    /// that identity, and the owner must connect an account or change the requester.
    RequesterRejected {
        reviewer: Reviewer,
    },
    Review {
        tier: Tier,
        request: Vec<Reviewer>,
        outstanding: Vec<Reviewer>,
    },
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub decision: Decision,
    requests: Vec<Request>,
}

/// Only the board can produce requests. A dispatcher must revalidate the head
/// and required checks and reserve the request before causing external effects.
#[derive(Debug, Serialize)]
pub struct Request {
    pub(super) number: u64,
    pub(super) head: String,
    pub(super) key: RequestKey,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestKey {
    patch_id: String,
    reviewer: Reviewer,
}

impl RequestKey {
    pub(super) fn valid(&self) -> bool {
        self.reviewer != Reviewer::Owner && git::object_id(&self.patch_id)
    }
}

impl Request {
    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    pub fn reviewer(&self) -> Reviewer {
        self.key.reviewer
    }

    pub fn patch_id(&self) -> &str {
        &self.key.patch_id
    }
}

/// A runner with authenticated reviewer access. Both operations are scoped to
/// the immutable head and own diff on the request. The preflight must confirm
/// admission and required CI and honor authenticated prior request receipts.
pub trait RequestRemote {
    fn current_and_admitted(&mut self, request: &Request) -> crate::Result<bool>;
    /// One submission attempt, with no automatic retry after an uncertain response.
    fn send_once(&mut self, request: &Request) -> crate::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    Held,
    AlreadyReserved,
    Sent,
}

impl Plan {
    pub(super) fn new(number: u64, head: &str, patch_id: Option<&str>, decision: Decision) -> Self {
        let requests = match (&decision, patch_id) {
            (Decision::Review { request, .. }, Some(patch_id)) => request
                .iter()
                .map(|&reviewer| Request {
                    number,
                    head: head.to_owned(),
                    key: RequestKey {
                        patch_id: patch_id.to_owned(),
                        reviewer,
                    },
                })
                .collect(),
            _ => vec![],
        };
        Self { decision, requests }
    }

    pub fn requests(&self) -> &[Request] {
        &self.requests
    }
}

/// Match an exact repo-relative path or a whole directory prefix.
pub struct PathRule<'a> {
    pub path: &'a str,
    pub tier: Tier,
}

impl PathRule<'_> {
    fn matches(&self, path: &str) -> bool {
        path == self.path
            || path
                .strip_prefix(self.path)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// Construct this policy from owner-controlled rules, never from the PR.
#[derive(Default)]
pub struct Policy<'a> {
    pub enabled: bool,
    pub paths: &'a [PathRule<'a>],
}

impl Policy<'_> {
    pub fn allows_approval_carry(&self, path: &str, kind: ChangeKind) -> bool {
        matches!(kind, ChangeKind::Documentation | ChangeKind::Test)
            && !agent_instruction_path(path)
            && !self
                .paths
                .iter()
                .any(|rule| rule.tier == Tier::Protected && rule.matches(path))
    }

    pub fn decide(&self, facts: &Facts<'_>) -> Decision {
        if !self.enabled {
            return Decision::Disabled;
        }
        if facts.unresolved_findings {
            return Decision::ResolveFindings;
        }
        let source_deletions = facts
            .changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Source)
            .fold(0_u64, |total, change| total.saturating_add(change.deleted_lines));
        if (source_deletions > 60 || facts.changes.iter().any(|change| change.deleted_file))
            && !has_removes_note(facts.body)
        {
            return Decision::AddRemoves;
        }
        if facts.requester_rejected {
            return Decision::RequesterRejected {
                reviewer: Reviewer::Codex,
            };
        }
        let Some(patch_id) = facts.verified_patch_id() else {
            return Decision::Wait;
        };
        if facts.required_ci != RequiredCi::Passed
            || facts.draft
            || !facts.review_admitted
            || facts.head.is_empty()
            || facts.changes.is_empty()
        {
            return Decision::Wait;
        }
        let tier = facts
            .changes
            .iter()
            .map(|change| {
                let paths = [Some(change.path), change.previous_path];
                let path_tier = self
                    .paths
                    .iter()
                    .filter(|rule| paths.into_iter().flatten().any(|path| rule.matches(path)))
                    .map(|rule| rule.tier)
                    .max()
                    .unwrap_or(Tier::DocsAndTests);
                match change.kind {
                    _ if path_tier == Tier::Protected => Tier::Protected,
                    ChangeKind::Documentation | ChangeKind::Test
                        if !paths.into_iter().flatten().any(agent_instruction_path) =>
                    {
                        Tier::DocsAndTests
                    }
                    _ => path_tier.max(Tier::Product),
                }
            })
            .max()
            .unwrap_or(Tier::DocsAndTests);
        let tier = if facts.two_review {
            tier.max(Tier::Core)
        } else {
            tier
        };
        let required: &[Reviewer] = match tier {
            Tier::DocsAndTests | Tier::Product => &[Reviewer::Codex],
            Tier::Core => &[Reviewer::Codex, Reviewer::Security],
            Tier::Protected => &[Reviewer::Codex, Reviewer::Security, Reviewer::Owner],
        };
        let outstanding: Vec<_> = required
            .iter()
            .copied()
            .filter(|reviewer| {
                !facts.approvals.iter().any(|approval| {
                    approval.reviewer == *reviewer
                        && approval.patch_id == patch_id
                        && (approval.head == facts.head
                            || facts.main_merge_predecessors.contains(&approval.head)
                            || (*reviewer == Reviewer::Owner
                                && facts.owner_approval_predecessors.contains(&approval.head)))
                })
            })
            .collect();
        let request = outstanding
            .iter()
            .copied()
            .filter(|reviewer| {
                *reviewer != Reviewer::Owner
                    && !facts
                        .requested
                        .iter()
                        .any(|receipt| receipt.reviewer == *reviewer && receipt.patch_id == patch_id)
            })
            .collect();
        Decision::Review {
            tier,
            request,
            outstanding,
        }
    }
}

fn agent_instruction_path(path: &str) -> bool {
    matches!(path.rsplit('/').next(), Some("AGENTS.md" | "SKILL.md"))
        || path.split('/').any(|component| component == "skills")
        || path.starts_with("oxa/oxa-resources/")
}

pub(super) fn has_removes_note(body: &str) -> bool {
    body.lines().any(|line| {
        line.trim()
            .strip_prefix("Removes:")
            .is_some_and(|note| !note.trim().is_empty())
    })
}

const CONNECTOR_BOT_ID: u64 = 199_175_422;
const SUMMARY: &str = "<!-- codex-pull-request-review-summary -->";
const STATE_PREFIX: &str = "<!-- codex-review-state:v1 ";
const STATE_SUFFIX: &str = " -->";
const MAX_STATE_BYTES: usize = 2048;

/// The connector's refusal of an account it is not connected to, matched on a few stable
/// lowercase phrases kept here. An unknown connector reply is not a refusal: it stays a wait.
const REFUSAL_PHRASES: &[&str] = &[
    "not connected",
    "connect your",
    "link your github",
    "create a codex account",
];

/// Whether the connector account answered the newest trusted review request of this patch
/// with its account-not-connected refusal and has posted no review summary since. Pass the
/// connector account's own comments and the request time; either missing is `false`.
pub fn requester_rejected(comments: Option<&[Comment<'_>]>, requested_at: Option<u64>) -> bool {
    let (Some(comments), Some(since)) = (comments, requested_at) else {
        return false;
    };
    let (mut refused, mut summary) = (None, None);
    for comment in comments.iter().filter(|c| c.bot_id == Some(CONNECTOR_BOT_ID)) {
        let Some(at) = comment.updated_at.filter(|at| *at >= since) else {
            continue;
        };
        let slot = if comment.body.lines().next() == Some(SUMMARY) {
            &mut summary
        } else if REFUSAL_PHRASES
            .iter()
            .any(|phrase| comment.body.to_ascii_lowercase().contains(phrase))
        {
            &mut refused
        } else {
            continue;
        };
        *slot = (*slot).max(Some(at));
    }
    refused.is_some_and(|refused| summary.is_none_or(|summary| summary <= refused))
}

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
    latest_summary(comments, None).map_or(Findings::Unknown, |summary| summary.findings)
}

/// Landing requires completed Code review on this head for clearance. Older
/// unresolved findings still hold landing, but cannot authorize an action.
pub fn latest_findings_for_head(comments: Option<&[Comment<'_>]>, head: &str) -> (Findings, bool) {
    latest_summary(comments, Some(head)).map_or((Findings::Unknown, false), |summary| {
        let findings = match summary.findings {
            Findings::Clear if !summary.exact_code_completed => Findings::Unknown,
            findings => findings,
        };
        (findings, summary.exact_hold)
    })
}

/// The full commit reviewed by Codex when the latest trusted summary has no
/// code findings. The caller must still match it against the PR's own diff.
pub fn latest_code_approval(comments: Option<&[Comment<'_>]>) -> Option<String> {
    latest_summary(comments, None)?.code_approval
}

struct Summary {
    findings: Findings,
    code_approval: Option<String>,
    exact_code_completed: bool,
    exact_hold: bool,
}

fn latest_summary(comments: Option<&[Comment<'_>]>, head: Option<&str>) -> Option<Summary> {
    let mut latest = None;
    for comment in comments? {
        if comment.bot_id != Some(CONNECTOR_BOT_ID) {
            continue;
        }
        let mut lines = comment.body.lines();
        if lines.next() != Some(SUMMARY) {
            continue;
        }
        let updated_at = comment.updated_at?;
        let state = state_line(lines.next(), head).unwrap_or(Summary {
            findings: Findings::Unknown,
            code_approval: None,
            exact_code_completed: false,
            exact_hold: false,
        });
        match latest {
            Some((time, _)) if time > updated_at => {}
            Some((time, previous)) if time == updated_at => {
                latest = Some((time, combine_tied(previous, state)));
            }
            _ => latest = Some((updated_at, state)),
        }
    }
    latest.map(|(_, state)| state)
}

fn combine_tied(left: Summary, right: Summary) -> Summary {
    let findings = if left.findings == Findings::Unresolved || right.findings == Findings::Unresolved {
        Findings::Unresolved
    } else if left.findings == right.findings {
        left.findings
    } else {
        Findings::Unknown
    };
    Summary {
        findings,
        exact_code_completed: left.exact_code_completed && right.exact_code_completed,
        exact_hold: left.exact_hold && right.exact_hold,
        code_approval: left
            .code_approval
            .filter(|head| left.findings == right.findings && right.code_approval.as_ref() == Some(head)),
    }
}

fn state_line(line: Option<&str>, head: Option<&str>) -> Option<Summary> {
    let state = line?
        .strip_prefix(STATE_PREFIX)
        .and_then(|line| line.strip_suffix(STATE_SUFFIX))
        .filter(|json| json.len() <= MAX_STATE_BYTES)
        .and_then(|json| serde_json::from_str::<Value>(json).ok())?;
    let code = &state["code_review"];
    let code_review = review(code);
    let security = &state["security_review"];
    let findings = match (code_review, review(security)) {
        (Findings::Unresolved, _) | (_, Findings::Unresolved) => Findings::Unresolved,
        (Findings::Clear, Findings::Clear) => Findings::Clear,
        (Findings::Clear, _) if no_review(security) => Findings::Clear,
        _ => Findings::Unknown,
    };
    let code_approval = code["commit_sha"]
        .as_str()
        .filter(|_| {
            code_review == Findings::Clear
                && code["has_any_findings"] == false
                && findings != Findings::Unresolved
        })
        .map(str::to_owned);
    let completed_on_head = |review: &Value| {
        head.is_some_and(|head| {
            git::object_id(head)
                && review["commit_sha"].as_str() == Some(head)
                && review["status"] == "completed"
        })
    };
    let exact_hold = [code, security]
        .into_iter()
        .any(|review| completed_on_head(review) && review["has_unresolved_findings"] == true);
    Some(Summary {
        findings,
        code_approval,
        exact_code_completed: completed_on_head(code),
        exact_hold,
    })
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
    let valid_head = value["commit_sha"].as_str().is_some_and(git::object_id);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(bot: u64, at: u64, body: &str) -> Comment<'_> {
        Comment {
            bot_id: Some(bot),
            updated_at: Some(at),
            body,
        }
    }

    #[test]
    fn a_refusal_after_the_request_is_a_rejection_until_a_later_summary() {
        let refusal = "To use Codex here, connect your ChatGPT account to GitHub.";
        let summary = format!("{SUMMARY}\nreview");
        let rejected = |comments: &[Comment<'_>], at| requester_rejected(Some(comments), at);
        assert!(rejected(&[reply(CONNECTOR_BOT_ID, 20, refusal)], Some(10)));
        // Before the request, from another account, or in an unknown wording: not a rejection.
        assert!(!rejected(&[reply(CONNECTOR_BOT_ID, 5, refusal)], Some(10)));
        assert!(!rejected(&[reply(1, 20, refusal)], Some(10)));
        assert!(!rejected(
            &[reply(CONNECTOR_BOT_ID, 20, "Working on it")],
            Some(10)
        ));
        assert!(!rejected(&[reply(CONNECTOR_BOT_ID, 20, refusal)], None));
        assert!(!requester_rejected(None, Some(10)));
        // A review summary after the refusal means the reviewer did answer.
        let later = [
            reply(CONNECTOR_BOT_ID, 20, refusal),
            reply(CONNECTOR_BOT_ID, 30, &summary),
        ];
        assert!(!rejected(&later, Some(10)));
    }
}
