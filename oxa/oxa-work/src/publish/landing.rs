//! Typed landing operations over the canonical publisher's GitHub transport.
use super::{gh, json, repository};
use crate::{
    board::queue::{is_node_id, is_object_id, recipient_mention},
    checked, git, Result, RunPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs::File, path::Path};

const ADD_COMMENT: &str = "mutation($pr:ID!,$body:String!){addComment(input:{subjectId:$pr,body:$body}){
    commentEdge{node{id repository{id} pullRequest{id headRefOid author{login ... on User{id} ... on Bot{id}}}
    author{__typename
    ... on User{id} ... on Bot{id}} createdAt}}}}";
const ENQUEUE: &str = "mutation($pr:ID!,$head:GitObjectID!){enqueuePullRequest(input:{
    pullRequestId:$pr,expectedHeadOid:$head}){mergeQueueEntry{id enqueuedAt
    mergeQueue{id repository{id}} pullRequest{id number headRefOid} headCommit{oid}
    enqueuer{__typename ... on User{id} ... on Bot{id}}}}}";

/// Identity supplied by the board Reader and matched to the owner journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RejectedLanding {
    pub repository_id: String,
    pub queue_id: String,
    pub pull_request_id: String,
    pub pull_request_number: u64,
    pub pr_head: String,
    pub rejected_group: String,
    pub removal_id: String,
    pub removed_at: u64,
}

impl RejectedLanding {
    fn check(&self) -> Result<()> {
        if ![
            &self.repository_id,
            &self.queue_id,
            &self.pull_request_id,
            &self.removal_id,
        ]
        .into_iter()
        .all(|id| is_node_id(id))
            || !is_object_id(&self.pr_head)
            || !is_object_id(&self.rejected_group)
            || self.rejected_group == self.pr_head
            || self.removed_at == 0
            || self.pull_request_number == 0
        {
            return Err("invalid rejected landing identity".into());
        }
        Ok(())
    }
}

/// A journal reservation for a single notification of the rejected PR author.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NoticeIntent {
    pub rejection: RejectedLanding,
    pub recipient_id: String,
    pub recipient_login: String,
    pub publisher_id: String,
}

impl NoticeIntent {
    fn body(&self, failed_tests: &[&str]) -> Result<String> {
        self.rejection.check()?;
        let login = recipient_mention(&self.recipient_login, self.recipient_login.ends_with("[bot]"))
            .ok_or("invalid landing notice recipient")?;
        if !is_node_id(&self.recipient_id)
            || !is_node_id(&self.publisher_id)
            || failed_tests.is_empty()
            || failed_tests.len() > 128
        {
            return Err("invalid landing notice".into());
        }
        let mut body = format!(
            "@{login} required checks rejected merge-group commit `{}` for PR head `{}`. Failed tests:",
            self.rejection.rejected_group, self.rejection.pr_head
        );
        for name in failed_tests {
            if name.is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
                return Err("invalid reported test name".into());
            }
            body.push_str("\n- `");
            body.push_str(&name.replace('`', "’"));
            body.push('`');
        }
        Ok(body)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeliveryReceipt {
    pub intent: NoticeIntent,
    pub comment_id: String,
    pub created_at: u64,
}

/// A journal reservation authorized by the landing policy after confirmed delivery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetryIntent {
    pub delivery: DeliveryReceipt,
    pub observed_main: String,
    pub observed_at: u64,
    pub last_admission_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionReceipt {
    pub acknowledgment: AdmissionAcknowledgment,
    pub admitted_group: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionAcknowledgment {
    pub intent: RetryIntent,
    pub queue_entry_id: String,
    pub enqueued_at: u64,
}

pub enum Admission {
    Pending(AdmissionAcknowledgment),
    Confirmed(AdmissionReceipt),
}

impl AdmissionAcknowledgment {
    /// Reconcile only the entry acknowledged by the mutation. Its group may
    /// have been assigned after dispatch and read through the board Reader.
    pub fn reconcile(&self, entry: &Value) -> Result<Admission> {
        let result = admission(&self.intent, entry)?;
        let observed = match &result {
            Admission::Pending(observed) => observed,
            Admission::Confirmed(observed) => &observed.acknowledgment,
        };
        if observed != self {
            return Err("queue entry changed since publication acknowledgment".into());
        }
        Ok(result)
    }
}

/// Only the canonical publisher may dispatch these operations. The caller must
/// hold the owner journal lock, reserve the intent, and revalidate current
/// Reader evidence before calling. An ambiguous outcome is never a retry signal.
pub struct LandingPublisher<'a> {
    repo: &'a Path,
    policy: &'a RunPolicy,
    gh_binary: &'a Path,
    journal: &'a File,
}

impl<'a> LandingPublisher<'a> {
    pub fn new(repo: &'a Path, policy: &'a RunPolicy, gh_binary: &'a Path, journal: &'a File) -> Self {
        Self {
            repo,
            policy,
            gh_binary,
            journal,
        }
    }

