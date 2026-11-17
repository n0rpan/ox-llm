//! Verified queue rejection observations. The owner supplies the existing repository Reader.
use super::{Action, Board, Entry};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

const BATCH: usize = 8;
const MAX_PRS: usize = 1000;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const QUEUE: &str = r#"mergeQueue(branch:"main"){id}"#;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RejectionEvidence {
    number: u64,
    removal_event: String,
    removed_at: String,
    merge_group_head: String,
    failed_required_check_runs: Vec<u64>,
}

/// Reads at most one relevant timeline event per open PR. The returned facts
/// describe the removed merge group; a current PR head is only a read fence.
/// The caller must keep its normal opt-in, trust and offline checks.
pub fn read(
    board: &Board,
    mut repository_read: impl FnMut(&str) -> Result<(Value, usize), String>,
) -> Result<Vec<RejectionEvidence>, String> {
    let entries: Vec<_> = board
        .entries
        .iter()
        .filter(|entry| !matches!(entry.action, Action::BuildIssue | Action::WaitIssue))
        .collect();
    let numbers: BTreeSet<_> = entries.iter().map(|entry| entry.number).collect();
    if entries.len() > MAX_PRS || numbers.len() != entries.len() || numbers.contains(&0) {
        return Err("invalid queue evidence PR set".into());
    }
    let mut bytes = 0usize;
    let mut rejections = Vec::new();
    let mut queue: Option<Option<String>> = None;
    for batch in entries.chunks(BATCH) {
        let mut fields = QUEUE.to_owned();
        for entry in batch {
            use std::fmt::Write;
            let n = entry.number;
            write!(
                fields,
                r#"p{n}:pullRequest(number:{n}){{
                    number headRefOid state baseRefName
                    timelineItems(last:1,itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT]){{
                        nodes{{__typename ... on RemovedFromMergeQueueEvent{{
                            id reason createdAt mergeQueue{{id}}
                            beforeCommit{{oid statusCheckRollup{{commit{{oid}} contexts(first:100){{
                                totalCount nodes{{__typename ... on CheckRun{{
                                    databaseId isRequired(pullRequestNumber:{n}) status conclusion completedAt
                                }}}}
                            }}}}}}
                        }}}}
                    }}
                }}"#
            )
            .expect("writing to a String cannot fail");
        }
        let (data, read) = repository_read(&fields)?;
        bytes = bytes
            .checked_add(read)
            .filter(|total| *total <= MAX_BYTES)
            .ok_or("queue evidence exceeds snapshot byte limit")?;
        let current = match data.get("mergeQueue") {
            Some(Value::Null) => None,
            Some(value) => Some(node_id(&value["id"]).ok_or("invalid queue identity")?),
            None => return Err("missing queue identity".into()),
        };
        if let Some(previous) = &queue {
            if current != previous.as_deref() {
                return Err("merge queue changed during snapshot".into());
            }
        } else {
            queue = Some(current.map(str::to_owned));
        }
        let Some(queue_id) = current else { continue };
        for entry in batch {
            if let Some(rejection) = rejection(&data[format!("p{}", entry.number)], entry, queue_id)? {
                rejections.push(rejection);
            }
        }
    }
    Ok(rejections)
}

fn node_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        (1..=128).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b))
    })
}

/// Verify a complete lowercase object ID from a GitHub commit field.
pub fn is_object_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64)
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn rejection(row: &Value, entry: &Entry, queue_id: &str) -> Result<Option<RejectionEvidence>, String> {
    if row["number"].as_u64() != Some(entry.number)
        || row["headRefOid"].as_str() != Some(entry.head.as_str())
        || row["state"] != "OPEN"
    {
        return Err("PR changed during queue evidence read".into());
    }
    if row["baseRefName"] != "main" {
        return Ok(None);
    }
    let events = row["timelineItems"]["nodes"]
        .as_array()
        .filter(|items| items.len() <= 1)
        .ok_or("invalid queue timeline")?;
    let Some(event) = events.first() else {
        return Ok(None);
    };
    if event["__typename"] != "RemovedFromMergeQueueEvent"
        || event["reason"] != "failed_checks"
        || event["mergeQueue"]["id"].as_str() != Some(queue_id)
    {
        return Ok(None);
    }

    let Some(removal_event) = node_id(&event["id"]) else {
        return Ok(None);
    };
    let Some(removed_at) = github_utc(&event["createdAt"]) else {
        return Ok(None);
    };
    let commit = &event["beforeCommit"];
    let Some(sha) = commit["oid"]
        .as_str()
        .filter(|sha| is_object_id(sha) && *sha != entry.head)
    else {
        return Ok(None);
    };
    let rollup = &commit["statusCheckRollup"];
    let contexts = &rollup["contexts"];
    let Some(runs) = contexts["nodes"].as_array().filter(|runs| {
        rollup["commit"]["oid"].as_str() == Some(sha)
            && runs.len() <= 100
            && contexts["totalCount"].as_u64() == Some(runs.len() as u64)
    }) else {
        return Ok(None);
    };
    let mut failed = BTreeSet::new();
    for run in runs {
        if run["__typename"] == "CheckRun"
            && run["isRequired"] == true
            && run["status"] == "COMPLETED"
            && matches!(
                run["conclusion"].as_str(),
                Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE")
            )
            && github_utc(&run["completedAt"]).is_some_and(|completed_at| completed_at <= removed_at)
        {
            let id = run["databaseId"]
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or("invalid required check run identity")?;
            failed.insert(id);
        }
    }
    Ok((!failed.is_empty()).then(|| RejectionEvidence {
        number: entry.number,
        removal_event: removal_event.to_owned(),
        removed_at: removed_at.to_owned(),
        merge_group_head: sha.to_owned(),
        failed_required_check_runs: failed.into_iter().collect(),
    }))
}

/// GitHub GraphQL DateTime emits UTC at whole-second precision. Compare only
/// that fixed-width representation; unfamiliar precision stays unavailable.
fn github_utc(value: &Value) -> Option<&str> {
    value.as_str().filter(|time| {
        time.len() == 20
            && time.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 => b == b'-',
                10 => b == b'T',
                13 | 16 => b == b':',
                19 => b == b'Z',
                _ => b.is_ascii_digit(),
            })
    })
}
