//! One read-only GraphQL projection for main CI, queue membership and a selected release issue.
//! No scheduler decisions are inferred here. Checkbox states are reported, not validated.
use oxa_proto::work::{PullRequest, MAX_ITEMS};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde_json::Value;

type Result<T> = std::result::Result<T, &'static str>;

pub struct Panels {
    /// True only when one required check fails at the observed main head. Optional
    /// job failures, pending states, and absent or unobserved required checks stay
    /// quiet, so an optional macOS failure never pages the owner (#1634).
    pub main_red: bool,
    /// Unix seconds, only when the observed main head is itself a verified merge.
    main_merged_at: Option<u64>,
    /// None means GitHub did not supply a merge queue; Some(empty) is an observed empty queue.
    pub queue: Option<Vec<PullRequest>>,
    pub release: Option<(u64, Vec<(bool, String)>)>,
}

impl Panels {
    /// The verified main merge time at the observed head, when the head is itself
    /// a verified merge. `None` means no verified merge establishes a stall age.
    pub fn main_merged_at(&self) -> Option<u64> {
        self.main_merged_at
    }

    /// Compare with the source observation time, so an old sample cannot become a new alarm.
    pub fn main_merge_stalled(&self, observed_at: u64) -> bool {
        self.main_merged_at
            .and_then(|merged_at| observed_at.checked_sub(merged_at))
            .is_some_and(|age| age >= 45 * 60)
    }
}

/// Repository fields for one refresh, wrapped by the caller's own bounded
/// GraphQL transport. The lead Reader sends one query per sample, never a per-PR loop.
pub fn fields(release_issue: Option<u64>) -> Result<String> {
    if release_issue == Some(0) {
        return Err("release issue must be positive");
    }
    let release = release_issue.map_or(String::new(), |number| {
        format!("issue(number:{number}){{number body}}")
    });
    let main = r#"main:ref(qualifiedName:"refs/heads/main"){
        refUpdateRule{requiredStatusCheckContexts}
        rules(first:100){totalCount nodes{type parameters{... on RequiredStatusChecksParameters{requiredStatusChecks{context}}}}}
        target{... on Commit{
        oid statusCheckRollup{contexts(first:100){pageInfo{hasNextPage} nodes{
            ... on CheckRun{name status conclusion}
            ... on StatusContext{context state}
        }}}
        associatedPullRequests(first:10){pageInfo{hasNextPage} nodes{repository{id} baseRefName mergedAt mergeCommit{oid}}}
    }}}"#;
    Ok(format!(
        "id \
         {main}\
         mergeQueue(branch:\"main\"){{entries(first:100){{totalCount nodes{{pullRequest{{number headRefOid}}}}}}}}\
         {release}"
    ))
}

/// One complete query for a caller that owns its own transport validation.
pub fn query(repo: &str, release_issue: Option<u64>) -> Result<String> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || p.len() > 100
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err("expected OWNER/REPO");
    }
    Ok(format!(
        "query{{repository(owner:{:?},name:{:?}){{{}}}}}",
        parts[0],
        parts[1],
        fields(release_issue)?
    ))
}

/// Fixed diagnostics only. Arbitrary body/header/token/debug fields never reach the view.
pub fn parse(bytes: &[u8], release_issue: Option<u64>) -> Result<Panels> {
    if bytes.len() > 1_048_576 {
        return Err("GitHub panels exceed size limit");
    }
    let response: Value = serde_json::from_slice(bytes).map_err(|_| "invalid GitHub panels")?;
    if response.get("errors").is_some() {
        return Err("incomplete GitHub panels");
    }
    let repo = response
        .pointer("/data/repository")
        .filter(|v| v.is_object())
        .ok_or("missing GitHub repository")?;
    parse_repository(repo, release_issue)
}

