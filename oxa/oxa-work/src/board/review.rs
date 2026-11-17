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
    /// Heads proven to reach this head through pure main merges only.
    pub main_merge_predecessors: &'a [&'a str],
    pub requested: &'a [RequestReceipt<'a>],
    pub approvals: &'a [Approval<'a>],
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

/// Construct this policy from owner-controlled rules, never from the PR.
#[derive(Default)]
pub struct Policy<'a> {
    pub enabled: bool,
    pub paths: &'a [PathRule<'a>],
}

impl Policy<'_> {
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
                    .filter(|rule| {
                        paths.into_iter().flatten().any(|path| {
                            path == rule.path
                                || path
                                    .strip_prefix(rule.path)
                                    .is_some_and(|rest| rest.starts_with('/'))
                        })
                    })
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
                            || facts.main_merge_predecessors.contains(&approval.head))
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
    latest_summary(comments).map_or(Findings::Unknown, |summary| summary.findings)
}

/// The full commit reviewed by Codex when the latest trusted summary has no
/// code findings. The caller must still match it against the PR's own diff.
pub fn latest_code_approval(comments: Option<&[Comment<'_>]>) -> Option<String> {
    latest_summary(comments)?.code_approval
}

struct Summary {
    findings: Findings,
    code_approval: Option<String>,
}

fn latest_summary(comments: Option<&[Comment<'_>]>) -> Option<Summary> {
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
        let state = state_line(lines.next()).unwrap_or(Summary {
            findings: Findings::Unknown,
            code_approval: None,
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
        code_approval: left
            .code_approval
            .filter(|head| left.findings == right.findings && right.code_approval.as_ref() == Some(head)),
    }
}

fn state_line(line: Option<&str>) -> Option<Summary> {
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
    Some(Summary {
        findings,
        code_approval,
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
