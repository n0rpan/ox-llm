//! Runner-owned pull request context, bound to an immutable reviewed commit.
use crate::{
    append_capped, checked, default_max_time, git, github_json, github_runner_guidance,
    isolate_git_environment, load_task_checkpoint, Result, RunPolicy, Task, GITHUB_RUNNER_LEAD,
    MAX_COMMENT_BYTES,
};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

const PR_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      title body headRefOid headRefName isCrossRepository
      comments(first:100) { totalCount nodes { author { login } body } }
      reviews(first:100) { totalCount nodes { author { login } body state } }
      reviewThreads(first:100) {
        totalCount nodes {
          isResolved path line
          comments(first:30) { totalCount nodes { author { login } body } }
        }
      }
      files(first:100) { totalCount nodes { path } }
      commits(last:1) { nodes { commit {
        oid
        statusCheckRollup { contexts(first:100) { totalCount nodes {
          ... on CheckRun { name conclusion }
          ... on StatusContext { context state }
        } } }
      } } }
    }
  }
}"#;

fn oid(value: &str) -> bool {
    value.len() == 40 && crate::board::queue::is_object_id(value)
}

/// The pull request reviewed before an attempt was admitted.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewedPullRequest {
    pub number: u64,
    pub head: String,
}

impl Task {
    pub(crate) fn reviewed_source(&self) -> Option<(u64, &str)> {
        if !self.prompt.starts_with(GITHUB_RUNNER_LEAD) {
            return None;
        }
        let legacy = self.id.strip_prefix("pr-").and_then(|id| {
            let (number, head) = id.split_once('-')?;
            let number = number.parse::<u64>().ok()?;
            (number != 0 && oid(head)).then_some((number, head))
        });
        if let Some(source) = &self.reviewed_pull_request {
            let reviewed = (source.number, source.head.as_str());
            if source.number == 0
                || !crate::board::queue::is_object_id(&source.head)
                || (self.id.starts_with("pr-") && legacy != Some(reviewed))
            {
                return None;
            }
            return Some(reviewed);
        }
        legacy
    }

    pub(crate) fn reviewed_head(&self) -> Option<&str> {
        self.reviewed_source().map(|(_, head)| head)
    }
}

pub fn pull_request_task(
    repo: &Path,
    gh: &Path,
    policy: &RunPolicy,
    number: u64,
    model: String,
) -> Result<Task> {
    if number == 0 {
        return Err("pull request number must be positive".into());
    }
    let mut command = Command::new(gh);
    isolate_git_environment(&mut command);
    command.env("PATH", &policy.host_path).current_dir(repo).args([
        "api",
        "graphql",
        "-f",
        &format!("query={PR_QUERY}"),
        "-F",
        "owner={owner}",
        "-F",
        "repo={repo}",
        "-F",
        &format!("number={number}"),
    ]);
    let response = github_json(command, "pull request")?;
    if response.get("errors").is_some() {
        return Err("GitHub returned an incomplete pull request snapshot".into());
    }
    let pr = &response["data"]["repository"]["pullRequest"];
    if pr["isCrossRepository"] != false {
        return Err("pull request must have a branch in the source repository".into());
    }
    let head = pr["headRefOid"]
        .as_str()
        .filter(|value| oid(value))
        .ok_or("reviewed head missing")?;
    let branch = pr["headRefName"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or("pull request branch missing")?;
    let id = format!("pr-{number}-{head}");
    if let Some(saved) = load_task_checkpoint(policy, &id)? {
        let task = saved.task;
        if task.branch != branch || task.model != model || task.reviewed_head() != Some(head) {
            return Err("task specification changed; use a new task id".into());
        }
        return Ok(task);
    }
    // Capture acceptance from the trusted local main lineage. The branch can
    // be behind main without changing the reviewed tree before the worker runs.
    let base = checked(git(policy, repo, &["merge-base", "refs/heads/main", head]))?;
    let mut prompt = github_runner_guidance("pull request");
    prompt.push_str(&pull_request_prompt(number, head, pr)?);
    Ok(Task {
        id,
        model,
        branch: branch.into(),
        base,
        prompt,
        max_time: default_max_time(),
        reviewed_pull_request: None,
    })
}

fn rows<'a>(connection: &'a Value, incomplete: &mut bool) -> Result<&'a [Value]> {
    let nodes = connection["nodes"]
        .as_array()
        .ok_or("missing pull request packet rows")?;
    let count = connection["totalCount"]
        .as_u64()
        .ok_or("missing pull request packet row count")?;
    if count < nodes.len() as u64 {
        return Err("inconsistent pull request packet row count".into());
    }
    *incomplete |= count > nodes.len() as u64;
    Ok(nodes)
}

