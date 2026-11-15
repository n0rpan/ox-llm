//! Native long-running project goal: one process and conversation, many turns.
use crate::{
    goals::{self, Decision, Effect, Goal, State},
    journal::Journal,
    run::SessionWorker,
    Outcome, RunSettings, WorkerSpec,
};
use serde::Serialize;
use serde_json::json;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, TryRecvError},
    Arc,
};
use std::time::Duration;

pub enum Control {
    /// Correlate an attachment request with its eventual owner admission event.
    Identified {
        id: String,
        control: Box<Control>,
    },
    Pause {
        revision: u64,
    },
    Resume {
        revision: u64,
    },
    Steer {
        revision: u64,
        text: String,
    },
    Cancel {
        revision: u64,
    },
    SetGoal {
        revision: u64,
        text: String,
        budget: Option<goals::Budget>,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub kind: String,
    pub goal: goals::Snapshot,
    pub reply: Option<String>,
    pub request_id: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub goal: goals::Snapshot,
    pub last_reply: String,
    pub process_id: u32,
    pub process_stopped: bool,
    pub output_bytes: usize,
    pub runtime_outcome: Option<Outcome>,
    pub review: Option<crate::workspace::ReviewBundle>,
}
const CONTRACT: &str = "Pursue the operator's long-running goal across turns. A turn ending is not goal completion. After each turn return exactly one JSON object: {\"state\":\"continue\",\"summary\":\"work completed and what remains\"}, {\"state\":\"done\",\"summary\":\"what was verified\",\"evidence\":[\"concrete checks/results\"]}, or {\"state\":\"blocked\",\"reason\":\"specific missing input or external dependency\"}. Do not declare done until the full requested goal is verified. Budget, model, tools and permissions are operator-owned and cannot be changed by your reply.";

/// Controls and status consumers are trusted operator components. The observer
/// must not block. The SAME RPC process owns context across all turns and pauses.
/// Inference authority is captured locally; no wire grants, session discovery
/// or initial-send replay can widen it.
pub fn pursue(
    settings: RunSettings,
    mut spec: WorkerSpec,
    budget: goals::Budget,
    controls: Receiver<Control>,
    cancel: Arc<AtomicBool>,
    mut observe: impl FnMut(Event),
) -> io::Result<Report> {
    if settings.parallelism != 1
        || settings.wall_time != Duration::from_millis(budget.wall_ms)
        || budget
            .tokens
            .is_some_and(|tokens| tokens != spec.budget.reported_tokens)
    {
        return Err(invalid(
            "goal session needs one runtime and matching cumulative allowances",
        ));
    }
    crate::run::validate(&settings, std::slice::from_ref(&spec))?;
    if spec
        .workspace
        .as_ref()
        .is_some_and(|grant| grant.uses_git_worktrees())
    {
        return Err(invalid(
            "goal sessions currently use approved private copies, not Git worktrees",
        ));
    }
    let mut goal = Goal::new(settings.id.clone(), spec.task.clone(), budget)
        .map_err(|_| invalid("invalid session goal or budget"))?;
    let turn_cap = spec.budget.wall_time;
    spec.role = format!("{}\n\n{CONTRACT}", spec.role);
    spec.task = prompt(&goal, false, false);
    crate::run::validate(&settings, std::slice::from_ref(&spec))?;
    let parent = settings
        .directory
        .parent()
        .ok_or_else(|| invalid("missing goal session parent"))?
        .canonicalize()?;
    let _namespace = crate::namespace::protected_directory(&parent)?;
    let directory = parent.join(
        settings
            .directory
            .file_name()
            .ok_or_else(|| invalid("missing goal session directory name"))?,
    );
    DirBuilder::new().mode(0o700).create(&directory)?;
    let mut journal = Journal::create(&directory.join("goal.jsonl"), settings.journal_bytes)?;
    journal.record(
        None,
        "goal_set",
        json!({"id":settings.id,"budget":goal.snapshot().budget,"goalSha256":digest(&spec.task)}),
    )?;
    goal.begin_turn()
        .map_err(|_| invalid("goal budget unavailable before first turn"))?;
    let mut runtime = SessionWorker::start(&settings, spec, &directory, &mut journal)?;
    let mut turn_started = crate::resume::boot_time_ns()?;
    let mut accounted = 0u64;
    let mut last_reply = String::new();
    let mut runtime_outcome = None;
    let mut connected = true;
    let mut active = true;
    let mut needs_declaration = false;
    emit(&goal, "goal_started", None, &mut observe);
    let mut status_at = std::time::Instant::now();
    loop {
        if status_at.elapsed() >= Duration::from_millis(250) {
            emit(&goal, "goal_status", None, &mut observe);
            status_at = std::time::Instant::now();
        }
        for _ in 0..32 {
            let control = match controls.try_recv() {
                Ok(control) => control,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    connected = false;
                    break;
                }
            };
            let (request_id, control) = match control {
                Control::Identified { id, control } if !id.is_empty() && id.len() <= 128 => {
                    (Some(id), *control)
                }
                control => (None, control),
            };
            let accepted = match control {
                Control::Identified { .. } => false,
                Control::Pause { revision } => goal
                    .pause(revision)
                    .map(|effect| {
                        if effect == Effect::AbortTurn {
                            runtime.pause()
                        } else {
                            Ok(())
                        }
                    })
                    .ok()
                    .transpose()?
                    .is_some(),
                Control::Resume { revision } => goal.resume(revision).is_ok(),
                Control::Cancel { revision } => goal.cancel(revision).is_ok(),
                Control::SetGoal {
                    revision,
                    text,
                    budget,
                } => {
                    // Keep escaped text inside the native task's size bound.
                    if budget.is_none()
                        && serde_json::to_string(&text).is_ok_and(|s| s.len() <= 24 * 1024)
                        && goal.set_goal(revision, text.clone()).is_ok()
                    {
                        journal.record(None, "goal_updated", json!({"goalSha256":digest(&text)}))?;
                        true
                    } else {
                        false
                    }
                }
                Control::Steer { revision, text } => {
                    if revision == goal.revision()
                        && goal.state() == State::Active
                        && active
                        && runtime.steer(&text).is_ok()
                    {
                        goal.steered();
                        journal.record(None, "goal_steer", json!({"textSha256":digest(&text)}))?;
                        true
                    } else {
                        false
                    }
                }
            };
            observe(Event {
                kind: if accepted {
                    "control_admitted"
                } else {
                    "control_rejected"
                }
                .into(),
                goal: goal.snapshot(),
                reply: None,
                request_id,
            });
        }
        if cancel.load(Ordering::Acquire) {
            let _ = goal.cancel(goal.revision());
        }
        let effect = goal.tick();
        if matches!(goal.state(), State::Canceled | State::BudgetExhausted) {
            runtime.stop();
            if active {
                let _ = goal.finish_turn(
                    None,
                    Decision::Continue {
                        summary: "Runtime stopped at operator/budget boundary".into(),
                    },
                );
            }
            break;
        }
        if effect == Effect::AbortTurn {
            runtime.pause()?;
        }
        if active {
            let now = crate::resume::boot_time_ns()?;
            if now
                .checked_sub(turn_started)
                .ok_or_else(|| invalid("native turn clock regressed"))? as u128
                >= turn_cap.as_nanos()
            {
                runtime.stop();
                let _ = goal.finish_turn(
                    None,
                    Decision::Blocked {
                        reason: "turn_time_budget".into(),
                    },
                );
                goal.runtime_blocked("turn_time_budget");
                runtime_outcome = Some(Outcome::Deadline);
                break;
            }
            if let Some(outcome) = runtime.poll(&mut journal)? {
                let known = runtime.final_usage_known(&outcome);
                let usage = if known {
                    Some(
                        runtime
                            .tokens()
                            .checked_sub(accounted)
                            .ok_or_else(|| invalid("native usage regressed"))?,
                    )
                } else {
                    None
                };
                accounted = runtime.tokens();
                last_reply = runtime.reply().to_owned();
                let valid = serde_json::from_str::<Decision>(&last_reply).ok();
                needs_declaration = outcome == Outcome::Completed && valid.is_none();
                let decision = if outcome == Outcome::Completed {
                    valid.unwrap_or_else(|| Decision::Continue {
                        summary:
                            "Reply did not contain a valid complete goal declaration; goal remains active"
                                .into(),
                    })
                } else {
                    Decision::Continue {
                        summary: "Turn was interrupted".into(),
                    }
                };
                if goal.finish_turn(usage, decision).is_err() {
                    needs_declaration = true;
                    goal.finish_turn(
                        usage,
                        Decision::Continue {
                            summary: "Invalid goal declaration; continue within the same budget".into(),
                        },
                    )
                    .map_err(|_| invalid("goal accounting rejected turn"))?;
                }
                active = false;
                let stopped = !matches!(outcome, Outcome::Completed | Outcome::Canceled);
                if stopped {
                    if matches!(outcome, Outcome::TokenBudget | Outcome::OutputBudget) {
                        goal.runtime_budget(if outcome == Outcome::TokenBudget {
                            "token_budget"
                        } else {
                            "output_budget"
                        });
                    } else {
                        goal.runtime_blocked("native_runtime_failure");
                    }
                    runtime_outcome = Some(outcome);
                }
                journal.record(None,"goal_turn_finished",json!({"turn":goal.snapshot().turns_started,"state":goal.state(),"reportedTokens":goal.snapshot().reported_tokens,"replySha256":digest(&last_reply)}))?;
                emit(
                    &goal,
                    if stopped { "turn_stopped" } else { "turn_reply" },
                    Some(last_reply.clone()),
                    &mut observe,
                );
                if stopped {
                    break;
                }
                if goal.state() == State::Done {
                    break;
                }
                continue; // admit controls before automatically starting another turn
            }
        }
        if !active {
            match goal.state() {
                State::Active => {
                    if goal.begin_turn().is_ok() {
                        runtime.continue_turn(prompt(&goal, true, needs_declaration), &mut journal)?;
                        active = true;
                        turn_started = crate::resume::boot_time_ns()?;
                        emit(&goal, "turn_started", None, &mut observe);
                    }
                }
                State::Blocked | State::Paused if !connected => break,
                State::Done | State::Canceled | State::BudgetExhausted => break,
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    runtime.stop();
    let review = match runtime.review() {
        Ok(review) => review,
        Err(_) => {
            goal.runtime_blocked("workspace_review_refused");
            runtime_outcome = Some(Outcome::PolicyViolation);
            None
        }
    };
    journal.record(None,"goal_finished",json!({"state":goal.state(),"turns":goal.snapshot().turns_started,"reportedTokens":goal.snapshot().reported_tokens}))?;
    emit(&goal, "goal_finished", None, &mut observe);
    Ok(Report {
        goal: goal.snapshot(),
        last_reply,
        process_id: runtime.pid(),
        process_stopped: true,
        output_bytes: runtime.bytes(),
        runtime_outcome,
        review,
    })
}
fn prompt(goal: &Goal, continuation: bool, needs_declaration: bool) -> String {
    json!({"goal":goal.snapshot().goal,"continuation":continuation,
        "declaration_feedback":if needs_declaration { "The previous turn did not provide a valid complete goal declaration. Keep the original goal and return exactly one continue/done/blocked JSON object with the required fields; a quoted DONE or ordinary reply is not completion." } else { "Follow the goal contract." },
        "instruction":"Continue useful work toward the whole goal. Verify completion and return the structured continue/done/blocked declaration required by the goal contract."}).to_string()
}
fn emit(goal: &Goal, kind: &str, reply: Option<String>, observe: &mut impl FnMut(Event)) {
    observe(Event {
        kind: kind.into(),
        goal: goal.snapshot(),
        reply,
        request_id: None,
    });
}
fn digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
