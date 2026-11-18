//! Verified queue rejection observations. The owner supplies the existing repository Reader.
use super::{Action, Board, Entry};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub mod report;

const BATCH: usize = 8;
const MAX_PRS: usize = 1000;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const QUEUE: &str = r#"id mergeQueue(branch:"main"){id}"#;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RejectionEvidence {
    number: u64,
    removal_event: String,
    removed_at: String,
    #[serde(skip)]
    notification_target: Option<NotificationTarget>,
    #[serde(skip)]
    publication: Option<PublicationBinding>,
    merge_group_head: String,
    failed_required_check_runs: Vec<u64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    failed_required_workflows: BTreeMap<u64, WorkflowIdentity>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verified_test_reports: Vec<report::VerifiedReports>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct NotificationTarget {
    pub(crate) pull_request_id: String,
    pub(crate) author_id: String,
    pub(crate) login: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PublicationBinding {
    repository_id: String,
    queue_id: String,
    pr_head: String,
    absent: bool,
    admission_upper_bound: Option<u8>,
}

impl NotificationTarget {
    fn read(row: &Value) -> Option<Self> {
        let author = &row["author"];
        let login = author["login"].as_str()?;
        let account = match author["__typename"].as_str()? {
            "User" => login,
            "Bot" => login.strip_suffix("[bot]").unwrap_or(login),
            _ => return None,
        };
        if account.len() > 100
            || !account.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            || !account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return None;
        }
        Some(Self {
            pull_request_id: node_id(&row["id"])?.to_owned(),
            author_id: node_id(&author["id"])?.to_owned(),
            login: login.to_owned(),
        })
    }
}

pub(crate) struct Removal<'a> {
    pub number: u64,
    pub event: &'a str,
    pub group: &'a str,
    pub removed_at: &'a str,
}

impl RejectionEvidence {
    pub(crate) fn notification_target(&self) -> Option<&NotificationTarget> {
        self.notification_target.as_ref()
    }

    pub(crate) fn publication_binding(&self) -> Option<(crate::RejectedLanding, Option<u8>)> {
        let binding = self.publication.as_ref()?;
        let target = self.notification_target()?;
        Some((
            crate::RejectedLanding {
                repository_id: binding.repository_id.clone(),
                queue_id: binding.queue_id.clone(),
                pull_request_id: target.pull_request_id.clone(),
                pull_request_number: self.number,
                pr_head: binding.pr_head.clone(),
                rejected_group: self.merge_group_head.clone(),
                removal_id: self.removal_event.clone(),
                removed_at: oxa_session::parse_utc_seconds(&self.removed_at)?,
            },
            binding.admission_upper_bound.filter(|_| binding.absent),
        ))
    }

