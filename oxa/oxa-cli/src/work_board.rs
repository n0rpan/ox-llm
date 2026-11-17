//! Explicitly opted-in, read-only GitHub sampling. Never invokes a worker or mutation.
use oxa_work::board::{Board, GitHub, Mergeability, Policy, PullRequest, RequiredCi};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, IsTerminal, Read},
    path::Path,
    process::Command,
    time::SystemTime,
};

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

struct Reader<'a> {
    gh: &'a Path,
    path: &'a std::ffi::OsStr,
    repo: &'a str,
    token: &'a str,
}

impl Reader<'_> {
    fn query(&self, fields: &str) -> Result<Value, String> {
        let (owner, name) = self.repo.split_once('/').ok_or("expected OWNER/REPO")?;
        let query = format!("query{{repository(owner:{owner:?},name:{name:?}){{{fields}}}}}");
        let mut command = Command::new(self.gh);
        command
            .env("PATH", self.path)
            .env("GH_TOKEN", self.token)
            .env_remove("GH_DEBUG")
            .args([
                "api",
                "--hostname",
                "github.com",
                "graphql",
                "-f",
                &format!("query={query}"),
            ]);
        let (code, text) = crate::report_cmd::command_runner(command, Some(4 * 1024 * 1024))
            .ok_or("GitHub read failed, exceeded its byte limit or timed out")?;
        if code != 0 {
            return Err("GitHub read failed".into());
        }
        let result: Value = serde_json::from_str(&text).map_err(|_| "invalid GitHub response")?;
        if result.get("errors").is_some() {
            return Err("incomplete GitHub response".into());
        }
        result
            .pointer("/data/repository")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or("missing GitHub repository".into())
    }
}

fn nodes(value: &Value) -> Result<&Vec<Value>, String> {
    let rows = value["nodes"].as_array().ok_or("missing GitHub connection")?;
    if value["totalCount"].as_u64() != Some(rows.len() as u64) {
        return Err("GitHub connection exceeds snapshot limit; refusing a partial board".into());
    }
    Ok(rows)
}

impl GitHub for Reader<'_> {
    fn open_pull_requests(&mut self) -> Result<Vec<PullRequest>, String> {
        // One list plus one batched detail query, never per-PR polling. Fail on truncation.
        const LIST: &str = "pullRequests(first:100,states:OPEN){totalCount nodes{number}}";
        let list = self.query(&format!("{LIST} mergeQueue(branch:\"main\"){{id}}"))?;
        let has_queue = list["mergeQueue"].is_object();
        let mut fields = LIST.to_owned();
        for pr in nodes(&list["pullRequests"])? {
            let n = pr["number"].as_u64().ok_or("missing PR number")?;
            fields.push_str(&format!(
                r#"p{n}:pullRequest(number:{n}){{number headRefOid baseRefName state body isDraft reviewDecision mergeable mergeStateStatus
                labels(first:100){{totalCount nodes{{name}}}}
                files(first:100){{totalCount nodes{{path deletions}}}}
                reviewThreads(first:100){{totalCount nodes{{isResolved}}}}
                latestOpinionatedReviews(first:100,writersOnly:true){{totalCount nodes{{state commit{{oid}}}}}}
                commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{totalCount nodes{{
                    ... on CheckRun{{isRequired(pullRequestNumber:{n}) status conclusion}}
                    ... on StatusContext{{isRequired(pullRequestNumber:{n}) state}}
                }}}}}}}}}}}}}}"#
            ));
        }
        let details = self.query(&fields)?;
        let ids = |connection: &Value| -> Result<BTreeSet<u64>, String> {
            nodes(connection)?
                .iter()
                .map(|pr| pr["number"].as_u64().ok_or("missing PR number".into()))
                .collect()
        };
        if ids(&list["pullRequests"])? != ids(&details["pullRequests"])? {
            return Err("open PR set changed during snapshot; retry later".into());
        }
        nodes(&list["pullRequests"])?
            .iter()
            .map(|pr| {
                let row = &details[format!("p{}", pr["number"])];
                if row["state"] != "OPEN" {
                    return Err("PR changed during snapshot; retry later".into());
                }
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
                                    && row["mergeStateStatus"] == "BEHIND") =>
                        {
                            Mergeability::Mergeable
                        }
                        Some("CONFLICTING") => Mergeability::Conflicting,
                        _ => Mergeability::Unknown,
                    },
                    body: row["body"].as_str().ok_or("missing PR body")?.into(),
                    removes_source: nodes(&row["files"])?.iter().any(|f| {
                        f["deletions"].as_u64().unwrap_or(0) > 0
                            && f["path"].as_str().is_some_and(|p| {
                                [
                                    ".rs", ".py", ".go", ".ts", ".tsx", ".js", ".jsx", ".sh", ".c", ".cc",
                                    ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx", ".qml",
                                ]
                                .iter()
                                .any(|ext| p.ends_with(ext))
                            })
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
            .collect()
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
        _ => return Err("usage: oxa work board --shadow --repo OWNER/REPO [--once] [--token-stdin]".into()),
    };
    if options
        .iter()
        .any(|o| !matches!(o.as_str(), "--once" | "--token-stdin"))
    {
        return Err("invalid board option".into());
    }
    let once = options.iter().any(|o| o == "--once");
    let streamed = options.iter().any(|o| o == "--token-stdin");
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
    oxa_work::shadow::run(
        &mut log,
        once,
        || {
            let token = if streamed {
                token_line(&mut input)?
            } else {
                inherited.ok_or("missing App token")?.to_owned()
            };
            let mut reader = Reader {
                gh: &gh,
                path: &policy.host_path,
                repo,
                token: &token,
            };
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| "invalid system clock")?
                .as_secs();
            Board::read(
                &mut reader,
                &Policy {
                    enabled: true,
                    ..Policy::default()
                },
                &BTreeMap::new(),
                now,
            )
        },
        std::thread::sleep,
    )?;
    Ok(0)
}
