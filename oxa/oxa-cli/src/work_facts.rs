//! Bounded facts reads. Task state and logs are never parsed by this CLI view.
use oxa_proto::work::{Fact, Snapshot, Subject};
use oxa_work::FactObserver;
use std::io::Write;
use std::path::Path;

pub(super) fn read(args: &[String], storage: &Path, out: &mut dyn Write) -> Result<Option<i32>, String> {
    let Some(command @ ("situation" | "events")) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    let json = args.last().is_some_and(|argument| argument == "--json");
    let words = if json { &args[..args.len() - 1] } else { args };
    match (command, words) {
        ("situation", [_]) => {
            let snapshot = FactObserver::open(storage)?.situation()?;
            if json {
                out.write_all(&snapshot.to_json().map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
                writeln!(out).map_err(|error| error.to_string())?;
            } else {
                situation(&snapshot, out)?;
            }
        }
        ("events", [_, flag, after]) if flag == "--after" => {
            let after = after
                .parse::<u64>()
                .map_err(|_| "event cursor must be an unsigned integer")?;
            let page = FactObserver::open(storage)?.events_after(after)?;
            if json {
                out.write_all(&page.to_json().map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
                writeln!(out).map_err(|error| error.to_string())?;
            } else {
                writeln!(
                    out,
                    "events after {}; next {}; first available {}; gap {}",
                    page.after, page.next, page.first_available, page.gap
                )
                .map_err(|error| error.to_string())?;
                for event in page.events {
                    let (kind, subject) = event_label(&event.fact);
                    writeln!(out, "{} {} {}", event.seq, kind, subject).map_err(|error| error.to_string())?;
                }
            }
        }
        _ => return Err("usage: oxa work situation [--json] | events --after N [--json]".into()),
    }
    Ok(Some(0))
}
fn situation(snapshot: &Snapshot, out: &mut dyn Write) -> Result<(), String> {
    let result = (|| -> std::io::Result<()> {
        write!(out, "as of event {}; ", snapshot.as_of_seq)?;
        if snapshot.standing_revision == 0 {
            writeln!(out, "standing revision unavailable")?;
        } else {
            writeln!(out, "standing revision {}", snapshot.standing_revision)?;
        }
        if let Some(goal) = &snapshot.goal {
            writeln!(out, "goal {goal}")?;
        }
        for row in &snapshot.running {
            writeln!(
                out,
                "running {} on {}: {} {} ({:?})",
                row.task, row.host, row.branch, row.worktree, row.step
            )?;
        }
        for row in &snapshot.local_accepted {
            writeln!(out, "checked {} {}", row.task, row.commit)?;
        }
        for row in &snapshot.published {
            writeln!(out, "published {} #{} {}", row.task, row.pr.number, row.pr.head)?;
        }
        for row in &snapshot.landed {
            writeln!(out, "landed #{} {}", row.pr.number, row.merge_commit)?;
        }
        for row in &snapshot.blocked {
            writeln!(out, "blocked {}: {}", subject(&row.subject), row.code)?;
        }
        for row in &snapshot.asks {
            writeln!(
                out,
                "ask {} for {}: {}",
                row.id,
                subject(&row.subject),
                row.question
            )?;
        }
        for row in &snapshot.incidents {
            writeln!(
                out,
                "incident {} for {}: {:?}",
                row.id,
                subject(&row.subject),
                row.kind
            )?;
        }
        if snapshot.history_truncated {
            writeln!(out, "older history was truncated")?;
        }
        Ok(())
    })();
    result.map_err(|error| error.to_string())
}
fn subject(subject: &Subject) -> String {
    match subject {
        Subject::Repository => "repository".into(),
        Subject::Task(id) => format!("task {id}"),
        Subject::PullRequest(number) => format!("#{number}"),
        Subject::Host(host) => format!("host {host}"),
        Subject::Inbox(id) => format!("inbox {id}"),
    }
}
fn event_label(fact: &Fact) -> (&'static str, String) {
    match fact {
        Fact::TaskStarted(row) => ("task_started", row.task.clone()),
        Fact::TaskTransition(row) => ("task_transition", row.task.clone()),
        Fact::WorkerFinished(row) => ("worker_finished", row.task.clone()),
        Fact::LocalAcceptance(row) => ("local_acceptance", row.task.clone()),
        Fact::Published(row) => ("published", format!("#{} {}", row.pr.number, row.pr.head)),
        Fact::ExternalAcceptance(row) => (
            "external_acceptance",
            format!("#{} {}", row.pr.number, row.pr.head),
        ),
        Fact::Enqueued(row) => ("enqueued", format!("#{} {}", row.number, row.head)),
        Fact::PrMerged(row) => ("merged", format!("#{} {}", row.pr.number, row.merge_commit)),
        Fact::Blocked(row) => ("blocked", subject(&row.subject)),
        Fact::Unblocked(row) => ("unblocked", subject(row)),
        Fact::AskOpened(row) => ("ask_opened", row.id.clone()),
        Fact::AskClosed(row) => ("ask_closed", row.id.clone()),
        Fact::Incident(row) => ("incident", row.id.clone()),
        Fact::Mechanical(row) => ("mechanical", subject(&row.subject)),
    }
}
