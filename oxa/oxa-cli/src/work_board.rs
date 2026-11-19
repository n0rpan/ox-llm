//! Explicitly opted-in, read-only GitHub sampling. Never invokes a worker or mutation.
use crate::lead_quota::{GitHubIdentity, GitHubQuotas, GitHubResource};
pub mod refill_input;
use oxa_work::board::{Board, GitHub, Incomplete, Mergeability, Policy, PullRequest, RequiredCi, Snapshot};
use serde_json::Value;
mod claims_readback;
mod refill;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, IsTerminal, Read},
    path::Path,
    process::Command,
    time::{Duration, Instant, SystemTime},
};

mod landing;
mod review;

fn token_line(input: &mut dyn BufRead) -> Result<String, String> {
    let mut line = Vec::new();
    input
        .take(4097)
        .read_until(b'\n', &mut line)
        .map_err(|_| "cannot read App token input")?;
    if line.len() > 4096 || line.last() != Some(&b'\n') {
        return Err("missing or oversized App token input".into());
    }
    let token = std::str::from_utf8(&line)
        .map_err(|_| "invalid App token input")?
        .trim_end_matches(['\r', '\n']);
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("invalid App token input".into());
    }
    Ok(token.to_owned())
}

pub(crate) struct Reader<'a> {
    gh: &'a Path,
    path: &'a std::ffi::OsStr,
    repo: &'a str,
    token: &'a str,
    review_rows: Option<Value>,
    deadline: Cell<Instant>,
    bytes: Cell<usize>,
    app_quota: Option<RefCell<GitHubQuotas>>,
}

const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const LIST: &str = "pullRequests(first:100,states:OPEN){totalCount pageInfo{hasNextPage endCursor} nodes{number headRefOid}}";

fn charge(bytes: &mut usize, read: usize) -> Result<(), String> {
    *bytes = bytes
        .checked_add(read)
        .filter(|total| *total <= MAX_RESPONSE)
        .ok_or("GitHub snapshot exceeds byte limit")?;
    Ok(())
}

