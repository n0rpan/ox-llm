//! Shadow-only observation of latest review holds through the shared Reader.
use super::{charge, nodes, Reader};
use oxa_work::board::{
    landing::{LatestReviews, Observation, Policy, Queue},
    queue::{is_node_id, is_object_id},
    review::{self, Comment, Findings},
    Action, Board,
};
use oxa_work::shadow::{LandingDecision, QueueObservation, ThreadHold};
use serde_json::Value;
use std::{collections::BTreeMap, fmt::Write};

pub(super) mod report;

const BATCH: usize = 8;

pub(super) fn review_decisions(
    reader: &Reader<'_>,
    board: &Board,
) -> Result<(BTreeMap<u64, LandingDecision>, Option<String>), String> {
    let entries: Vec<_> = board
        .entries
        .iter()
        .filter(|entry| {
            entry.evidence_incomplete.is_none()
                && !matches!(entry.action, Action::BuildIssue | Action::WaitIssue)
        })
        .collect();
    let mut observations = BTreeMap::new();
    let mut exact_holds = BTreeMap::new();
    let mut thread_holds = BTreeMap::new();
    let (queue, received) = reader.repository_read(
        r#"
        main:ref(qualifiedName:"refs/heads/main"){target{oid}}
        mergeQueue(branch:"main"){entries(first:100){totalCount nodes{
            pullRequest{number headRefOid} enqueuedAt headCommit{oid}
        }}}
    "#,
    )?;
    let mut observed = 0;
    charge(&mut observed, received)?;
    let main = oid(&queue["main"]["target"]["oid"]);
    let merge_queue = queue.get("mergeQueue").ok_or("missing merge queue observation")?;
    let mut queued = queue_observations(merge_queue, board)?;
    for batch in entries.chunks(BATCH) {
        let mut fields = String::new();
        for entry in batch {
            let n = entry.number;
            write!(
                fields,
                r#"p{n}:pullRequest(number:{n}){{
                    id number state headRefOid
                    autoMergeRequest{{enabledAt}} mergeQueueEntry{{id}}
                    comments(last:100){{totalCount nodes{{author{{... on Bot{{databaseId}}}} updatedAt body}}}}
                    reviewThreads(first:100){{totalCount nodes{{isResolved isOutdated comments(first:1){{nodes{{createdAt author{{login}}}}}}}}}}
                }}"#
            ).expect("writing to a String cannot fail");
        }
        let (data, size) = reader.repository_read(&fields)?;
        charge(&mut observed, size)?;
        for entry in batch {
            let row = &data[format!("p{}", entry.number)];
            if row["number"].as_u64() != Some(entry.number)
                || row["state"] != "OPEN"
                || row["headRefOid"].as_str() != Some(entry.head.as_str())
            {
                return Err("PR changed during review evidence read".into());
            }
            let comments = comments(&row["comments"]);
            let (findings, exact_hold) = review::latest_findings_for_head(comments.as_deref(), &entry.head);
            let threads = thread_hold(&row["reviewThreads"]);
            let thread_held = threads.as_ref().is_some_and(|hold| hold.current > 0);
            if let Some(id) = row["id"].as_str().filter(|_| exact_hold || thread_held) {
                let auto_merge = row["autoMergeRequest"]["enabledAt"]
                    .as_str()
                    .and_then(oxa_session::parse_utc_seconds)
                    .is_some();
                let queued = row["mergeQueueEntry"]["id"].as_str().is_some_and(is_node_id);
                exact_holds.insert(entry.number, (id.to_owned(), auto_merge, queued));
            }
            let reviews = reviews_with_threads(
                match findings {
                    Findings::Unresolved => LatestReviews::Unresolved,
                    Findings::Clear => LatestReviews::Clear,
                    Findings::Unknown => LatestReviews::Unknown,
                },
                threads.as_ref(),
            );
            thread_holds.insert(entry.number, threads);
            observations.insert(
                entry.number,
                Observation {
                    head: &entry.head,
                    reviews,
                    // Unobserved entries may have been rejected. The receipt journal
                    // must settle that before any enqueue/retry can be considered.
                    queue: queued
                        .get(&entry.number)
                        .map_or(Queue::Unknown, |row| Queue::Queued {
                            since: row.enqueued_at,
                        }),
                },
            );
        }
    }
    let landing = board
        .plan_landing(Policy { enabled: true }, &observations, main.unwrap_or_default())
        .into_iter()
        .map(|(number, plan)| {
            let hold_intervention = exact_holds.remove(&number).and_then(|(id, auto_merge, queued)| {
                plan.hold_intervention(&id, observations[&number].head, auto_merge, queued)
            });
            (
                number,
                LandingDecision {
                    next: plan.next,
                    queue_alarm: plan.queue_alarm,
                    hold_intervention,
                    queue: queued.remove(&number),
                    latest_reviews: observations[&number].reviews,
                    threads: thread_holds.remove(&number).flatten(),
                    would_notify_rejection: None,
                },
            )
        })
        .collect();
    Ok((landing, main.map(str::to_owned)))
}

