use super::{nodes, Board, GitHub, Policy, PullRequest, Reader, RequiredCi};
use oxa_work::board::review::{
    self, Approval, Change, ChangeKind, Facts, GitDiff, GitHubRequestReceipt, PathRule, Reviewer, Tier,
};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

pub(super) struct Settings {
    requester: String,
    codex: String,
    security: String,
    owner: String,
    core: Vec<String>,
    protected: Vec<String>,
}

impl Settings {
    pub(super) fn read(path: &Path) -> Result<Self, String> {
        let mut input = Vec::new();
        File::open(path)
            .and_then(|file| {
                if !file.metadata()?.is_file() {
                    return Err(std::io::Error::other("expected regular file"));
                }
                file.take(16_385).read_to_end(&mut input)
            })
            .map_err(|_| "cannot read board review policy")?;
        if input.len() > 16_384 {
            return Err("board review policy exceeds the size limit".into());
        }
        let invalid = "invalid board review policy";
        let raw: Value = serde_json::from_slice(&input).map_err(|_| invalid)?;
        if raw.as_object().is_none_or(|fields| fields.len() != 6) {
            return Err(invalid.into());
        }
        let login = |field: &str| raw[field].as_str().map(str::to_owned).ok_or(invalid);
        let paths = |field: &str| {
            raw[field]
                .as_array()
                .ok_or(invalid)?
                .iter()
                .map(|path| path.as_str().map(str::to_owned).ok_or(invalid))
                .collect::<Result<Vec<_>, _>>()
        };
        let settings = Self {
            requester: login("requester")?,
            codex: login("codex")?,
            security: login("security")?,
            owner: login("owner")?,
            core: paths("core")?,
            protected: paths("protected")?,
        };
        let roles = [&settings.codex, &settings.security, &settings.owner];
        if settings.requester.is_empty()
            || roles.iter().any(|login| login.is_empty())
            || roles.iter().enumerate().any(|(index, login)| {
                roles[index + 1..]
                    .iter()
                    .any(|other| other.eq_ignore_ascii_case(login))
            })
            || settings.core.iter().chain(&settings.protected).any(|prefix| {
                prefix
                    .split('/')
                    .any(|component| matches!(component, "" | "." | ".."))
            })
        {
            return Err("invalid board review policy paths or reviewer accounts".into());
        }
        Ok(settings)
    }

    fn reviewer(&self, actor: &str) -> Option<Reviewer> {
        [
            (Reviewer::Codex, &self.codex),
            (Reviewer::Security, &self.security),
            (Reviewer::Owner, &self.owner),
        ]
        .into_iter()
        .find_map(|(reviewer, login)| login.eq_ignore_ascii_case(actor).then_some(reviewer))
    }
}

pub(super) fn source_path(path: &str) -> bool {
    [
        ".rs", ".py", ".go", ".ts", ".tsx", ".js", ".jsx", ".sh", ".c", ".cc", ".cpp", ".cxx", ".h", ".hh",
        ".hpp", ".hxx", ".inc", ".qml",
    ]
    .iter()
    .any(|extension| path.ends_with(extension))
}

fn kind(path: &str) -> ChangeKind {
    let name = path.rsplit('/').next().unwrap_or(path);
    if path
        .split('/')
        .any(|part| matches!(part, "test" | "tests" | "fixtures"))
        || name.ends_with("_test.go")
        || name.starts_with("test_") && name.ends_with(".py")
    {
        ChangeKind::Test
    } else if matches!(name, "README.md" | "CHANGELOG.md" | "CONTRIBUTING.md")
        || (path.split('/').any(|part| part == "docs") && (path.ends_with(".md") || path.ends_with(".rst")))
    {
        ChangeKind::Documentation
    } else if source_path(path) {
        ChangeKind::Source
    } else {
        ChangeKind::Other
    }
}