enum QueryError {
    Http(u16),
    Other(&'static str),
}

enum SnapshotError {
    Changed(&'static str),
    Terminal(String),
}
impl From<String> for SnapshotError {
    fn from(message: String) -> Self {
        Self::Terminal(message)
    }
}
impl From<&str> for SnapshotError {
    fn from(message: &str) -> Self {
        Self::Terminal(message.into())
    }
}
impl SnapshotError {
    fn message(self) -> String {
        match self {
            Self::Changed(message) => message.into(),
            Self::Terminal(message) => message,
        }
    }
}

fn identity(row: &Value) -> Result<(u64, &str), String> {
    let number = row["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or("invalid PR number")?;
    let head = row["headRefOid"]
        .as_str()
        .filter(|head| !head.is_empty())
        .ok_or("missing PR head")?;
    Ok((number, head))
}

/// Validate embedded identity pages before classifying differences as changing membership.
fn listing_page(connection: &Value) -> Result<(&Vec<Value>, u64, bool), String> {
    let total = connection["totalCount"].as_u64().ok_or("missing PR count")?;
    let rows = connection["nodes"].as_array().ok_or("missing PR page")?;
    if total > oxa_work::board::MAX_OPEN_PULL_REQUESTS as u64 || rows.len() > 100 || rows.len() as u64 > total
    {
        return Err("GitHub listing exceeds limit".into());
    }
    let more = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .ok_or("missing PR pagination")?;
    if more
        && (rows.is_empty()
            || connection["pageInfo"]["endCursor"]
                .as_str()
                .is_none_or(str::is_empty))
    {
        return Err("invalid PR pagination progress".into());
    }
    let mut seen = BTreeSet::new();
    for row in rows {
        if !seen.insert(identity(row)?.0) {
            return Err("duplicate PR in listing".into());
        }
    }
    Ok((rows, total, more))
}
impl QueryError {
    fn retryable(&self) -> bool {
        matches!(self, Self::Http(502..=504))
    }
    fn message(self) -> String {
        match self {
            Self::Http(status) => format!("GitHub read failed (HTTP {status})"),
            Self::Other(message) => message.into(),
        }
    }
}

impl<'a> Reader<'a> {
    /// Callers must enforce trust/offline policy and supply an explicit App credential.
    pub(crate) fn new(gh: &'a Path, path: &'a std::ffi::OsStr, repo: &'a str, token: &'a str) -> Self {
        Self {
            gh,
            path,
            repo,
            token,
            review_rows: None,
            deadline: Cell::new(Instant::now() + Duration::from_secs(120)),
            bytes: Cell::new(0),
            app_quota: None,
        }
    }

    fn time_left(&self) -> Result<Duration, QueryError> {
        let remaining = self.deadline.get().saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(QueryError::Other("GitHub snapshot deadline exhausted"));
        }
        Ok(remaining)
    }

    /// Fixed repository fields from native callers share the bounded, redacted transport.
    /// The byte count lets callers enforce an aggregate snapshot budget across queries.
    pub(crate) fn repository_read(&self, fields: &str) -> Result<(Value, usize), String> {
        self.query(fields).map_err(QueryError::message)
    }

    fn listing(&self) -> Result<(Value, Value), SnapshotError> {
        let (mut fields, mut rows, mut seen, mut cursors) =
            (LIST.to_owned(), vec![], BTreeSet::new(), BTreeSet::new());
        let mut result = Value::Null;
        loop {
            let (page, _) = self.repository_read(&format!("{fields} mergeQueue(branch:\"main\"){{id}}"))?;
            if result.is_null() {
                result = page.clone();
            }
            let connection = &page["pullRequests"];
            let (nodes, total, more) = listing_page(connection)?;
            if connection["totalCount"] != result["pullRequests"]["totalCount"]
                || page["mergeQueue"] != result["mergeQueue"]
            {
                return Err(SnapshotError::Changed(
                    "GitHub listing changed during snapshot; retry later",
                ));
            }
            for row in nodes {
                let (number, _) = identity(row)?;
                if !seen.insert(number) {
                    return Err("duplicate PR or missing head in listing".into());
                }
                rows.push(row.clone());
            }
            if !more {
                if rows.len() as u64 != total {
                    return Err("incomplete PR listing".into());
                }
                let first = result["pullRequests"].clone();
                result["pullRequests"]["nodes"] = rows.into();
                return Ok((result, first));
            }
            let cursor = connection["pageInfo"]["endCursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .ok_or("missing PR cursor")?;
            if nodes.is_empty() || rows.len() as u64 >= total || !cursors.insert(cursor.to_owned()) {
                return Err("invalid PR pagination progress".into());
            }
            fields = LIST.replace("states:OPEN", &format!("states:OPEN,after:{cursor:?}"));
        }
    }

    fn query(&self, fields: &str) -> Result<(Value, usize), QueryError> {
        let mut retries = 0;
        loop {
            match self.query_once(fields) {
                Err(error) if error.retryable() && retries < 2 => {
                    let pause = Duration::from_millis(200 << retries);
                    if self.time_left()? <= pause {
                        return Err(QueryError::Other("GitHub snapshot deadline exhausted"));
                    }
                    std::thread::sleep(pause);
                    retries += 1;
                }
                result => return result,
            }
        }
    }

    fn query_once(&self, fields: &str) -> Result<(Value, usize), QueryError> {
        let invalid = QueryError::Other;
        let (owner, name) = self.repo.split_once('/').ok_or(invalid("expected OWNER/REPO"))?;
        let query = format!("query{{repository(owner:{owner:?},name:{name:?}){{{fields}}}}}");
        let mut command = self.api_command();
        command.args(["--include", "graphql", "-f", &format!("query={query}")]);
        let (remaining, duration) = self.read_budget()?;
        let (code, text) = oxa_work::command_runner(command, Some(remaining), duration).ok_or(invalid(
            "GitHub read failed: snapshot exceeds byte limit, capture refused or deadline reached",
        ))?;
        // Charge captured bytes, including headers and non-2xx bodies, before interpreting them.
        self.charge_capture(text.len())?;
        let (status, body) = self.response(&text, GitHubResource::Graphql)?;
        if !(200..300).contains(&status) {
            return Err(QueryError::Http(status));
        }
        let result: Value = serde_json::from_str(body).map_err(|_| invalid("invalid GitHub response"))?;
        if result.get("errors").is_some() {
            return Err(invalid(match result["errors"][0]["type"].as_str() {
                Some("FORBIDDEN") => "GitHub GraphQL error (FORBIDDEN)",
                Some("RATE_LIMITED") => "GitHub GraphQL error (RATE_LIMITED)",
                Some("NOT_FOUND") => "GitHub GraphQL error (NOT_FOUND)",
                _ => "GitHub GraphQL error",
            }));
        }
        if code != 0 {
            return Err(QueryError::Http(status));
        }
        let repository = result
            .pointer("/data/repository")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or(invalid("missing GitHub repository"))?;
        Ok((repository, text.len()))
    }

    fn response<'b>(&self, text: &'b str, resource: GitHubResource) -> Result<(u16, &'b str), QueryError> {
        let invalid = QueryError::Other;
        let (headers, body) = text
            .split_once("\r\n\r\n")
            .or_else(|| text.split_once("\n\n"))
            .ok_or(invalid("GitHub command failed without an HTTP response"))?;
        let status = headers
            .lines()
            .next()
            .filter(|line| line.starts_with("HTTP/"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|value| (100..600).contains(value))
            .ok_or(invalid("invalid GitHub HTTP status"))?;
        if let Some(quota) = &self.app_quota {
            let headers = if (200..300).contains(&status) { headers } else { "" };
            quota.borrow_mut().observe(GitHubIdentity::App, resource, headers);
        }
        Ok((status, body))
    }

    fn read_budget(&self) -> Result<(usize, Duration), QueryError> {
        let remaining = MAX_RESPONSE
            .checked_sub(self.bytes.get())
            .filter(|n| *n > 0)
            .ok_or(QueryError::Other("GitHub snapshot exceeds byte limit"))?;
        Ok((remaining, oxa_work::COMMAND_DEADLINE.min(self.time_left()?)))
    }

    fn charge_capture(&self, bytes: usize) -> Result<(), QueryError> {
        let mut used = self.bytes.get();
        charge(&mut used, bytes).map_err(|_| QueryError::Other("GitHub snapshot exceeds byte limit"))?;
        self.bytes.set(used);
        Ok(())
    }

    fn api_command(&self) -> Command {
        let mut command = Command::new(self.gh);
        command
            .env("PATH", self.path)
            .env("GH_TOKEN", self.token)
            .env_remove("GH_DEBUG")
            .args(["api", "--hostname", "github.com"]);
        command
    }

    fn actions_get(&self, path: &str) -> Command {
        let mut command = self.api_command();
        command.args(["--method", "GET", &format!("repos/{}/actions/{path}", self.repo)]);
        command
    }

    fn actions_json(&self, path: &str) -> Result<Value, String> {
        let (remaining, duration) = self.read_budget().map_err(QueryError::message)?;
        let mut command = self.actions_get(path);
        if self.app_quota.is_some() {
            command.arg("--include");
        }
        let (code, text) = oxa_work::command_runner(command, Some(remaining), duration)
            .ok_or("GitHub Actions response exceeds its limit or timed out")?;
        self.charge_capture(text.len()).map_err(QueryError::message)?;
        let body = if self.app_quota.is_some() {
            let (status, body) = self
                .response(&text, crate::lead_quota::GitHubResource::Core)
                .map_err(QueryError::message)?;
            if !(200..300).contains(&status) {
                return Err("GitHub Actions metadata unavailable".into());
            }
            body
        } else {
            &text
        };
        if code != 0 {
            return Err("GitHub Actions metadata unavailable".into());
        }
        serde_json::from_str(body).map_err(|_| "invalid GitHub Actions metadata".into())
    }
}

/// Complete one pull request's changed-file connection with bounded
/// continuations. Every page is the same bounded slice and is held to the same
/// rules: it must belong to the pinned head and total, carry at most the 100
/// nodes it asks for, list distinct paths, and progress only while evidence
/// remains. The pages must cover the whole connection; anything else fails the
/// snapshot before it is published, exactly as an incomplete first page did.
fn complete_changed_files(
    reader: &Reader,
    number: u64,
    head: &str,
    row: &mut Value,
) -> Result<(), SnapshotError> {
    let files = &row["files"];
    let total = files["totalCount"].as_u64().ok_or("missing changed-file count")?;
    let mut page = files.clone();
    let mut rows: Vec<Value> = Vec::new();
    let mut paths = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    loop {
        let more = page["pageInfo"]["hasNextPage"]
            .as_bool()
            .ok_or("missing changed-file pagination")?;
        let batch = page["nodes"].as_array().ok_or("missing changed-file page")?;
        if batch.len() > 100 {
            return Err(SnapshotError::from("invalid changed-file page"));
        }
        for file in batch {
            if !paths.insert(changed_path(file)?.to_owned()) {
                return Err(SnapshotError::from("duplicate changed file"));
            }
        }
        rows.extend(batch.iter().cloned());
        if rows.len() as u64 > total {
            return Err(SnapshotError::from("invalid changed-file page"));
        }
        if !more {
            break;
        }
        let cursor = page["pageInfo"]["endCursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .ok_or("missing changed-file cursor")?
            .to_owned();
        if batch.is_empty() || rows.len() as u64 >= total || !cursors.insert(cursor.clone()) {
            return Err(SnapshotError::from("invalid changed-file pagination progress"));
        }
        let (next, _) = reader
            .query(&format!(
                "p{number}:pullRequest(number:{number}){{number headRefOid \
                 files(first:100,after:{cursor:?}){{totalCount pageInfo{{hasNextPage endCursor}} \
                 nodes{{path deletions}}}}}}"
            ))
            .map_err(|error| SnapshotError::from(error.message()))?;
        let continuation = &next[format!("p{number}")];
        let (seen, seen_head) = identity(continuation)?;
        if seen != number {
            return Err(SnapshotError::from("mismatched PR number"));
        }
        if seen_head != head || continuation["files"]["totalCount"].as_u64() != Some(total) {
            return Err(SnapshotError::Changed("mismatched PR details; retry later"));
        }
        page = continuation["files"].clone();
    }
    if rows.len() as u64 != total {
        return Err(SnapshotError::from("incomplete changed-file listing"));
    }
    row["files"] = serde_json::json!({
        "totalCount": total,
        "pageInfo": {"hasNextPage": false, "endCursor": Value::Null},
        "nodes": rows,
    });
    Ok(())
}

/// One changed-file node's path, which must be usable evidence on its own.
fn changed_path(file: &Value) -> Result<&str, String> {
    file["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "missing changed-file path".to_string())
}

/// Page size of every per-PR connection read in the snapshot query.
const PAGE_ITEMS: u64 = 100;

/// The first per-PR connection whose total exceeds the page it was read with. Only that PR degrades.
fn over_page_limit(row: &Value, reviews: bool) -> Option<&'static str> {
    let mut connections: Vec<(&'static str, &Value)> = vec![
        ("labels", &row["labels"]),
        ("review threads", &row["reviewThreads"]),
        ("latest reviews", &row["latestOpinionatedReviews"]),
        ("base rules", &row["baseRef"]["rules"]),
        (
            "check contexts",
            &row["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"],
        ),
    ];
    if reviews {
        connections.push(("comments", &row["comments"]));
        connections.push(("reviews", &row["reviews"]));
    }
    connections.into_iter().find_map(|(name, connection)| {
        let total = connection["totalCount"].as_u64()?;
        let shown = connection["nodes"].as_array()?.len() as u64;
        (total > shown).then_some(name)
    })
}

fn nodes(value: &Value) -> Result<&Vec<Value>, String> {
    let rows = value["nodes"].as_array().ok_or("missing GitHub connection")?;
    if value["totalCount"].as_u64() != Some(rows.len() as u64) {
        return Err("GitHub connection exceeds snapshot limit; refusing a partial board".into());
    }
    Ok(rows)
}

fn check_outcome(check: &Value) -> Option<&str> {
    match check["status"].as_str() {
        Some("COMPLETED") => check["conclusion"].as_str(),
        // GitHub can retain a prior conclusion while the check is active.
        Some(_) => None,
        None => check["state"].as_str(),
    }
}

fn required_contexts(base: &Value) -> Result<BTreeSet<&str>, String> {
    if !base.is_object() {
        return Err("missing PR base requirements".into());
    }
    let mut required = BTreeSet::new();
    if !base["refUpdateRule"].is_null() {
        for context in base["refUpdateRule"]["requiredStatusCheckContexts"]
            .as_array()
            .ok_or("missing classic check requirements")?
        {
            required.insert(context.as_str().ok_or("invalid check requirement")?);
        }
    }
    // Ref.rules includes active repository and organization rulesets for this base.
    for rule in nodes(&base["rules"])? {
        if rule["type"] == "REQUIRED_STATUS_CHECKS" {
            for check in rule["parameters"]["requiredStatusChecks"]
                .as_array()
                .ok_or("missing ruleset check requirements")?
            {
                required.insert(check["context"].as_str().ok_or("invalid check requirement")?);
            }
        }
    }
    Ok(required)
}

impl Reader<'_> {
    fn acquire(&mut self) -> Result<(Vec<PullRequest>, Vec<Incomplete>), SnapshotError> {
        // Verify the complete paginated identity set before publishing any details.
        let (list, first) = self.listing()?;
        let has_queue = list["mergeQueue"].is_object();
        let numbers: Vec<u64> = nodes(&list["pullRequests"])?
            .iter()
            .map(|pr| pr["number"].as_u64().ok_or("missing PR number"))
            .collect::<Result<_, _>>()?;
        let expected: BTreeMap<_, _> = numbers.iter().zip(nodes(&list["pullRequests"])?).collect();
        let mut details = serde_json::json!({});
        let review_fields = if self.review_rows.is_some() {
            "baseRefOid isCrossRepository authorAssociation
             comments(first:100){totalCount nodes{body updatedAt lastEditedAt
                author{login ... on Bot{databaseId}} editor{... on Bot{databaseId}}}}
             reviews(first:100){totalCount nodes{state body author{login} commit{oid} lastEditedAt}}"
        } else {
            ""
        };
        let (mut offset, mut size) = (0, 15);
        loop {
            let end = numbers.len().min(offset + size);
            let mut fields = LIST.to_owned();
            for n in &numbers[offset..end] {
                fields.push_str(&format!(
                r#"p{n}:pullRequest(number:{n}){{number headRefOid baseRefName state body isDraft reviewDecision mergeable mergeStateStatus {review_fields}
                baseRef{{refUpdateRule{{requiredStatusCheckContexts}} rules(first:100){{totalCount nodes{{type parameters{{... on RequiredStatusChecksParameters{{requiredStatusChecks{{context}}}}}}}}}}}}
                labels(first:100){{totalCount nodes{{name}}}}
                files(first:100){{totalCount pageInfo{{hasNextPage endCursor}} nodes{{path deletions}}}}
                reviewThreads(first:100){{totalCount nodes{{isResolved}}}}
                latestOpinionatedReviews(first:100,writersOnly:true){{totalCount nodes{{state commit{{oid}} author{{login}}}}}}
                commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{totalCount nodes{{
                    ... on CheckRun{{name isRequired(pullRequestNumber:{n}) status conclusion}}
                    ... on StatusContext{{context isRequired(pullRequestNumber:{n}) state}}
                }}}}}}}}}}}}}}"#
                ));
            }
            let (mut page, _) = match self.query(&fields) {
                Ok(page) => page,
                Err(error) if error.retryable() && end - offset > 1 => {
                    size = (end - offset) / 2;
                    continue;
                }
                Err(error) => return Err(error.message().into()),
            };
            listing_page(&page["pullRequests"])?;
            if page["pullRequests"] != first {
                return Err(SnapshotError::Changed(
                    "open PR set changed during snapshot; retry later",
                ));
            }
            for n in &numbers[offset..end] {
                let head = {
                    let row = &page[format!("p{n}")];
                    let (number, head) = identity(row)?;
                    if number != *n {
                        return Err(SnapshotError::from("mismatched PR number"));
                    }
                    if row["headRefOid"] != expected[n]["headRefOid"] {
                        return Err(SnapshotError::Changed("mismatched PR details; retry later"));
                    }
                    head.to_owned()
                };
                let row = &mut page[format!("p{n}")];
                complete_changed_files(self, *n, &head, row)?;
                details[format!("p{n}")] = row.clone();
            }
            offset = end;
            if offset == numbers.len() {
                break;
            }
        }
        let (verified, _) = self.listing()?;
        if verified["pullRequests"] != list["pullRequests"] || verified["mergeQueue"] != list["mergeQueue"] {
            return Err(SnapshotError::Changed(
                "PR identities changed during snapshot; retry later",
            ));
        }
        let reviews = self.review_rows.is_some();
        let rows: Vec<Result<PullRequest, Incomplete>> = nodes(&list["pullRequests"])?
            .iter()
            .map(|pr| {
                let row = &details[format!("p{}", pr["number"])];
                match row["state"].as_str() {
                    Some("OPEN") => (),
                    Some("CLOSED" | "MERGED") => {
                        return Err(SnapshotError::Changed("PR changed during snapshot; retry later"));
                    }
                    _ => return Err(SnapshotError::from("invalid PR state")),
                }
                if let Some(connection) = over_page_limit(row, reviews) {
                    let (number, head) = identity(row)?;
                    return Ok(Err(Incomplete {
                        number,
                        head: head.into(),
                        release_pinned: row["labels"]["nodes"]
                            .as_array()
                            .is_some_and(|labels| labels.iter().any(|l| l["name"] == "release-pinned")),
                        reason: format!(
                            "{connection} exceed the {PAGE_ITEMS}-item GitHub page; the item is not evaluated"
                        ),
                    }));
                }
                let required = required_contexts(&row["baseRef"])?;
                let contexts = &row["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
                let checks = if contexts.is_null() {
                    vec![]
                } else {
                    nodes(contexts)?
                        .iter()
                        .filter(|c| c["isRequired"] == true)
                        .collect()
                };
                let required_ci = if checks.iter().any(|c| {
                    matches!(
                        check_outcome(c),
                        Some(
                            "FAILURE"
                                | "ERROR"
                                | "CANCELLED"
                                | "TIMED_OUT"
                                | "ACTION_REQUIRED"
                                | "STARTUP_FAILURE"
                                | "STALE"
                        )
                    )
                }) {
                    RequiredCi::Failed
                } else if checks.is_empty()
                    || required.iter().any(|name| {
                        !checks
                            .iter()
                            .any(|c| c["name"].as_str().or(c["context"].as_str()) == Some(*name))
                    })
                    || checks
                        .iter()
                        .any(|c| !matches!(check_outcome(c), Some("SUCCESS" | "NEUTRAL" | "SKIPPED")))
                {
                    RequiredCi::Pending
                } else {
                    RequiredCi::Passed
                };
                Ok(Ok(PullRequest {
                    number: row["number"].as_u64().ok_or("missing PR number")?,
                    head: row["headRefOid"].as_str().ok_or("missing PR head")?.into(),
                    release_pinned: nodes(&row["labels"])?
                        .iter()
                        .any(|l| l["name"] == "release-pinned"),
                    findings: row["reviewDecision"] == "CHANGES_REQUESTED"
                        || nodes(&row["reviewThreads"])?
                            .iter()
                            .any(|t| t["isResolved"] == false),
                    required_ci,
                    mergeability: match row["mergeable"].as_str() {
                        Some("MERGEABLE")
                            if matches!(
                                row["mergeStateStatus"].as_str(),
                                Some("CLEAN" | "UNSTABLE" | "HAS_HOOKS")
                            ) || (row["isDraft"] == true && row["mergeStateStatus"] == "DRAFT")
                                || (has_queue
                                    && row["baseRefName"] == "main"
                                    && row["mergeStateStatus"] == "BEHIND")
                                || (required_ci == RequiredCi::Passed
                                    && row["reviewDecision"] == "REVIEW_REQUIRED"
                                    && row["mergeStateStatus"] == "BLOCKED") =>
                        {
                            Mergeability::Mergeable
                        }
                        Some("CONFLICTING") => Mergeability::Conflicting,
                        _ => Mergeability::Unknown,
                    },
                    body: row["body"].as_str().ok_or("missing PR body")?.into(),
                    removes_source: nodes(&row["files"])?.iter().any(|f| {
                        f["deletions"].as_u64().unwrap_or(0) > 0
                            && f["path"].as_str().is_some_and(review::source_path)
                    }),
                    draft: row["isDraft"].as_bool().ok_or("missing draft state")?,
                    reviewed: row["reviewDecision"] == "APPROVED" && {
                        let approvals: Vec<_> = nodes(&row["latestOpinionatedReviews"])?
                            .iter()
                            .filter(|r| r["state"] == "APPROVED")
                            .collect();
                        !approvals.is_empty()
                            && approvals.iter().all(|r| r["commit"]["oid"] == row["headRefOid"])
                    },
                }))
            })
            .collect::<Result<_, SnapshotError>>()?;
        let mut pulls = Vec::new();
        let mut incomplete = Vec::new();
        for row in rows {
            match row {
                Ok(pull) => pulls.push(pull),
                Err(item) => incomplete.push(item),
            }
        }
        self.time_left().map_err(QueryError::message)?;
        if self.review_rows.is_some() {
            self.review_rows = Some(details);
        }
        Ok((pulls, incomplete))
    }
}