    fn call(&self, args: &[&str]) -> Result<Value> {
        json(&gh(self.gh_binary, self.policy, self.repo, self.journal, args)?)
            .map_err(|_| "invalid GitHub landing publication response".into())
    }

    fn graphql(&self, host: &str, query: &str, fields: &[&str]) -> Result<Value> {
        let query = format!("query={query}");
        let mut args = vec!["api", "graphql", "--hostname", host, "-f", &query];
        for field in fields {
            args.extend(["-f", field]);
        }
        data(self.call(&args)?)
    }

    fn host(&self, intent: &NoticeIntent) -> Result<String> {
        if self.policy.offline {
            return Err("publishing is unavailable in offline mode".into());
        }
        let remote = checked(git(
            self.policy,
            self.repo,
            &["remote", "get-url", "--push", "--all", "origin"],
        ))?;
        if remote.lines().count() != 1 {
            return Err("publishing requires one origin push URL".into());
        }
        let origin = self.call(&["repo", "view", &remote, "--json", "id,nameWithOwner,url"])?;
        if origin["id"].as_str() != Some(&intent.rejection.repository_id) {
            return Err("landing reservation does not match publication origin".into());
        }
        let host = repository(&origin)?.host;
        let viewer = self.graphql(&host, "query{viewer{id}}", &[])?;
        if viewer["viewer"]["id"].as_str() != Some(&intent.publisher_id) {
            return Err("landing reservation does not match the publisher identity".into());
        }
        Ok(host)
    }

    pub fn notice(&self, intent: &NoticeIntent, failed_tests: &[&str]) -> Result<DeliveryReceipt> {
        let body = intent.body(failed_tests)?;
        let host = self.host(intent)?;
        let result = self.graphql(
            &host,
            ADD_COMMENT,
            &[
                &format!("pr={}", intent.rejection.pull_request_id),
                &format!("body={body}"),
            ],
        )?;
        delivery(intent, &result["addComment"]["commentEdge"]["node"])
    }