/// The repository object from the shared bounded Reader transport; the caller has
/// already refused a GraphQL error response and the shared byte bound.
pub fn parse_repository(repo: &Value, release_issue: Option<u64>) -> Result<Panels> {
    let queue = if repo["mergeQueue"].is_null() {
        None
    } else {
        let entries = &repo["mergeQueue"]["entries"];
        let nodes = entries["nodes"].as_array().ok_or("missing queue entries")?;
        if entries["totalCount"].as_u64() != Some(nodes.len() as u64) || nodes.len() > MAX_ITEMS {
            return Err("incomplete merge queue");
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut queue = Vec::new();
        for node in nodes {
            let pr = &node["pullRequest"];
            let number = pr["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("invalid queued PR")?;
            let head = pr["headRefOid"]
                .as_str()
                .filter(|s| is_git_oid(s))
                .ok_or("invalid queue commit")?;
            if !seen.insert(number) {
                return Err("duplicate queue entry");
            }
            queue.push(PullRequest {
                number,
                head: head.into(),
            });
        }
        Some(queue)
    };
    let release = if let Some(number) = release_issue {
        if repo["issue"].is_null() {
            None
        } else {
            if repo["issue"]["number"].as_u64() != Some(number) {
                return Err("release issue mismatch");
            }
            let body = repo["issue"]["body"]
                .as_str()
                .ok_or("missing release issue body")?;
            Some((number, checklist(body)?))
        }
    } else {
        None
    };
    let main_red = main_required_failure(repo)?;
    Ok(Panels {
        main_red,
        main_merged_at: verified_main_merge_time(repo),
        queue,
        release,
    })
}

/// Exact-main required-check triage (#1634): an alarm needs one failed required
/// check at the observed main head. A green or pending required check with any
/// number of failing optional jobs stays quiet, and so do unknown or absent
/// required checks, a missing rollup, and a rollup beyond the bounded page.
fn main_required_failure(repo: &Value) -> Result<bool> {
    let main = &repo["main"];
    let required =
        crate::work_board::required_contexts(main).map_err(|_| "incomplete main check requirements")?;
    if required.is_empty() {
        return Ok(false);
    }
    let contexts = &main["target"]["statusCheckRollup"]["contexts"];
    if contexts.is_null() {
        return Ok(false);
    }
    let rows = contexts["nodes"]
        .as_array()
        .ok_or("missing main rollup contexts")?;
    if contexts["pageInfo"]["hasNextPage"].as_bool() == Some(true) {
        return Ok(false);
    }
    Ok(rows.iter().any(|row| {
        row["name"]
            .as_str()
            .or(row["context"].as_str())
            .is_some_and(|name| required.contains(name))
            && matches!(
                crate::work_board::check_outcome(row),
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
    }))
}

fn is_git_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn verified_main_merge_time(repo: &Value) -> Option<u64> {
    let repo_id = repo["id"].as_str().filter(|s| !s.is_empty())?;
    let target = &repo["main"]["target"];
    let head = target["oid"].as_str().filter(|s| is_git_oid(s))?;
    let prs = &target["associatedPullRequests"];
    if prs["pageInfo"]["hasNextPage"].as_bool() != Some(false) {
        return None;
    }
    let nodes = prs["nodes"].as_array().filter(|nodes| nodes.len() <= 10)?;
    let mut matches = nodes.iter().filter(|pr| {
        pr["repository"]["id"].as_str() == Some(repo_id)
            && pr["baseRefName"].as_str() == Some("main")
            && pr["mergeCommit"]["oid"].as_str() == Some(head)
    });
    let time = oxa_session::parse_utc_seconds(matches.next()?["mergedAt"].as_str()?)?;
    matches.next().is_none().then_some(time)
}

fn label(items: &mut [(bool, String)], stack: &[(Option<usize>, bool)], text: &str) -> Result<()> {
    if let Some((Some(index), true)) = stack.last() {
        let label = &mut items[*index].1;
        if label.len().saturating_add(text.len()) > 1024 {
            return Err("release checklist exceeds bounds");
        }
        label.push_str(text);
    }
    Ok(())
}

fn checklist(body: &str) -> Result<Vec<(bool, String)>> {
    let mut items: Vec<(bool, String)> = Vec::new();
    let mut stack = Vec::new();
    for event in Parser::new_ext(body, Options::ENABLE_TASKLISTS) {
        match event {
            Event::Start(Tag::Item) => stack.push((None, true)),
            Event::TaskListMarker(checked) => {
                if items.len() == MAX_ITEMS {
                    return Err("release checklist exceeds bounds");
                }
                let item = stack.last_mut().ok_or("invalid checklist context")?;
                item.0 = Some(items.len());
                items.push((checked, String::new()));
            }
            Event::End(TagEnd::Item) => {
                if let Some((Some(index), _)) = stack.pop() {
                    let text = &mut items[index].1;
                    *text = text.trim().to_owned();
                    if text.is_empty() {
                        return Err("release checklist exceeds bounds");
                    }
                }
            }
            // Only the item's first paragraph labels its checkbox. Nested items have
            // their own stack entry; code, HTML, later prose and URLs are never labels.
            Event::End(TagEnd::Paragraph)
            | Event::Start(
                Tag::List(_) | Tag::CodeBlock(_) | Tag::HtmlBlock | Tag::BlockQuote(_) | Tag::Heading { .. },
            ) => {
                if let Some(item) = stack.last_mut() {
                    item.1 = false;
                }
            }
            Event::Text(text) | Event::Code(text) => label(&mut items, &stack, &text)?,
            Event::SoftBreak | Event::HardBreak => label(&mut items, &stack, " ")?,
            _ => {}
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn response() -> Value {
        json!({"data":{"repository":{
            "main":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},
            "mergeQueue":{"entries":{"totalCount":2,"nodes":[
                {"pullRequest":{"number":17,"headRefOid":"a".repeat(40)}},
                {"pullRequest":{"number":18,"headRefOid":"b".repeat(40)}}
            ]}},
            "issue":{"number":19,"body":"Private overview is not a checklist\n- [x] Merged with evidence\n- [ ] Hardware check"},
            "unrelated_private_field":"synthetic marker"
        }}})
    }
    #[test]
    fn selects_ordered_queue_and_only_reported_checkboxes() {
        let panels = parse(&serde_json::to_vec(&response()).unwrap(), Some(19)).unwrap();
        let queue = panels.queue.unwrap();
        assert_eq!(queue.iter().map(|p| p.number).collect::<Vec<_>>(), [17, 18]);
        assert_eq!(
            panels.release.unwrap(),
            (
                19,
                vec![
                    (true, "Merged with evidence".into()),
                    (false, "Hardware check".into()),
                ],
            )
        );
    }
    #[test]
    fn checklist_ignores_fenced_code_examples() {
        let mut data = response();
        data["data"]["repository"]["issue"]["body"] = json!("- [ ] Actual task\n```markdown\n- [x] Example task\n```\n~~~~\n- [x] Another example\n~~~\n- [x] Still fenced\n~~~~");
        assert_eq!(
            parse(&serde_json::to_vec(&data).unwrap(), Some(19))
                .unwrap()
                .release
                .unwrap()
                .1,
            [(false, "Actual task".into())]
        );
    }
    #[test]
    fn checklist_recognizes_all_bullet_markers() {
        let mut data = response();
        data["data"]["repository"]["issue"]["body"] =
            json!("* [x] First task\n+ [ ] Second task\n- [X] Third task");
        assert_eq!(
            parse(&serde_json::to_vec(&data).unwrap(), Some(19))
                .unwrap()
                .release
                .unwrap()
                .1,
            [
                (true, "First task".into()),
                (false, "Second task".into()),
                (true, "Third task".into())
            ]
        );
    }
    #[test]
    fn markdown_context_keeps_nested_multiline_labels_and_omits_examples_and_urls() {
        let body = "    - [x] Indented code example\n\n<!--\n- [x] Hidden example\n-->\n\n1. [ ] **Parent** task\n   continuing here\n   - [x] Child `gate` [label](https://example.invalid/private-marker)\n\n   Later prose is not a checkbox label.\n";
        assert_eq!(
            checklist(body).unwrap(),
            [
                (false, "Parent task continuing here".into()),
                (true, "Child gate label".into())
            ]
        );
        assert!(checklist(&format!("- [ ] {}", "a".repeat(1025))).is_err());
        assert!(checklist(&"- [ ] Task\n".repeat(MAX_ITEMS + 1)).is_err());
    }
    #[test]
    fn refuses_truncation_errors_wrong_issue_and_missing_fields() {
        for (pointer, replacement) in [
            ("/data/repository/mergeQueue/entries/totalCount", json!(999)),
            ("/data/repository/issue/number", json!(999)),
            (
                "/data/repository/mergeQueue/entries/nodes/0/pullRequest/number",
                Value::Null,
            ),
        ] {
            let mut data = response();
            *data.pointer_mut(pointer).unwrap() = replacement;
            assert!(parse(&serde_json::to_vec(&data).unwrap(), Some(19)).is_err());
        }
        assert!(parse(br#"{"errors":[{"message":"private synthetic detail"}]}"#, None).is_err());
        assert!(parse(&vec![b'x'; 1_048_577], None).is_err());
    }
    #[test]
    fn distinguishes_unavailable_and_empty_queue_and_validates_query_inputs() {
        // The main projection is part of every record the Reader captures; a
        // response without it is refused, not read as a healthy main.
        let absent = br#"{"data":{"repository":{"main":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},"mergeQueue":null}}}"#;
        assert!(parse(absent, None).unwrap().queue.is_none());
        let empty = br#"{"data":{"repository":{"main":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},"mergeQueue":{"entries":{"totalCount":0,"nodes":[]}}}}}"#;
        assert!(parse(empty, None).unwrap().queue.unwrap().is_empty());
        let query_text = query("example/project", Some(19)).unwrap();
        for field in [
            "ref(qualifiedName:\"refs/heads/main\")",
            "refUpdateRule{requiredStatusCheckContexts}",
            "statusCheckRollup{contexts(first:100)",
            "on CheckRun{name status conclusion}",
            "on StatusContext{context state}",
            "nodes{repository{id} baseRefName mergedAt mergeCommit{oid}}",
            "mergeQueue(branch:\"main\")",
            "issue(number:19)",
        ] {
            assert!(query_text.contains(field));
        }
        assert_eq!(
            query("example/project", None).unwrap(),
            format!(
                "query{{repository(owner:{:?},name:{:?}){{{}}}}}",
                "example",
                "project",
                fields(None).unwrap()
            ),
            "fields() builds the same projection for the shared Reader transport"
        );
        for repo in ["example", "example/project/extra", "example/\"injection"] {
            assert!(query(repo, None).is_err());
        }
        assert!(query("example/project", Some(0)).is_err());
        assert!(fields(Some(0)).is_err());
    }

    #[test]
    fn main_alarm_pages_only_on_a_failed_required_check_at_the_observed_head() {
        let head = "c".repeat(40);
        let base = |contexts: Value, required: Value| {
            json!({"data":{"repository":{
                "id":"example-repo",
                "main":{
                    "refUpdateRule":required,
                    "rules":{"totalCount":0,"nodes":[]},
                    "target":{"oid":head, "statusCheckRollup":{"contexts":contexts}}
                },
                "mergeQueue":null
            }}})
        };
        let rollup = |rows: &[Value]| json!({"pageInfo":{"hasNextPage":false}, "nodes":rows});
        let check_run = |name: &str, status: &str, conclusion: &str| json!({"name":name, "status":status, "conclusion":conclusion});
        let classic = json!({"requiredStatusCheckContexts":["gate"]});
        // A green required check with any number of failing optional jobs stays quiet.
        let optional_failure = check_run("macos", "COMPLETED", "FAILURE");
        let green = [
            check_run("gate", "COMPLETED", "SUCCESS"),
            optional_failure.clone(),
        ];
        assert!(
            !parse(
                &serde_json::to_vec(&base(rollup(&green), classic.clone())).unwrap(),
                None
            )
            .unwrap()
            .main_red
        );
        // A pending required check with a failing optional job stays quiet too.
        let pending = [
            check_run("gate", "in_progress", "FAILURE"),
            optional_failure.clone(),
        ];
        assert!(
            !parse(
                &serde_json::to_vec(&base(rollup(&pending), classic.clone())).unwrap(),
                None
            )
            .unwrap()
            .main_red
        );
        // A true required failure is the only paging state.
        let failed = [check_run("gate", "COMPLETED", "FAILURE"), optional_failure];
        assert!(
            parse(
                &serde_json::to_vec(&base(rollup(&failed), classic)).unwrap(),
                None
            )
            .unwrap()
            .main_red
        );
        // A classic status context reports its own state, and optional-only failures
        // never alarm when no required check is configured at all.
        let context_failure = json!({"context":"ci", "state":"FAILURE"});
        let contexts_only = [context_failure];
        assert!(
            parse(
                &serde_json::to_vec(&base(
                    rollup(&contexts_only),
                    json!({"requiredStatusCheckContexts":["ci"]})
                ))
                .unwrap(),
                None
            )
            .unwrap()
            .main_red
        );
        for required in [Value::Null, json!({"requiredStatusCheckContexts":[]})] {
            assert!(
                !parse(
                    &serde_json::to_vec(&base(rollup(&contexts_only), required)).unwrap(),
                    None
                )
                .unwrap()
                .main_red,
                "unknown or absent required checks stay pending"
            );
        }
        // A rollup that exceeds its bounded page, a missing rollup and a required
        // check absent from the rollup all stay pending, never alarm and never error.
        let beyond_page = json!({"pageInfo":{"hasNextPage":true}, "nodes":[
            check_run("gate", "COMPLETED", "FAILURE")
        ]});
        assert!(
            !parse(
                &serde_json::to_vec(&base(
                    beyond_page,
                    json!({"requiredStatusCheckContexts":["gate"]})
                ))
                .unwrap(),
                None
            )
            .unwrap()
            .main_red
        );
        let missing_rollup = json!({"target":{"oid":head}});
        let mut no_rollup = base(json!(null), json!({"requiredStatusCheckContexts":["gate"]}));
        no_rollup["data"]["repository"]["main"]["target"] = missing_rollup;
        assert!(
            !parse(&serde_json::to_vec(&no_rollup).unwrap(), None)
                .unwrap()
                .main_red
        );
        let absent_required = [check_run("macos", "COMPLETED", "FAILURE")];
        assert!(
            !parse(
                &serde_json::to_vec(&base(
                    rollup(&absent_required),
                    json!({"requiredStatusCheckContexts":["gate"]})
                ))
                .unwrap(),
                None
            )
            .unwrap()
            .main_red
        );
        // Required checks may also arrive through an active ruleset.
        let ruleset = json!({"requiredStatusCheckContexts":null});
        let mut with_ruleset = base(rollup(&failed), ruleset);
        with_ruleset["data"]["repository"]["main"]["refUpdateRule"] = Value::Null;
        with_ruleset["data"]["repository"]["main"]["rules"] = json!({
            "totalCount":1, "nodes":[{"type":"REQUIRED_STATUS_CHECKS",
            "parameters":{"requiredStatusChecks":[{"context":"gate"}]}}]
        });
        assert!(
            parse(&serde_json::to_vec(&with_ruleset).unwrap(), None)
                .unwrap()
                .main_red
        );
    }
}
