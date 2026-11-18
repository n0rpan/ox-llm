use super::{is_node_id, is_object_id, read, Source, Trigger, Verified};
use serde_json::Value;

/// An exact PR selected by the protected runner, with its current check commit.
pub struct Current<'a> {
    pub repository: &'a str,
    pub repository_id: &'a str,
    pub number: u64,
    pub pr_head: &'a str,
    pub head: &'a str,
    pub workflow_path: &'a str,
    pub required_name: &'a str,
    pub trigger: Trigger,
}

impl Current<'_> {
    /// Keep the existing Reader's byte, deadline and authentication contract.
    /// No result can survive a PR, merge-group, or check change during REST acquisition.
    pub fn observe<T>(
        &self,
        mut repository_read: impl FnMut(&str) -> Result<Value, String>,
        actions_json: impl FnMut(&str) -> Result<Value, String>,
        consume: impl FnOnce(Option<Verified<'_>>) -> T,
    ) -> Result<T, String> {
        let Some(fields) = self.fields() else {
            return Ok(consume(None));
        };
        let first = repository_read(&fields)?;
        let Some(source) = self.exact_check(&first) else {
            return Ok(consume(None));
        };
        let Some(verified) = read(source, actions_json)? else {
            return Ok(consume(None));
        };
        let last = repository_read(&fields)?;
        let stable = self
            .exact_check(&last)
            .is_some_and(|current| current.check == source.check);
        Ok(consume(stable.then_some(verified)))
    }

    /// The enclosing GraphQL repository and all HTTP calls belong to the same Reader.
    pub fn fields(&self) -> Option<String> {
        if self.number == 0
            || !is_node_id(self.repository_id)
            || !is_object_id(self.pr_head)
            || !is_object_id(self.head)
        {
            return None;
        }
        let number = self.number;
        let head = self.head;
        Some(format!(
            r#"id nameWithOwner
            pr:pullRequest(number:{number}){{
                number state headRefOid mergeQueueEntry{{headCommit{{oid}}}}
            }}
            checked:object(oid:{head:?}){{... on Commit{{oid statusCheckRollup{{commit{{oid}}
                contexts(first:100){{totalCount nodes{{__typename ... on CheckRun{{
                    databaseId name isRequired(pullRequestNumber:{number}) status conclusion completedAt
                    checkSuite{{databaseId commit{{oid}} workflowRun{{databaseId event runAttempt}}}}
                }}}}}}
            }}}}}}"#
        ))
    }

    pub fn exact_check<'a>(&'a self, repo: &'a Value) -> Option<Source<'a>> {
        if repo["id"].as_str()? != self.repository_id
            || !repo["nameWithOwner"]
                .as_str()?
                .eq_ignore_ascii_case(self.repository)
        {
            return None;
        }
        let pr = &repo["pr"];
        if pr["number"].as_u64()? != self.number || pr["state"] != "OPEN" || pr["headRefOid"] != self.pr_head
        {
            return None;
        }
        let current = match self.trigger {
            Trigger::PullRequest => self.pr_head,
            Trigger::MergeGroup => pr["mergeQueueEntry"]["headCommit"]["oid"].as_str()?,
        };
        let checked = &repo["checked"];
        let rollup = &checked["statusCheckRollup"];
        if current != self.head || checked["oid"] != current || rollup["commit"]["oid"] != current {
            return None;
        }
        let contexts = &rollup["contexts"];
        let mut matches = contexts["nodes"].as_array()?.iter();
        if matches.len() > 100 || contexts["totalCount"].as_u64()? != matches.len() as u64 {
            return None;
        }
        let matches_required = |row: &&Value| {
            row["__typename"] == "CheckRun" && row["isRequired"] == true && row["name"] == self.required_name
        };
        let check = matches.find(matches_required)?;
        if matches.any(|row| matches_required(&row)) {
            return None;
        }
        Some(Source {
            repository: repo["nameWithOwner"].as_str()?,
            repository_id: self.repository_id,
            head: current,
            workflow_path: self.workflow_path,
            required_name: self.required_name,
            trigger: self.trigger,
            check,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_group_and_required_context_must_match_the_same_repository_and_pr() {
        let pr_head = "a".repeat(40);
        let group = "b".repeat(40);
        let target = Current {
            repository: "example/project",
            repository_id: "R_example",
            number: 42,
            pr_head: &pr_head,
            head: &group,
            workflow_path: ".github/workflows/checks.yml",
            required_name: "gate",
            trigger: Trigger::MergeGroup,
        };
        let check = json!({
            "__typename":"CheckRun","name":"gate","isRequired":true,
            "databaseId":31,"status":"IN_PROGRESS","conclusion":"SUCCESS",
            "completedAt":"1970-01-02T00:00:00Z",
            "checkSuite":{"databaseId":19,"commit":{"oid":group},
                "workflowRun":{"databaseId":12,"runAttempt":2,"event":"merge_group"}}
        });
        let repo = json!({
            "id":"R_example", "nameWithOwner":"example/project",
            "pr":{"number":42,"state":"OPEN","headRefOid":pr_head,
                "mergeQueueEntry":{"headCommit":{"oid":group}}},
            "checked":{"oid":group,"statusCheckRollup":{"commit":{"oid":group},
                "contexts":{"totalCount":1,"nodes":[check]}}}
        });
        let fields = target.fields().unwrap();
        assert!(fields.contains("isRequired(pullRequestNumber:42)"));
        assert_eq!(fields.matches('{').count(), fields.matches('}').count());
        assert_eq!(target.exact_check(&repo).unwrap().head, group);

        for (path, value) in [
            ("/id", json!("R_other")),
            ("/pr/headRefOid", json!("c".repeat(40))),
            ("/pr/state", json!("CLOSED")),
            ("/pr/mergeQueueEntry", Value::Null),
            ("/checked/statusCheckRollup/commit/oid", json!("c".repeat(40))),
            ("/checked/statusCheckRollup/contexts/totalCount", json!(2)),
            (
                "/checked/statusCheckRollup/contexts/nodes/0/isRequired",
                json!(false),
            ),
        ] {
            let mut changed = repo.clone();
            *changed.pointer_mut(path).unwrap() = value;
            assert!(target.exact_check(&changed).is_none(), "{path}");
        }

        for changed in [false, true] {
            let mut calls = 0;
            let found = target
                .observe(
                    |_| {
                        calls += 1;
                        let mut sample = repo.clone();
                        if changed && calls == 2 {
                            sample["checked"]["statusCheckRollup"]["contexts"]["nodes"][0]["status"] =
                                "COMPLETED".into();
                        }
                        Ok(sample)
                    },
                    |address| match address {
                        "runs/12/attempts/2" => Ok(json!({
                            "id":12,"check_suite_id":19,"head_sha":group,"run_attempt":2,
                            "event":"merge_group","path":".github/workflows/checks.yml"
                        })),
                        "runs/12/attempts/2/jobs?per_page=100" => Ok(json!({
                            "total_count":1,"jobs":[{
                                "id":23,"run_id":12,"run_attempt":2,"head_sha":group,"name":"gate",
                                "check_run_url":"https://api.github.com/repos/example/project/check-runs/31"
                            }]
                        })),
                        _ => panic!("unexpected Actions address"),
                    },
                    |verified| verified.map(|checked| checked.attempt.job_id),
                )
                .unwrap();
            assert_eq!(calls, 2);
            assert_eq!(found, (!changed).then_some(23));
        }
    }
}
