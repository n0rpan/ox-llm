//! Explicitly opted-in, read-only GitHub sampling. Never invokes a worker or mutation.
use oxa_work::board::{Board, GitHub, Mergeability, Policy, PullRequest, RequiredCi};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, IsTerminal, Read},
    path::Path,
    process::Command,
    time::{Duration, SystemTime},
};

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
impl QueryError {
    fn retryable(&self) -> bool {
        matches!(self, Self::Http(502 | 503 | 504))
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
        }
    }

    /// Fixed repository fields from native callers share the bounded, redacted transport.
    /// The byte count lets callers enforce an aggregate snapshot budget across queries.
    pub(crate) fn repository_read(&self, fields: &str) -> Result<(Value, usize), String> {
        self.query(fields).map_err(QueryError::message)
    }

    fn listing(&self, bytes: &mut usize) -> Result<(Value, Value), String> {
        let (mut fields, mut rows, mut seen, mut cursors) =
            (LIST.to_owned(), vec![], BTreeSet::new(), BTreeSet::new());
        let mut result = Value::Null;
        loop {
            let (page, read) =
                self.repository_read(&format!("{fields} mergeQueue(branch:\"main\"){{id}}"))?;
            charge(bytes, read)?;
            if result.is_null() {
                result = page.clone();
            }
            let connection = &page["pullRequests"];
            let total = connection["totalCount"].as_u64().ok_or("missing PR count")?;
            if total > 1000
                || connection["totalCount"] != result["pullRequests"]["totalCount"]
                || page["mergeQueue"] != result["mergeQueue"]
            {
                return Err("GitHub listing exceeds limit or changed during snapshot".into());
            }
            let nodes = connection["nodes"].as_array().ok_or("missing PR page")?;
            if nodes.len() > 100 {
                return Err("oversized PR page".into());
            }
            for row in nodes {
                let number = row["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or("invalid PR number")?;
                if !seen.insert(number) || row["headRefOid"].as_str().is_none_or(str::is_empty) {
                    return Err("duplicate PR or missing head in listing".into());
                }
                rows.push(row.clone());
            }
            let more = connection["pageInfo"]["hasNextPage"]
                .as_bool()
                .ok_or("missing PR pagination")?;
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
                    std::thread::sleep(Duration::from_millis(200 << retries));
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
        let mut command = Command::new(self.gh);
        command
            .env("PATH", self.path)
            .env("GH_TOKEN", self.token)
            .env_remove("GH_DEBUG")
            .args([
                "api",
                "--include",
                "--hostname",
                "github.com",
                "graphql",
                "-f",
                &format!("query={query}"),
            ]);
        let (code, text) = crate::report_cmd::command_runner(command, Some(MAX_RESPONSE)).ok_or(invalid(
            "GitHub read failed, exceeded its byte limit or timed out",
        ))?;
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
}

fn nodes(value: &Value) -> Result<&Vec<Value>, String> {
    let rows = value["nodes"].as_array().ok_or("missing GitHub connection")?;
    if value["totalCount"].as_u64() != Some(rows.len() as u64) {
        return Err("GitHub connection exceeds snapshot limit; refusing a partial board".into());
    }
    Ok(rows)
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

impl GitHub for Reader<'_> {
    fn open_pull_requests(&mut self) -> Result<Vec<PullRequest>, String> {
        // Verify the complete paginated identity set before publishing any details.
        let mut bytes = 0;
        let (list, first) = self.listing(&mut bytes)?;
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
                files(first:100){{totalCount nodes{{path deletions}}}}
                reviewThreads(first:100){{totalCount nodes{{isResolved}}}}
                latestOpinionatedReviews(first:100,writersOnly:true){{totalCount nodes{{state commit{{oid}} author{{login}}}}}}
                commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{totalCount nodes{{
                    ... on CheckRun{{name isRequired(pullRequestNumber:{n}) status conclusion}}
                    ... on StatusContext{{context isRequired(pullRequestNumber:{n}) state}}
                }}}}}}}}}}}}}}"#
                ));
            }
            let (page, read) = match self.query(&fields) {
                Ok(page) => page,
                Err(error) if error.retryable() && end - offset > 1 => {
                    size = (end - offset) / 2;
                    continue;
                }
                Err(error) => return Err(error.message()),
            };
            charge(&mut bytes, read)?;
            if page["pullRequests"] != first {
                return Err("open PR set changed during snapshot; retry later".into());
            }
            for n in &numbers[offset..end] {
                let row = &page[format!("p{n}")];
                if row["number"].as_u64() != Some(*n) || row["headRefOid"] != expected[n]["headRefOid"] {
                    return Err("missing or mismatched PR details".into());
                }
                details[format!("p{n}")] = row.clone();
            }
            offset = end;
            if offset == numbers.len() {
                break;
            }
        }
        let (verified, _) = self.listing(&mut bytes)?;
        if verified["pullRequests"] != list["pullRequests"] || verified["mergeQueue"] != list["mergeQueue"] {
            return Err("PR identities changed during snapshot; retry later".into());
        }
        let rows = nodes(&list["pullRequests"])?
            .iter()
            .map(|pr| {
                let row = &details[format!("p{}", pr["number"])];
                if row["state"] != "OPEN" {
                    return Err("PR changed during snapshot; retry later".into());
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
                        c["state"].as_str().or(c["conclusion"].as_str()),
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
                    || checks.iter().any(|c| {
                        !matches!(
                            c["state"].as_str().or(c["conclusion"].as_str()),
                            Some("SUCCESS" | "NEUTRAL" | "SKIPPED")
                        )
                    })
                {
                    RequiredCi::Pending
                } else {
                    RequiredCi::Passed
                };
                Ok(PullRequest {
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
                })
            })
            .collect();
        if self.review_rows.is_some() {
            self.review_rows = Some(details);
        }
        rows
    }
}