impl GitHub for Reader<'_> {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.deadline.set(Instant::now() + Duration::from_secs(120));
        self.bytes.set(0);
        if let Some(quota) = &mut self.app_quota {
            *quota.get_mut() = GitHubQuotas::default();
        }
        // A consistency refusal gets exactly one fresh acquisition, sharing its byte budget.
        let (pull_requests, incomplete) = match self.acquire() {
            Err(SnapshotError::Changed(_)) => self.acquire(),
            result => result,
        }
        .map_err(SnapshotError::message)?;
        Ok(Snapshot {
            pull_requests,
            issues: vec![],
            incomplete,
        })
    }

    /// Open PRs whose evidence is complete. Incomplete PRs are reported only through `snapshot`.
    fn open_pull_requests(&mut self) -> Result<Vec<PullRequest>, String> {
        self.snapshot().map(|snapshot| snapshot.pull_requests)
    }
}

pub(crate) fn run(
    args: &[String],
    root: &Path,
    common: &Path,
    env: &oxa_config::Env,
    dirs: &oxa_config::Dirs,
    policy: &oxa_work::RunPolicy,
) -> Result<i32, String> {
    if args.first().is_some_and(|arg| arg == "--claims-readback") {
        return claims_readback::run(args, root, common, env, dirs);
    }
    if args.first().is_some_and(|arg| arg == "--refill") {
        return refill::run(args, root, common, env, dirs, policy);
    }
    // Existing parser declaration; migrate command/flag metadata together in #1062.
    let (repo, options) = match args {
        [shadow, flag, repo, options @ ..] if shadow == "--shadow" && flag == "--repo" => (repo, options),
        _ => return Err("usage: oxa work board --shadow --repo OWNER/REPO [--once] [--token-stdin] [--app-quota-shadow] [--review-policy FILE] [--queue-evidence] [--landing-shadow] [--queue-report-policy FILE]".into()),
    };
    let (mut once, mut streamed, mut review_file) = (false, false, None);
    let (mut queue_evidence, mut landing_shadow, mut quota_shadow) = (false, false, false);
    let mut report_file = None;
    let mut options = options.iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--once" => once = true,
            "--token-stdin" => streamed = true,
            "--queue-evidence" => queue_evidence = true,
            "--landing-shadow" => landing_shadow = true,
            "--app-quota-shadow" => quota_shadow = true,
            "--queue-report-policy" if report_file.is_none() => {
                report_file = Some(options.next().ok_or("missing queue report policy file")?);
            }
            "--review-policy" if review_file.is_none() => {
                review_file = Some(options.next().ok_or("missing board review policy file")?);
            }
            _ => return Err("invalid board option".into()),
        }
    }
    if !once && !streamed {
        return Err("continuous sampling requires --token-stdin; use --once with a five-minute token-refresh supervisor otherwise".into());
    }
    if quota_shadow && !streamed {
        return Err("App quota observations require the protected --token-stdin source".into());
    }
    if repo.split('/').count() != 2
        || repo.split('/').any(|p| {
            p.is_empty()
                || matches!(p, "." | "..")
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err("expected OWNER/REPO".into());
    }
    if policy.offline {
        return Err("GitHub board is unavailable in offline mode".into());
    }
    if !crate::work_cmd::trusted(root, env, dirs)? {
        return Err("source project is not trusted".into());
    }
    let review_settings = review_file
        .map(|path| review::Settings::read(Path::new(path)))
        .transpose()?;
    let mut report_settings = report_file
        .map(|path| landing::report::Settings::read(Path::new(path)))
        .transpose()?;
    let inherited = (!streamed)
        .then(|| env.get("GH_TOKEN"))
        .flatten()
        .filter(|s| !s.is_empty());
    if !streamed && inherited.is_none() {
        return Err("shadow board requires an explicit GitHub App token".into());
    }
    let stdin = std::io::stdin();
    if streamed && stdin.is_terminal() {
        return Err("App token input requires a private pipe, not a terminal".into());
    }
    let mut input = stdin.lock();
    let gh =
        crate::netguard::trusted_executable_for("gh", Some(&policy.host_path), Some(root), Some(&dirs.home))
            .ok_or("no trusted gh executable")?;
    use std::os::unix::fs::DirBuilderExt;
    let storage = policy.storage.join("board").join(repo);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&storage)
        .map_err(|_| "cannot create shadow storage")?;
    oxa_work::require_private_directory(&storage)?;
    let mut log = oxa_work::shadow::open_log(&storage.join("board-shadow.jsonl"))?;
    oxa_work::shadow::run_with_landing(
        &mut log,
        once,
        || {
            let token = if streamed {
                token_line(&mut input)?
            } else {
                inherited.ok_or("missing App token")?.to_owned()
            };
            let mut reader = Reader::new(&gh, &policy.host_path, repo, &token);
            if quota_shadow {
                reader.app_quota = Some(RefCell::new(GitHubQuotas::default()));
            }
            reader.review_rows = review_settings.as_ref().map(|_| Value::Null);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| "invalid system clock")?
                .as_secs();
            let board_policy = Policy {
                enabled: true,
                ..Policy::default()
            };
            let (board, reviews) = if let Some(settings) = &review_settings {
                review::sample(&mut reader, root, policy, &board_policy, settings, now)
            } else {
                Board::read(&mut reader, &board_policy, &BTreeMap::new(), now)
                    .map(|board| (board, BTreeMap::new()))
            }?;
            let mut queue_rejections = if queue_evidence || landing_shadow || report_settings.is_some() {
                oxa_work::board::queue::read(&board, |fields| reader.repository_read(fields))?
            } else {
                Vec::new()
            };
            if let Some(settings) = &mut report_settings {
                reader.sample_test_reports(&mut queue_rejections, settings)?;
            }
            let (landing, landing_main) = if landing_shadow {
                landing::review_decisions(&reader, &board)?
            } else {
                (BTreeMap::new(), None)
            };
            reader.time_left().map_err(QueryError::message)?;
            Ok(oxa_work::shadow::LandingSample {
                board,
                reviews,
                queue_rejections,
                landing,
                landing_main,
                app_quota: reader
                    .app_quota
                    .as_ref()
                    .and_then(|quota| quota.borrow().app_sample(now)),
            })
        },
        std::thread::sleep,
    )?;
    Ok(0)
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn exhausted_budget_refuses_launch_and_remaining_time_bounds_the_child() {
        let test = TestEnv::new();
        test.write("work/gh", "#!/bin/sh\nprintf started > \"$0.started\"\nsleep 3\n");
        let program = test.work().join("gh");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let marker = test.work().join("gh.started");
        let reader = Reader::new(
            &program,
            std::ffi::OsStr::new("/usr/bin:/bin"),
            "example/repo",
            "synthetic-app-token",
        );
        reader.deadline.set(Instant::now());
        assert!(reader.repository_read("id").is_err());
        assert!(!marker.exists(), "an expired operation must not start a child");
        let start = Instant::now();
        reader.deadline.set(start + Duration::from_millis(200));
        assert!(reader.repository_read("id").is_err());
        assert!(
            marker.exists(),
            "the remaining-budget control must launch the real child"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the last call must not get a fresh 30-second budget"
        );
    }
}