    /// A queue acknowledgment may precede assignment of a merge group. In
    /// that case the owner journal retains the pending reservation.
    pub fn enqueue(&self, intent: &RetryIntent) -> Result<Admission> {
        let rejection = &intent.delivery.intent.rejection;
        rejection.check()?;
        if !is_node_id(&intent.delivery.comment_id)
            || intent.delivery.created_at <= rejection.removed_at
            || intent.observed_at < intent.delivery.created_at
            || intent.observed_at.saturating_sub(intent.last_admission_at) < 1800
            || intent.last_admission_at >= rejection.removed_at
            || !is_object_id(&intent.observed_main)
        {
            return Err("invalid landing retry reservation".into());
        }
        let host = self.host(&intent.delivery.intent)?;
        let result = self.graphql(
            &host,
            ENQUEUE,
            &[
                &format!("pr={}", rejection.pull_request_id),
                &format!("head={}", rejection.pr_head),
            ],
        )?;
        admission(intent, &result["enqueuePullRequest"]["mergeQueueEntry"])
    }
}

fn data(response: Value) -> Result<Value> {
    if response.get("errors").is_some() {
        return Err("GitHub landing publication response contained errors".into());
    }
    match response {
        Value::Object(mut fields) => fields.remove("data").filter(Value::is_object),
        _ => None,
    }
    .ok_or_else(|| "missing GitHub landing publication data".into())
}

fn receipt_id(value: &Value) -> Result<String> {
    value
        .as_str()
        .filter(|id| is_node_id(id))
        .map(str::to_owned)
        .ok_or_else(|| "missing GitHub landing receipt identity".into())
}

fn timestamp(value: &Value) -> Option<u64> {
    oxa_session::parse_utc_seconds(value.as_str()?)
}

fn delivery(intent: &NoticeIntent, node: &Value) -> Result<DeliveryReceipt> {
    if node["repository"]["id"].as_str() != Some(&intent.rejection.repository_id)
        || node["pullRequest"]["id"].as_str() != Some(&intent.rejection.pull_request_id)
        || node["pullRequest"]["headRefOid"].as_str() != Some(&intent.rejection.pr_head)
        || node["pullRequest"]["author"]["id"].as_str() != Some(&intent.recipient_id)
        || node["pullRequest"]["author"]["login"].as_str() != Some(&intent.recipient_login)
        || !matches!(node["author"]["__typename"].as_str(), Some("User" | "Bot"))
        || node["author"]["id"].as_str() != Some(&intent.publisher_id)
    {
        return Err("delivery receipt does not match the landing reservation".into());
    }
    Ok(DeliveryReceipt {
        intent: intent.clone(),
        comment_id: receipt_id(&node["id"])?,
        created_at: timestamp(&node["createdAt"])
            .filter(|time| *time > intent.rejection.removed_at)
            .ok_or("invalid delivery receipt timestamp")?,
    })
}

fn admission(intent: &RetryIntent, entry: &Value) -> Result<Admission> {
    let rejection = &intent.delivery.intent.rejection;
    if entry["mergeQueue"]["repository"]["id"].as_str() != Some(&rejection.repository_id)
        || entry["mergeQueue"]["id"].as_str() != Some(&rejection.queue_id)
        || entry["pullRequest"]["id"].as_str() != Some(&rejection.pull_request_id)
        || entry["pullRequest"]["headRefOid"].as_str() != Some(&rejection.pr_head)
        || entry["pullRequest"]["number"].as_u64() != Some(rejection.pull_request_number)
        || !matches!(entry["enqueuer"]["__typename"].as_str(), Some("User" | "Bot"))
        || entry["enqueuer"]["id"].as_str() != Some(&intent.delivery.intent.publisher_id)
    {
        return Err("admission acknowledgment does not match the landing reservation".into());
    }
    let queue_entry_id = receipt_id(&entry["id"])?;
    let enqueued_at = timestamp(&entry["enqueuedAt"])
        .filter(|time| *time >= intent.observed_at && *time >= intent.delivery.created_at)
        .ok_or("invalid landing admission timestamp")?;
    let acknowledgment = AdmissionAcknowledgment {
        intent: intent.clone(),
        queue_entry_id,
        enqueued_at,
    };
    if entry.get("headCommit") == Some(&Value::Null) {
        return Ok(Admission::Pending(acknowledgment));
    }
    let group = entry["headCommit"]["oid"]
        .as_str()
        .filter(|group| {
            is_object_id(group) && *group != rejection.pr_head && *group != rejection.rejected_group
        })
        .ok_or("invalid newly admitted merge-group commit")?;
    Ok(Admission::Confirmed(AdmissionReceipt {
        acknowledgment,
        admitted_group: group.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_intent() -> NoticeIntent {
        NoticeIntent {
            rejection: RejectedLanding {
                repository_id: "R_repo".into(),
                queue_id: "MQ_main".into(),
                pull_request_id: "PR_pull".into(),
                pull_request_number: 42,
                pr_head: "a".repeat(40),
                rejected_group: "b".repeat(40),
                removal_id: "E_removed".into(),
                removed_at: 3_500,
            },
            recipient_id: "U_author".into(),
            recipient_login: "synthetic-author".into(),
            publisher_id: "U_publisher".into(),
        }
    }

    #[test]
    fn notice_uses_the_canonical_transport_on_a_mock_executable() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = crate::test_dir::PrivateDir::new("landing-transport");
        let git = temp.path().join("git");
        let gh = temp.path().join("gh");
        fs::write(
            &git,
            "#!/bin/sh\nprintf '%s\\n' https://example.invalid/sample/repo\n",
        )
        .unwrap();
        fs::write(&gh, r#"#!/bin/sh
test -z "${GH_TOKEN+x}" || exit 1
case "$*" in
  'repo view https://example.invalid/sample/repo --json id,nameWithOwner,url')
    printf '%s\n' '{"id":"R_repo","nameWithOwner":"sample/repo","url":"https://example.invalid/sample/repo"}' ;;
  'api graphql --hostname example.invalid -f query=query{viewer{id}}')
    printf '%s\n' '{"data":{"viewer":{"id":"U_publisher"}}}' ;;
  'api graphql --hostname example.invalid -f query=mutation('*' pr=PR_pull -f body=@synthetic-author '*)
    printf '%s\n' '{"data":{"addComment":{"commentEdge":{"node":{"id":"IC_comment",
        "repository":{"id":"R_repo"},"pullRequest":{"id":"PR_pull","author":{"id":"U_author","login":"synthetic-author"},
        "headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
        "author":{"__typename":"User","id":"U_publisher"},"createdAt":"1970-01-01T01:00:00Z"}}}}}'
    ;;
  *) exit 1 ;;