pub(crate) fn run(
    args: &[String],
    root: &Path,
    env: &oxa_config::Env,
    dirs: &oxa_config::Dirs,
    policy: &oxa_work::RunPolicy,
) -> Result<i32, String> {
    // Existing parser declaration; migrate command/flag metadata together in #1062.
    let (repo, options) = match args {
        [shadow, flag, repo, options @ ..] if shadow == "--shadow" && flag == "--repo" => (repo, options),
        _ => return Err("usage: oxa work board --shadow --repo OWNER/REPO [--once] [--token-stdin] [--review-policy FILE]".into()),
    };
    let (mut once, mut streamed, mut review_file) = (false, false, None);
    let mut options = options.iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--once" => once = true,
            "--token-stdin" => streamed = true,
            "--review-policy" if review_file.is_none() => {
                review_file = Some(options.next().ok_or("missing board review policy file")?);
            }
            _ => return Err("invalid board option".into()),
        }
    }
    if !once && !streamed {
        return Err("continuous sampling requires --token-stdin; use --once with a five-minute token-refresh supervisor otherwise".into());
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
    let inherited = env.get("GH_TOKEN").filter(|s| !s.is_empty());
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
    let mut log = oxa_work::shadow::open_log(&storage.join("board-shadow.jsonl"))?;
    oxa_work::shadow::run_with_reviews(
        &mut log,
        once,
        || {
            let token = if streamed {
                token_line(&mut input)?
            } else {
                inherited.ok_or("missing App token")?.to_owned()
            };
            let mut reader = Reader::new(&gh, &policy.host_path, repo, &token);
            reader.review_rows = review_settings.as_ref().map(|_| Value::Null);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| "invalid system clock")?
                .as_secs();
            let board_policy = Policy {
                enabled: true,
                ..Policy::default()
            };
            if let Some(settings) = &review_settings {
                review::sample(&mut reader, root, policy, &board_policy, settings, now)
            } else {
                Board::read(&mut reader, &board_policy, &BTreeMap::new(), now)
                    .map(|board| (board, BTreeMap::new()))
            }
        },
        std::thread::sleep,
    )?;
    Ok(0)
}
