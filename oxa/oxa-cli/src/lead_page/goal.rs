//! Read-only native Goal acceptance; source and full Session authority stay with the caller.
use super::{escape, human_time, View};
use oxa_session::{GoalCheck, GoalStatus};
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=owner-goal><h2>Owner goal</h2>");
    let Some((goal, observation)) = view.owner_goal else {
        html.push_str("<p>Goal observation unavailable. No bound native goal observation reached this page.</p></section>");
        return;
    };
    if observation.revision == 0 || observation.status != Some(goal.status) || goal.validate().is_err() {
        html.push_str("<p>Goal observation unavailable. The supplied native goal and saved observation do not agree.</p></section>");
        return;
    }
    let state = if goal.status == GoalStatus::Done && goal.plan.is_none() {
        "done reported; acceptance plan unavailable"
    } else {
        goal.status.as_str()
    };
    let _ = write!(
        html,
        "<p>{}</p><p class=muted>Recorded state: {state} · revision {}</p><p class=muted>Started {}</p>",
        escape(&goal.objective),
        observation.revision,
        human_time(goal.started_ms / 1000, now)
    );
    if let Some(evidence) = &goal.evidence {
        let _ = write!(html, "<p>Goal evidence record: {}</p>", escape(evidence));
    }
    if let Some(plan) = &goal.plan {
        html.push_str(if plan.verified() {
            "<p>Every acceptance check has passing evidence recorded.</p>"
        } else {
            "<p>Acceptance checks still need passing evidence.</p>"
        });
        html.push_str("<h3>Owner acceptance</h3>");
        checks(html, &plan.acceptance, now);
        html.push_str("<h3>Sub-goals</h3><ol>");
        for subgoal in &plan.subgoals {
            let _ = write!(
                html,
                "<li><article><h4>{}</h4><p class=muted>Sub-goal {}</p>",
                escape(&subgoal.objective),
                escape(&subgoal.id)
            );
            checks(html, &subgoal.acceptance, now);
            if subgoal.board_items.is_empty() {
                html.push_str("<p>Planned board items: none recorded.</p>");
            } else {
                html.push_str("<p>Planned board items: ");
                for number in &subgoal.board_items {
                    let _ = write!(html, "<a href=/items/{number}>#{number}</a> ");
                }
                html.push_str("</p>");
            }
            html.push_str("</article></li>");
        }
        html.push_str("</ol>");
    } else {
        html.push_str("<p>Acceptance tree unavailable. The native goal has no saved acceptance plan.</p>");
    }
    html.push_str("</section>");
}

fn checks(html: &mut String, checks: &[GoalCheck], now: u64) {
    html.push_str("<ul>");
    for check in checks {
        let _ = write!(
            html,
            "<li><p>{}</p><p class=muted>Check {} · {}</p>",
            escape(&check.acceptance),
            escape(&check.id),
            escape(&check.done_check)
        );
        if let Some(result) = &check.result {
            let _ = write!(
                html,
                "<p><b>{}</b></p><p>Evidence: {}</p><p class=muted>Observed {}</p>",
                if result.passed {
                    "Pass recorded"
                } else {
                    "Failed check"
                },
                escape(&result.receipt),
                human_time(result.observed_at_ms / 1000, now)
            );
        } else {
            html.push_str("<p><b>Not checked</b> · No outcome receipt is recorded.</p>");
        }
        html.push_str("</li>");
    }
    html.push_str("</ul>");
}
