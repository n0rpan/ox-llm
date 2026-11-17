//! One read-only GraphQL projection for queue membership and a selected release issue.
//! No scheduler decisions are inferred here. Checkbox states are reported, not validated.
use oxa_proto::work::{PullRequest, MAX_ITEMS};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde_json::Value;

type Result<T> = std::result::Result<T, &'static str>;

pub struct Panels {
    /// None means GitHub did not supply a merge queue; Some(empty) is an observed empty queue.
    pub queue: Option<Vec<PullRequest>>,
    pub release: Option<(u64, Vec<(bool, String)>)>,
}

/// The adapter sends one query per refresh, never a per-PR polling loop.
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
    if release_issue == Some(0) {
        return Err("release issue must be positive");
    }
    let release = release_issue.map_or(String::new(), |number| {
        format!("issue(number:{number}){{number body}}")
    });
    Ok(format!("query{{repository(owner:{:?},name:{:?}){{mergeQueue{{entries(first:100){{totalCount nodes{{pullRequest{{number headRefOid}}}}}}}}{release}}}}}", parts[0], parts[1]))
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
                .filter(|s| {
                    matches!(s.len(), 40 | 64)
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
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
    Ok(Panels { queue, release })
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
        let absent = br#"{"data":{"repository":{"mergeQueue":null}}}"#;
        assert!(parse(absent, None).unwrap().queue.is_none());
        let empty = br#"{"data":{"repository":{"mergeQueue":{"entries":{"totalCount":0,"nodes":[]}}}}}"#;
        assert!(parse(empty, None).unwrap().queue.unwrap().is_empty());
        assert!(query("example/project", Some(19))
            .unwrap()
            .contains("issue(number:19)"));
        for repo in ["example", "example/project/extra", "example/\"injection"] {
            assert!(query(repo, None).is_err());
        }
        assert!(query("example/project", Some(0)).is_err());
    }
}