esac
"#).unwrap();
        for binary in [&git, &gh] {
            fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let policy = RunPolicy {
            git_binary: git,
            host_path: Default::default(),
            terminate_binary: None,
            offline: false,
            runner_owns_github: false,
            storage: temp.path().to_owned(),
        };
        let log = crate::shadow::open_log(&temp.path().join("owner.jsonl")).unwrap();
        let publisher = LandingPublisher::new(temp.path(), &policy, &gh, &log);
        let receipt = publisher.notice(&test_intent(), &["synthetic::test"]).unwrap();
        assert_eq!(receipt.comment_id, "IC_comment");
    }

    #[test]
    fn typed_landing_receipts_refuse_partial_and_mismatched_evidence() {
        let intent = test_intent();
        let body = intent.body(&["suite::test`@unrelated"]).unwrap();
        assert!(body.starts_with("@synthetic-author "));
        assert!(body.ends_with("`suite::test’@unrelated`"));
        assert!(intent.body(&["bad\nname"]).is_err());
        let mut node = json!({
            "id":"IC_comment","repository":{"id":"R_repo"},
            "pullRequest":{"id":"PR_pull","headRefOid":intent.rejection.pr_head,
                "author":{"id":"U_author","login":"synthetic-author"}},
            "author":{"__typename":"User","id":"U_publisher"},"createdAt":"1970-01-01T01:00:00Z"
        });
        let delivered = delivery(&intent, &node).unwrap();
        node["pullRequest"]["author"]["id"] = json!("U_wrong_recipient");
        assert!(delivery(&intent, &node).is_err());
        node["pullRequest"]["author"]["id"] = json!("U_author");
        node["author"]["id"] = json!("U_unrelated");
        assert!(delivery(&intent, &node).is_err());
        assert!(data(json!({"data":{"addComment": node},"errors":[{}]})).is_err());

        let retry = RetryIntent {
            delivery: delivered,
            observed_main: "c".repeat(40),
            observed_at: 3_600,
            last_admission_at: 1_000,
        };
        let mut entry = json!({
            "id":"MQE_entry","enqueuedAt":"1970-01-01T01:00:00Z",
            "mergeQueue":{"id":"MQ_main","repository":{"id":"R_repo"}},
            "pullRequest":{"id":"PR_pull","number":42,"headRefOid":intent.rejection.pr_head},
            "enqueuer":{"__typename":"User","id":"U_publisher"},
            "headCommit":null
        });
        let Admission::Pending(pending) = admission(&retry, &entry).unwrap() else {
            panic!("a missing group cannot certify admission");
        };
        entry["headCommit"] = json!({"oid": "d".repeat(40)});
        let Admission::Confirmed(admitted) = pending.reconcile(&entry).unwrap() else {
            panic!("a matching acknowledged group must certify admission");
        };
        assert_eq!(admitted.admitted_group, "d".repeat(40));
        entry["id"] = json!("MQE_unrelated");
        assert!(pending.reconcile(&entry).is_err());
        entry["mergeQueue"]["id"] = json!("MQ_other");
        assert!(admission(&retry, &entry).is_err());
    }
}
