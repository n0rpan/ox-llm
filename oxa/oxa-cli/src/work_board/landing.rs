//! Shadow-only observation of latest review holds through the shared Reader.
use super::{charge, nodes, Reader};
use oxa_work::board::{
    landing::{LatestReviews, Observation, Policy, Queue},
    queue::{is_node_id, is_object_id},
    review::{self, Comment, Findings},
    Action, Board,
};
use oxa_work::shadow::{LandingDecision, QueueObservation};
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
        .filter(|entry| !matches!(entry.action, Action::BuildIssue | Action::WaitIssue))
        .collect();
    let mut observations = BTreeMap::new();
    let mut exact_holds = BTreeMap::new();
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
            if let Some(id) = row["id"].as_str().filter(|_| exact_hold) {
                let auto_merge = row["autoMergeRequest"]["enabledAt"]
                    .as_str()
                    .and_then(oxa_session::parse_utc_seconds)
                    .is_some();
                let queued = row["mergeQueueEntry"]["id"].as_str().is_some_and(is_node_id);
                exact_holds.insert(entry.number, (id.to_owned(), auto_merge, queued));
            }
            let reviews = match findings {
                Findings::Unresolved => LatestReviews::Unresolved,
                Findings::Clear => LatestReviews::Clear,
                Findings::Unknown => LatestReviews::Unknown,
            };
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
                    repeat_rejection_alarm: false,
                    hold_intervention,
                    queue: queued.remove(&number),
                    latest_reviews: observations[&number].reviews,
                    would_notify_rejection: None,
                },
            )
        })
        .collect();
    Ok((landing, main.map(str::to_owned)))
}

fn queue_observations(queue: &Value, board: &Board) -> Result<BTreeMap<u64, QueueObservation>, String> {
    if queue.is_null() {
        return Ok(BTreeMap::new());
    }
    let rows = nodes(&queue["entries"])?;
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
}
