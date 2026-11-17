//! Read-only decisions for the #307 auto-approve row. The runner supplies complete
//! diff facts and authenticated receipts. This module cannot post or approve reviews.

use super::RequiredCi;

mod git;
pub use git::GitDiff;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    DocsAndTests,
    Product,
    Core,
    Protected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Debug, PartialEq, Eq)]
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
        let Some(patch_id) = facts.patch_id.filter(|id| !id.is_empty()) else {
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
                let path_tier = self
                    .paths
                    .iter()
                    .filter(|rule| {
                        [Some(change.path), change.previous_path]
                            .into_iter()
                            .flatten()
                            .any(|path| {
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
                    ChangeKind::Documentation | ChangeKind::Test => Tier::DocsAndTests,
                    ChangeKind::Source | ChangeKind::Other => path_tier.max(Tier::Product),
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
            Tier::DocsAndTests => &[],
            Tier::Product => &[Reviewer::Codex],
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

pub(super) fn has_removes_note(body: &str) -> bool {
    body.lines().any(|line| {
        line.trim()
            .strip_prefix("Removes:")
            .is_some_and(|note| !note.trim().is_empty())
    })
}