/// A current unresolved review thread holds landing whatever the comment-level
/// findings say, so a thread that arrived after the board read still holds
/// every enqueue, retry and already-queued path (#1673).
fn reviews_with_threads(reviews: LatestReviews, threads: Option<&ThreadHold>) -> LatestReviews {
    if threads.is_some_and(|hold| hold.current > 0) {
        LatestReviews::Unresolved
    } else {
        reviews
    }
}

/// Decode one PR's `reviewThreads` page. An absent connection is unobserved
/// (`None`); a cut-off page or a malformed node is reported as `incomplete`.
fn thread_hold(value: &Value) -> Option<ThreadHold> {
    if !value.is_object() {
        return None;
    }
    let mut hold = ThreadHold::default();
    let rows = value["nodes"].as_array().map(Vec::as_slice).unwrap_or_default();
    hold.incomplete = value["nodes"].as_array().is_none()
        || value["totalCount"]
            .as_u64()
            .is_none_or(|total| total > rows.len() as u64);
    let mut oldest = None;
    for row in rows {
        let (Some(resolved), Some(outdated)) = (row["isResolved"].as_bool(), row["isOutdated"].as_bool())
        else {
            hold.incomplete = true;
            continue;
        };
        if resolved {
            continue;
        }
        hold.unresolved += 1;
        if !outdated {
            hold.current += 1;
        }
        let first = &row["comments"]["nodes"][0];
        match first["createdAt"]
            .as_str()
            .and_then(oxa_session::parse_utc_seconds)
        {
            Some(at) => oldest = Some(oldest.map_or(at, |old: u64| old.min(at))),
            None => hold.incomplete = true,
        }
        if let Some(login) = first["author"]["login"].as_str() {
            if hold.authors.len() < 4 && !hold.authors.iter().any(|known| known == login) {
                hold.authors.push(login.to_owned());
            }
        }
    }
    hold.oldest_at = oldest.unwrap_or(0);
    Some(hold)
}