    pub(crate) fn admission_identity(&self) -> Removal<'_> {
        Removal {
            number: self.number,
            event: &self.removal_event,
            group: &self.merge_group_head,
            removed_at: &self.removed_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
struct WorkflowIdentity {
    check_suite_id: u64,
    workflow_run_id: u64,
    // GitHub may begin a new workflow attempt after the check completed.
    // The job's own attempt must match before using any test report.
    observed_run_attempt: u64,
}

impl WorkflowIdentity {
    fn read(check: &Value, merge_group_head: &str) -> Option<Self> {
        let suite = &check["checkSuite"];
        let run = &suite["workflowRun"];
        if suite["commit"]["oid"].as_str() != Some(merge_group_head) || run["event"] != "merge_group" {
            return None;
        }
        Some(Self {
            check_suite_id: positive_id(&suite["databaseId"])?,
            workflow_run_id: positive_id(&run["databaseId"])?,
            observed_run_attempt: positive_id(&run["runAttempt"])?,
        })
    }
}

fn positive_id(value: &Value) -> Option<u64> {
    value.as_u64().filter(|id| *id > 0)
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
                    id number headRefOid state baseRefName
                    mergeQueueEntry{{id}}
                    admissions:timelineItems(last:3,itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT]){{
                        pageInfo{{hasPreviousPage hasNextPage}} nodes{{__typename ... on AddedToMergeQueueEvent{{id}}}}
                    }}
                    author{{__typename login ... on User{{id}} ... on Bot{{id}}}}
                    timelineItems(last:1,itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT]){{
                        nodes{{__typename ... on RemovedFromMergeQueueEvent{{
                            id reason createdAt mergeQueue{{id}}
                            beforeCommit{{oid statusCheckRollup{{commit{{oid}} contexts(first:100){{
                                totalCount nodes{{__typename ... on CheckRun{{
                                    databaseId isRequired(pullRequestNumber:{n}) status conclusion completedAt
                                    checkSuite{{databaseId commit{{oid}} workflowRun{{databaseId event runAttempt}}}}
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
            if let Some(rejection) = rejection(
                &data[format!("p{}", entry.number)],
                entry,
                queue_id,
                node_id(&data["id"]),
            )? {
                rejections.push(rejection);
            }
        }
    }
    Ok(rejections)
}

/// Re-read the exact pending entry using the same repository Reader.
pub fn read_acknowledged_admission(
    acknowledgment: &crate::AdmissionAcknowledgment,
    mut repository_read: impl FnMut(&str) -> Result<(Value, usize), String>,
) -> Result<Value, String> {
    let rejection = &acknowledgment.intent.delivery.intent.rejection;
    let number = rejection.pull_request_number;
    let (data, bytes) = repository_read(&format!(
        r#"id mergeQueue(branch:"main"){{id}} pullRequest(number:{number}){{
            mergeQueueEntry{{id enqueuedAt mergeQueue{{id repository{{id}}}}
                pullRequest{{id number headRefOid}} headCommit{{oid}}
                enqueuer{{__typename ... on User{{id}} ... on Bot{{id}}}}}}
        }}"#
    ))?;
    if bytes > MAX_BYTES
        || data["id"].as_str() != Some(&rejection.repository_id)
        || data["mergeQueue"]["id"].as_str() != Some(&rejection.queue_id)
    {
        return Err("queue changed since the recorded admission acknowledgment".into());
    }
    data["pullRequest"]["mergeQueueEntry"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .ok_or_else(|| "the acknowledged queue entry is unavailable".into())
}

fn node_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| is_node_id(id))
}

pub(crate) fn is_node_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b))
}

/// Verify a complete lowercase object ID from a GitHub commit field.
pub fn is_object_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64)
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn rejection(
    row: &Value,
    entry: &Entry,
    queue_id: &str,
    repository_id: Option<&str>,
) -> Result<Option<RejectionEvidence>, String> {
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
    let mut failed = BTreeMap::new();
    for run in runs {
        if run["__typename"] == "CheckRun"
            && run["isRequired"] == true
            && run["status"] == "COMPLETED"
            && matches!(
                run["conclusion"].as_str(),
                Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE")
            )
        {
            let Some(completed_at) = github_utc(&run["completedAt"]) else {
                return Ok(None);
            };
            if completed_at > removed_at {
                continue;
            }
            let id = positive_id(&run["databaseId"]).ok_or("invalid required check run identity")?;
            let workflow = WorkflowIdentity::read(run, sha);
            if failed
                .insert(id, workflow)
                .is_some_and(|previous| previous != workflow)
            {
                return Err("conflicting workflow identity for a required check run".into());
            }
        }
    }
    Ok((!failed.is_empty()).then(|| RejectionEvidence {
        number: entry.number,
        removal_event: removal_event.to_owned(),
        removed_at: removed_at.to_owned(),
        notification_target: NotificationTarget::read(row),
        publication: repository_id.map(|repository_id| PublicationBinding {
            repository_id: repository_id.to_owned(),
            queue_id: queue_id.to_owned(),
            pr_head: entry.head.clone(),
            absent: row.get("mergeQueueEntry") == Some(&Value::Null),
            // Earlier heads can only increase the count. An ambiguous or
            // exhausted upper bound cannot authorize a native retry.
            admission_upper_bound: admission_upper_bound(&row["admissions"]),
        }),
        merge_group_head: sha.to_owned(),
        failed_required_check_runs: failed.keys().copied().collect(),
        failed_required_workflows: failed
            .into_iter()
            .filter_map(|(id, workflow)| workflow.map(|workflow| (id, workflow)))
            .collect(),
        verified_test_reports: Vec::new(),
    }))
}

fn admission_upper_bound(page: &Value) -> Option<u8> {
    let nodes = page["nodes"].as_array().filter(|nodes| nodes.len() <= 3)?;
    if page["pageInfo"]["hasPreviousPage"] != false || page["pageInfo"]["hasNextPage"] != false {
        return None;
    }
    let mut ids = BTreeSet::new();
    nodes
        .iter()
        .all(|row| {
            row["__typename"] == "AddedToMergeQueueEvent"
                && node_id(&row["id"]).is_some_and(|id| ids.insert(id))
        })
        .then_some(nodes.len() as u8)
}

/// Validate a canonical UTC second before comparing GitHub event times.
fn github_utc(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|time| oxa_session::parse_utc_seconds(time).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admission_cap_counts_filtered_events_and_refuses_incomplete_pages() {
        let mut page = json!({"totalCount":42,"pageInfo":{"hasPreviousPage":false,"hasNextPage":false},
            "nodes":[{"__typename":"AddedToMergeQueueEvent","id":"E_admitted"}]});
        assert_eq!(admission_upper_bound(&page), Some(1));
        page["pageInfo"]["hasPreviousPage"] = json!(true);
        assert_eq!(admission_upper_bound(&page), None);
    }

    #[test]
    fn notification_target_requires_the_pr_and_a_supported_author_identity() {
        let mut row = json!({"id":"pr-42"});
        for (author, target) in [
            (
                json!({"__typename":"User","id":"user-1","login":"sample-author"}),
                Some(json!({"pull_request_id":"pr-42","author_id":"user-1","login":"sample-author"})),
            ),
            (
                json!({"__typename":"Bot","id":"bot-1","login":"sample-agent[bot]"}),
                Some(json!({"pull_request_id":"pr-42","author_id":"bot-1","login":"sample-agent[bot]"})),
            ),
            (Value::Null, None),
            (
                json!({"__typename":"User","id":"user-1","login":"@unverified"}),
                None,
            ),
        ] {
            row["author"] = author;
            assert_eq!(
                NotificationTarget::read(&row).map(|target| serde_json::to_value(target).unwrap()),
                target
            );
        }
        row["author"] = json!({"__typename":"User","id":"user-1","login":"sample-author"});
        row["id"] = Value::Null;
        assert!(NotificationTarget::read(&row).is_none());
    }
}
