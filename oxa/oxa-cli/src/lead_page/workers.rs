//! Workers view: live bindings and runner facts grouped by host, with state chips (#2350).
use super::{escape, human_time, say_why, View};
use oxa_proto::work::{IncidentKind, Subject};
use std::fmt::Write;

/// One live worker row: name, lane, model, state chip and how old its status is.
fn binding_row(html: &mut String, binding: &crate::workers::tool::Binding) {
    use crate::workers::registry::WorkerState;
    let state = match &binding.state {
        WorkerState::Starting => "Starting".to_owned(),
        WorkerState::Working => "Working".to_owned(),
        WorkerState::Idle => "Idle".to_owned(),
        WorkerState::Finished(outcome) => format!("Finished ({outcome})"),
        WorkerState::Dead => "Dead: its process is gone".to_owned(),
        WorkerState::Stopped => "Stopped".to_owned(),
    };
    let active = matches!(
        binding.state,
        WorkerState::Starting | WorkerState::Working | WorkerState::Idle
    );
    let age = match binding.status_age_secs {
        Some(age) if active && age > 300 => format!(" · status stale ({age} s old)"),
        Some(age) => format!(" · status {age} s old"),
        None => " · no status file".to_owned(),
    };
    let _ = write!(
        html,
        "<article><b>{}</b><p>{} · {} · <span class=chip>{}</span>{}</p></article>",
        escape(&binding.name),
        escape(&binding.branch),
        escape(&binding.model),
        escape(&state),
        age
    );
}

pub(super) fn render(html: &mut String, view: &View<'_>, now: u64) {
    use crate::workers::tool::BindingsError;
    html.push_str("<section id=lanes><h2>Workers</h2>");
    let live = matches!(view.workers, Some(Ok(_)));
    match view.workers {
        Some(Ok(bindings)) => {
            html.push_str("<p class=muted>Live workers of the Lead session</p>");
            html.push_str("<h3 class=host>This host</h3>");
            bindings.iter().for_each(|binding| binding_row(html, binding));
        }
        Some(Err(BindingsError::Refused)) => html.push_str(
            "<p>Live workers refused: the worker registry is unreadable or corrupt. It was left untouched.</p>",
        ),
        Some(Err(BindingsError::Absent)) | None => {}
    }
    if let Some(s) = view.situation {
        let _ = write!(
            html,
            "<p class=muted>Runner facts · sequence {} · observed {}</p>",
            s.as_of_seq,
            human_time(s.at_ms / 1000, now)
        );
        // Group the observed tasks by their host, in first-seen order; within a host the
        // reported stalls come first and the observation order is otherwise kept.
        let mut hosts: Vec<&str> = Vec::new();
        for running in &s.running {
            if !hosts.contains(&running.host.as_str()) {
                hosts.push(running.host.as_str());
            }
        }
        for host in hosts {
            let _ = write!(html, "<h3 class=host>{}</h3>", escape(host));
            let mut group: Vec<_> = s
                .running
                .iter()
                .filter(|running| running.host == host)
                .map(|running| {
                    let stalled = s.incidents.iter().any(|i| {
                        i.kind == IncidentKind::WorkerStalled
                            && i.subject == Subject::Task(running.task.clone())
                    });
                    (stalled, running)
                })
                .collect();
            group.sort_by_key(|(stalled, _)| !stalled);
            for (stalled, running) in group {
                let chip = if stalled {
                    "<span class=\"chip bad\">Stall reported</span>"
                } else {
                    "<span class=chip>Busy at observation</span>"
                };
                let _ = write!(
                    html,
                    "<article><b>{}</b><p>{chip}</p></article>",
                    escape(&running.task)
                );
            }
        }
        if s.running.is_empty() {
            html.push_str("<p>No running tasks reported. Idle worker inventory is unavailable because these facts do not include a complete worker inventory.</p>");
        }
        for incident in &s.incidents {
            if let Subject::Host(host) = &incident.subject {
                if incident.kind == IncidentKind::HostIdle && !s.running.iter().any(|r| &r.host == host) {
                    let _ = write!(
                        html,
                        "<article><b>{}</b><p><span class=chip>Idle reported</span></p></article>",
                        escape(host)
                    );
                }
            }
        }
    } else if !live && !matches!(view.workers, Some(Err(BindingsError::Refused))) {
        say_why(
            html,
            "Lane status",
            "no runner observation reached this page",
            "rerun the Lead loop to read a runner situation",
            None,
            now,
        );
    }
    html.push_str("</section>");
}