fn queue_observations(queue: &Value, board: &Board) -> Result<BTreeMap<u64, QueueObservation>, String> {
    if queue.is_null() {
        return Ok(BTreeMap::new());
    }
    let connection = &queue["entries"];
    let total = connection["totalCount"]
        .as_u64()
        .ok_or("missing GitHub connection")?;
    let rows = connection["nodes"]
        .as_array()
        .ok_or("missing GitHub connection")?;
    if total < rows.len() as u64 {
        return Err("invalid merge queue connection".into());
    }
    if total > rows.len() as u64 {
        // Observed rows stay usable. A PR outside them keeps queue state Unknown, which takes no action.
        eprintln!(
            "oxa work: merge queue connection exceeds its {}-item page ({total} entries); queue state is unknown beyond it",
            rows.len()
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut queued = BTreeMap::new();
    for row in rows {
        let pr = &row["pullRequest"];
        let number = pr["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("missing queued PR number")?;
        if !seen.insert(number) {
            return Err("duplicate queued PR number".into());
        }
        let Some(entry) = board.entries.iter().find(|entry| entry.number == number) else {
            continue;
        };
        let head = oid(&pr["headRefOid"]).ok_or("invalid queued PR head")?;
        if entry.head != head {
            continue;
        }
        let enqueued_at = row["enqueuedAt"]
            .as_str()
            .and_then(oxa_session::parse_utc_seconds)
            .ok_or("invalid enqueue time for the current PR head")?;
        queued.insert(
            number,
            QueueObservation {
                enqueued_at,
                group_head: oid(&row["headCommit"]["oid"]).map(str::to_owned),
            },
        );
    }
    Ok(queued)
}

fn oid(value: &Value) -> Option<&str> {
    value.as_str().filter(|sha| is_object_id(sha))
}

fn comments(value: &Value) -> Option<Vec<Comment<'_>>> {
    let rows = nodes(value).ok()?;
    if rows.len() > 100 {
        return None;
    }
    rows.iter()
        .map(|row| {
            Some(Comment {
                bot_id: row["author"]["databaseId"].as_u64(),
                updated_at: row["updatedAt"].as_str().and_then(oxa_session::parse_utc_seconds),
                body: row["body"].as_str()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_work::board::Entry;
    use serde_json::json;

    #[test]
    fn a_malformed_current_queue_row_cannot_be_read_as_absent() {
        const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let board = Board {
            entries: vec![Entry {
                number: 42,
                head: HEAD.into(),
                release_pinned: false,
                action: Action::Ready,
                lease: None,
                attempts_today: 0,
                eligible: true,
                evidence_incomplete: None,
            }],
            observed_at: 1_767_225_601,
        };
        let queue = |head, time| {
            json!({"entries":{"totalCount":1,"nodes":[{
                "pullRequest":{"number":42,"headRefOid":head},"enqueuedAt":time,"headCommit":null
            }]}})
        };
        for time in [Value::Null, json!("2026-04-31T00:00:00Z")] {
            assert!(queue_observations(&queue(json!(HEAD), time.clone()), &board).is_err());
            assert!(queue_observations(&queue(json!(OTHER), time), &board)
                .unwrap()
                .is_empty());
        }
        let time = json!("2026-01-01T00:00:00Z");
        assert!(queue_observations(&queue(Value::Null, time.clone()), &board).is_err());
        let rows = queue_observations(&queue(json!(HEAD), time), &board).unwrap();
        assert_eq!(rows[&42].enqueued_at, 1_767_225_600);
        assert!(rows[&42].group_head.is_none());
    }

    fn threads(nodes: Value, total: u64) -> Option<ThreadHold> {
        thread_hold(&json!({"totalCount": total, "nodes": nodes}))
    }

    fn thread(resolved: bool, outdated: bool, login: &str) -> Value {
        json!({"isResolved": resolved, "isOutdated": outdated, "comments": {"nodes": [
            {"createdAt": "2026-01-01T00:00:00Z", "author": {"login": login}}
        ]}})
    }

    #[test]
    fn a_current_unresolved_thread_holds_every_landing_path() {
        use oxa_work::board::landing::{Next, Policy, Queue, Rejection, Retry};
        const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const MAIN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        const GROUP: &str = "cccccccccccccccccccccccccccccccccccccccc";
        let entry = Entry {
            number: 42,
            head: HEAD.into(),
            release_pinned: false,
            action: Action::Ready,
            lease: None,
            attempts_today: 0,
            eligible: true,
            evidence_incomplete: None,
        };
        let rejected = Queue::Rejected(Rejection {
            head: HEAD,
            main: GROUP,
            merge_group_head: GROUP,
            removal_event: "MDEyOlJlbW92ZWRFdmVudDE=",
            cause: oxa_work::board::landing::RejectionCause::MergeConflict,
            notified_event: Some("MDEyOlJlbW92ZWRFdmVudDE="),
            retry: Retry::Unused { last_admission_at: 1 },
        });
        let plan = |reviews, queue| {
            Policy { enabled: true }
                .plan(&entry, reviews, queue, MAIN, 1_000_000)
                .next
        };
        let held = threads(json!([thread(false, false, "reviewer")]), 1);
        let outdated = threads(json!([thread(false, true, "reviewer")]), 1);
        for (queue, free) in [
            (Queue::Absent, Next::Enqueue),
            (rejected, Next::Retry),
            (Queue::Queued { since: 1 }, Next::AlreadyQueued),
        ] {
            // Comment-level findings are clear: only the thread can hold the PR.
            assert_eq!(
                plan(
                    reviews_with_threads(LatestReviews::Clear, outdated.as_ref()),
                    queue
                ),
                free
            );
            assert_eq!(
                plan(reviews_with_threads(LatestReviews::Clear, held.as_ref()), queue),
                Next::HoldFindings
            );
        }
    }

    #[test]
    fn thread_pages_report_age_authors_and_incompleteness() {
        let hold = threads(
            json!([
                thread(false, false, "a"),
                thread(false, true, "a"),
                thread(true, false, "b")
            ]),
            3,
        )
        .unwrap();
        assert_eq!((hold.unresolved, hold.current, hold.incomplete), (2, 1, false));
        assert_eq!(hold.authors, ["a"]);
        assert_eq!(hold.oldest_at, 1_767_225_600);
        // A page cut off by the connection bound, or a malformed node, is incomplete, never zero.
        assert!(
            threads(json!([thread(false, false, "a")]), 101)
                .unwrap()
                .incomplete
        );
        assert!(threads(json!([{"isResolved": "no"}]), 1).unwrap().incomplete);
        assert!(thread_hold(&Value::Null).is_none());
    }
}