fn codex_clean_receipt(body: &str) -> Option<&str> {
    let mut lines = body.lines().filter(|line| !line.trim().is_empty());
    if lines.next()? != "<!-- codex-review-receipt:v1 -->"
        || !matches!(
            lines.next()?,
            "Codex Review: Didn't find any major issues."
                | "Codex Review: Didn't find any major issues. :rocket:"
        )
    {
        return None;
    }
    let sha = lines
        .next()?
        .strip_prefix("**Reviewed commit:** `")?
        .strip_suffix('`')?;
    (matches!(sha.len(), 10..=40 | 64)
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
    .then_some(sha)
}

struct Evidence<'a> {
    git: GitDiff,
    head: &'a str,
    two_review: bool,
    admitted: bool,
    approvals: Vec<(Reviewer, &'a str)>,
    receipts: Vec<GitHubRequestReceipt>,
}

impl<'a> Evidence<'a> {
    fn read(row: &'a Value, root: &Path, policy: &oxa_work::RunPolicy, settings: &Settings) -> Option<Self> {
        if row["baseRefName"] != "main" {
            return None;
        }
        let base = row["baseRefOid"].as_str()?;
        let head = row["headRefOid"].as_str()?;
        let git = GitDiff::read(policy, root, base, head).ok().flatten()?;
        let labels = nodes(&row["labels"]).ok()?;
        let labeled = |label| labels.iter().any(|item| item["name"].as_str() == Some(label));
        let admitted = !row["isCrossRepository"].as_bool()?
            && (labeled("ok-to-review")
                || matches!(
                    row["authorAssociation"].as_str(),
                    Some("OWNER" | "MEMBER" | "COLLABORATOR")
                ));
        let mut approvals: Vec<_> = nodes(&row["latestOpinionatedReviews"])
            .ok()?
            .iter()
            .filter(|opinion| opinion["state"] == "APPROVED")
            .filter_map(|opinion| {
                Some((
                    settings.reviewer(opinion["author"]["login"].as_str()?)?,
                    opinion["commit"]["oid"].as_str()?,
                ))
            })
            .collect();
        let comments = nodes(&row["comments"]).ok()?;
        let receipts = comments
            .iter()
            .filter(|comment| comment["lastEditedAt"].is_null())
            .filter_map(|comment| {
                GitHubRequestReceipt::from_comment(
                    &settings.requester,
                    comment["author"]["login"].as_str()?,
                    comment["body"].as_str()?,
                )
            })
            .collect();
        if let Ok(reviews) = nodes(&row["reviews"]) {
            approvals.extend(reviews.iter().filter_map(|review| {
                let actor = review["author"]["login"].as_str()?;
                let commit = review["commit"]["oid"].as_str()?;
                let receipt = codex_clean_receipt(review["body"].as_str()?)?;
                (review["state"] == "COMMENTED"
                    && review["lastEditedAt"].is_null()
                    && settings.reviewer(actor) == Some(Reviewer::Codex)
                    && commit.starts_with(receipt))
                .then_some((Reviewer::Codex, commit))
            }));
        }
        Some(Self {
            git,
            head,
            two_review: labeled("two-review"),
            admitted,
            approvals,
            receipts,
        })
    }
}

struct Snapshot(Vec<PullRequest>);

impl GitHub for Snapshot {
    fn open_pull_requests(&mut self) -> Result<Vec<PullRequest>, String> {
        Ok(std::mem::take(&mut self.0))
    }
}

pub(super) fn sample(
    reader: &mut Reader<'_>,
    root: &Path,
    run_policy: &oxa_work::RunPolicy,
    board_policy: &Policy,
    settings: &Settings,
    now: u64,
) -> Result<(Board, BTreeMap<u64, review::Plan>), String> {
    let pulls = reader.open_pull_requests()?;
    let observations = reader
        .review_rows
        .take()
        .ok_or("missing board review observations")?;
    let observations: Vec<_> = pulls
        .iter()
        .filter_map(|pr| {
            let row = &observations[format!("p{}", pr.number)];
            Some((pr.number, Evidence::read(row, root, run_policy, settings)?))
        })
        .collect();
    let inputs: Vec<_> = observations
        .iter()
        .map(|(number, evidence)| {
            let changes: Vec<_> = evidence
                .git
                .files
                .iter()
                .map(|file| {
                    let current = kind(&file.path);
                    let previous = file.previous_path.as_deref().map(kind).unwrap_or(current);
                    let kind = match (current, previous) {
                        (ChangeKind::Source, _) | (_, ChangeKind::Source) => ChangeKind::Source,
                        (ChangeKind::Other, _) | (_, ChangeKind::Other) => ChangeKind::Other,
                        _ => current,
                    };
                    Change {
                        path: &file.path,
                        previous_path: file.previous_path.as_deref(),
                        kind,
                        deleted_file: file.deleted_file,
                        deleted_lines: file.deleted_lines,
                    }
                })
                .collect();
            let predecessors: Vec<_> = evidence
                .git
                .main_merge_predecessors
                .iter()
                .map(String::as_str)
                .collect();
            let requests: Vec<_> = evidence
                .receipts
                .iter()
                .map(GitHubRequestReceipt::receipt)
                .collect();
            let approvals: Vec<_> = evidence
                .approvals
                .iter()
                .map(|&(reviewer, head)| Approval {
                    reviewer,
                    head,
                    patch_id: &evidence.git.patch_id,
                })
                .collect();
            (*number, evidence, changes, predecessors, requests, approvals)
        })
        .collect();
    let facts = inputs
        .iter()
        .map(
            |(number, evidence, changes, predecessors, requested, approvals)| {
                (
                    *number,
                    Facts {
                        changes,
                        two_review: evidence.two_review,
                        required_ci: RequiredCi::Pending,
                        draft: false,
                        review_admitted: evidence.admitted,
                        unresolved_findings: false,
                        body: "",
                        head: evidence.head,
                        patch_id: Some(&evidence.git.patch_id),
                        main_merge_predecessors: predecessors,
                        requested,
                        approvals,
                    },
                )
            },
        )
        .collect();
    let paths: Vec<_> = settings
        .core
        .iter()
        .map(|path| PathRule {
            path,
            tier: Tier::Core,
        })
        .chain(settings.protected.iter().map(|path| PathRule {
            path,
            tier: Tier::Protected,
        }))
        .collect();
    Board::read_with_review_policy(
        &mut Snapshot(pulls),
        board_policy,
        &BTreeMap::new(),
        now,
        &review::Policy {
            enabled: true,
            paths: &paths,
        },
        &facts,
    )
}