fn append_review(output: &mut String, context: &[&str], note: &Value, complete: &mut bool) -> Result<()> {
    let text = note["body"]
        .as_str()
        .ok_or("pull request note body malformed")?
        .trim();
    if text.is_empty() || !*complete {
        return Ok(());
    }
    let author = note["author"]["login"]
        .as_str()
        .filter(|login| !login.is_empty())
        .unwrap_or("unknown");
    *complete &= !append_capped(output, context, MAX_COMMENT_BYTES);
    *complete &= !append_capped(output, &["by ", author, ":\n", text, "\n\n"], MAX_COMMENT_BYTES);
    Ok(())
}

fn pull_request_prompt(number: u64, head: &str, pr: &Value) -> Result<String> {
    let title = pr["title"].as_str().ok_or("pull request title missing")?;
    let body = pr["body"].as_str().ok_or("pull request body missing")?;
    let commit = &pr["commits"]["nodes"][0]["commit"];
    if commit["oid"] != head {
        return Err("pull request review and checks refer to different heads".into());
    }
    let mut incomplete = false;
    let mut description = String::new();
    incomplete |= append_capped(&mut description, &[title, "\n\n", body], 16 * 1024);
    let mut prompt = format!("Address pull request #{number} at {head}: {description}");
    let mut review_complete = true;
    let mut thread = String::new();
    for review in rows(&pr["reviews"], &mut incomplete)? {
        let state = review["state"]
            .as_str()
            .ok_or("pull request review state missing")?;
        append_review(
            &mut thread,
            &["Review (", state, ") "],
            review,
            &mut review_complete,
        )?;
    }
    for comment in rows(&pr["comments"], &mut incomplete)? {
        append_review(
            &mut thread,
            &["Conversation comment "],
            comment,
            &mut review_complete,
        )?;
    }
    for review in rows(&pr["reviewThreads"], &mut incomplete)? {
        let resolved = review["isResolved"]
            .as_bool()
            .ok_or("pull request thread state missing")?;
        if resolved {
            continue;
        }
        let path = review["path"]
            .as_str()
            .ok_or("pull request thread path missing")?;
        let line = review["line"]
            .as_u64()
            .map_or_else(|| "?".into(), |line| line.to_string());
        for comment in rows(&review["comments"], &mut incomplete)? {
            append_review(
                &mut thread,
                &["Unresolved thread ", path, ":", &line, " "],
                comment,
                &mut review_complete,
            )?;
        }
    }
    incomplete |= !review_complete;
    if !thread.is_empty() {
        prompt.push_str("\n\nReview and conversation data\n\n");
        prompt.push_str(&thread);
    }
    if !commit["statusCheckRollup"].is_null() {
        let mut failing = String::new();
        for check in rows(&commit["statusCheckRollup"]["contexts"], &mut incomplete)? {
            match (check["name"].as_str(), check["context"].as_str()) {
                (Some(name), _)
                    if matches!(
                        check["conclusion"].as_str(),
                        Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" | "CANCELLED")
                    ) =>
                {
                    incomplete |= append_capped(&mut failing, &["- ", name, "\n"], 4096);
                }
                (_, Some(name)) if matches!(check["state"].as_str(), Some("ERROR" | "FAILURE")) => {
                    incomplete |= append_capped(&mut failing, &["- ", name, "\n"], 4096);
                }
                (None, None) => return Err("pull request check name missing".into()),
                _ => {}
            }
        }
        if !failing.is_empty() {
            prompt.push_str("\n\nFailing checks at the reviewed head\n");
            prompt.push_str(&failing);
        }
    }
    let mut files = String::new();
    for file in rows(&pr["files"], &mut incomplete)? {
        let path = file["path"]
            .as_str()
            .ok_or("pull request changed file path missing")?;
        incomplete |= append_capped(&mut files, &["- ", path, "\n"], 4096);
    }
    if !files.is_empty() {
        prompt.push_str("\n\nChanged files\n");
        prompt.push_str(&files);
    }
    if incomplete {
        prompt.push_str(
            "\n\nNEED-METADATA if omitted GitHub packet data is required. Ask the runner for it.\n",
        );
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn explicit_reviewed_source_accepts_full_sha256_without_extending_legacy_ids() {
        let head = "a".repeat(64);
        let mut task = Task {
            id: "board-fixture-1-1".into(),
            model: "fixture".into(),
            branch: "agent/fixture".into(),
            base: "main".into(),
            prompt: github_runner_guidance("pull request"),
            max_time: default_max_time(),
            reviewed_pull_request: Some(ReviewedPullRequest {
                number: 1,
                head: head.clone(),
            }),
        };
        assert_eq!(task.reviewed_source(), Some((1, head.as_str())));
        task.id = format!("pr-1-{head}");
        assert_eq!(task.reviewed_source(), None);
        task.id = format!("pr-1-{HEAD}");
        task.reviewed_pull_request = None;
        assert_eq!(task.reviewed_source(), Some((1, HEAD)));
    }

    fn packet() -> Value {
        json!({
            "title": "Synthetic PR",
            "body": "Fixture",
            "reviews": {"totalCount": 0, "nodes": []},
            "comments": {"totalCount": 0, "nodes": []},
            "reviewThreads": {"totalCount": 0, "nodes": []},
            "files": {"totalCount": 0, "nodes": []},
            "commits": {"nodes": [{"commit": {"oid": HEAD, "statusCheckRollup": null}}]}
        })
    }

    #[test]
    fn clean_empty_packet_has_no_missing_data_marker() {
        let prompt = pull_request_prompt(1, HEAD, &packet()).unwrap();
        assert_eq!(
            prompt,
            format!("Address pull request #1 at {HEAD}: Synthetic PR\n\nFixture")
        );
    }

    #[test]
    fn rejects_inconsistent_heads_and_malformed_connections() {
        let mut pr = packet();
        pr["commits"]["nodes"][0]["commit"]["oid"] = json!("stale-head");
        assert!(pull_request_prompt(1, HEAD, &pr).is_err());
        let mut pr = packet();
        pr["reviews"] = json!(3);
        assert!(pull_request_prompt(1, HEAD, &pr).is_err());
        let mut pr = packet();
        pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({"contexts": 3});
        assert!(pull_request_prompt(1, HEAD, &pr).is_err());
    }

    #[test]
    fn bounded_packet_routes_omitted_threads_and_checks_back_to_runner() {
        let mut pr = packet();
        pr["comments"] = json!({
            "totalCount": 2,
            "nodes": [{"body": "é".repeat(MAX_COMMENT_BYTES)}]
        });
        pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({
            "contexts": {
                "totalCount": 1,
                "nodes": [{"name": "failure".repeat(1000), "conclusion": "FAILURE"}]
            }
        });
        let prompt = pull_request_prompt(1, HEAD, &pr).unwrap();
        assert!(prompt.contains("NEED-METADATA"));
        assert!(prompt.contains("Ask the runner"));
        assert!(prompt.contains("by unknown"));
        assert!(prompt.contains("Failing checks"));
        assert!(prompt.len() < MAX_COMMENT_BYTES + 4096 + 1024);
    }
}
