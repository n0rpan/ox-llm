//! The opt-in Lead observation loop. It can write a question, never a Board action.
//! A stuck-work signal is never a silent wait: every pass acts on it, by recording
//! an assignment in its report or by escalating an ask to the owner, before the
//! loop waits again (#1451, #1575).
use crate::{lead_observation::Observation, LeadOwnerBinding};
use oxa_config::{Dirs, Env};
use oxa_proto::{
    codes,
    work::{LocalAcceptance, PullRequest},
    OxaError, Value,
};
use oxa_session::{Goal, GoalStatus, OpenGoalAsk, SessionStore};
use oxa_work::board::{Action, Board, Entry};
use std::{collections::BTreeSet, fs, io::Write, os::unix::fs::MetadataExt, path::Path, time::Duration};

const OBJECTIVE: &str = "Observe the Board and ask the owner before taking the next action.";
const INTERVAL: Duration = Duration::from_secs(60);
/// At most this many per-PR rows in one stall report, in board order.
const REPORT_ROWS: usize = 8;

fn unavailable() -> OxaError {
    OxaError::new(codes::INVALID, "The dedicated Lead Session is unavailable.")
}

fn binding(dirs: &Dirs, cwd: &Path, env: &Env) -> Result<(LeadOwnerBinding, u32), OxaError> {
    if crate::dispatch::is_agent(env) {
        return Err(OxaError::new(codes::INVALID, "Use Lead from the owner session."));
    }
    let uid = fs::metadata("/proc/self").map_err(|_| unavailable())?.uid();
    let project = cwd.canonicalize().map_err(|_| unavailable())?;
    let binding = LeadOwnerBinding::load(dirs, &project, uid)?.ok_or_else(unavailable)?;
    Ok((binding, uid))
}

fn proposal(board: &Board, revision: u64) -> Option<OpenGoalAsk> {
    let entry = board
        .entries
        .iter()
        .filter(|entry| entry.eligible && entry.action <= Action::Ready)
        .min_by_key(|entry| (!entry.release_pinned, entry.action, entry.number))?;
    let action = match entry.action {
        Action::FixFindings => "address the review findings",
        Action::FixRequiredCi => "investigate the required CI failure",
        Action::ResolveConflict => "resolve the merge conflict",
        Action::AddRemoves => "review the removes documentation",
        Action::BuildIssue => "start the admitted issue",
        Action::PromoteDraft => "review the draft for promotion",
        Action::Ready => "review the pull request for the merge queue",
        Action::WaitRequiredCi
        | Action::WaitMergeability
        | Action::WaitReview
        | Action::WaitIssue
        | Action::EvidenceIncomplete => return None,
    };
    Some(OpenGoalAsk {
        id: format!("lead:board:{}", entry.number),
        revision,
        question: format!(
            "Board item #{} was eligible at {}. Should we {action} through the Board approval path?",
            entry.number, board.observed_at
        ),
    })
}

/// The per-PR reason one item is not landing (#1451). `None` rows are not part
/// of a stall report: waiting on intake or an incomplete row blocks nothing
/// that a stalled pipeline needs explained.
fn stall_reason(entry: &Entry) -> Option<&'static str> {
    match entry.action {
        Action::FixRequiredCi => Some("required CI failed"),
        Action::FixFindings => Some("unresolved review findings"),
        Action::ResolveConflict => Some("merge conflict"),
        Action::AddRemoves => Some("missing source-removal rationale"),
        Action::WaitReview => Some("missing review at the current head"),
        Action::WaitRequiredCi => Some("required CI pending"),
        _ => None,
    }
}

/// The worker that takes one red item: the item's own lease holder, otherwise an
/// idle known worker holding no active lease. `None` means no worker can take it
/// and the owner must (#1451).
fn assignee(board: &Board, entry: &Entry, now: u64) -> Option<String> {
    if let Some(lease) = entry.lease.as_ref() {
        return Some(lease.holder.clone());
    }
    idle_worker(board, now)
}

/// An idle worker: known to the board through a lease, and holding no active
/// lease anywhere at this observation.
fn idle_worker(board: &Board, now: u64) -> Option<String> {
    let active: BTreeSet<&str> = board
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .lease
                .as_ref()
                .filter(|lease| lease.expires_at > now)
                .map(|lease| lease.holder.as_str())
        })
        .collect();
    board
        .entries
        .iter()
        .filter_map(|entry| entry.lease.as_ref().map(|lease| lease.holder.as_str()))
        .filter(|holder| !active.contains(holder))
        .min()
        .map(str::to_owned)
}

/// One stuck-work observation of this pass: the signal names, the reasoned stall
/// report, and the escalations the pass must write. Derivation is pure; writing
/// asks stays the caller's job against the goal store's revision.
#[derive(Default)]
struct Stuck {
    signals: Vec<String>,
    report: Vec<Value>,
    escalations: Vec<OpenGoalAsk>,
}

impl Stuck {
    fn empty(&self) -> bool {
        self.signals.is_empty()
    }

    /// An idle worker takes a red main when one exists; otherwise the owner is
    /// the assignee of last resort and the pass escalates (#1451).
    fn main_red(&mut self, board: &Board, revision: u64, now: u64) {
        self.signals.push("main_red".into());
        match idle_worker(board, now) {
            Some(worker) => self.report.push(serde_json::json!({
                "subject": "main",
                "reason": "required checks on main failed",
                "assignee": worker,
            })),
            None => self.escalations.push(OpenGoalAsk {
                id: "lead:stuck:main-red".into(),
                revision,
                question:
                    "Required checks on main are failing and no known worker can take the fix. Assign someone to repair main CI."
                        .into(),
            }),
        }
    }

    /// The landing pipeline stalled with work waiting: every blocking item gets
    /// its reason on the report, and a red item gets an owner — its own worker,
    /// otherwise an idle one, otherwise the owner through an ask (#1451).
    fn merge_stalled(&mut self, board: &Board, revision: u64, now: u64) {
        self.signals.push("merge_stalled".into());
        let budget = REPORT_ROWS.saturating_sub(self.report.len());
        let blocking: Vec<&Entry> = board
            .entries
            .iter()
            .filter(|entry| stall_reason(entry).is_some())
            .take(budget)
            .collect();
        for entry in blocking {
            let assignee = if entry.action == Action::FixRequiredCi {
                match assignee(board, entry, now) {
                    Some(worker) => Some(worker),
                    None => {
                        self.escalations.push(OpenGoalAsk {
                            id: format!("lead:stuck:ci:{}", entry.number),
                            revision,
                            question: format!(
                                "PR #{} is red on required CI and no known worker can take it. Assign someone to fix required CI for PR #{}.",
                                entry.number, entry.number
                            ),
                        });
                        None
                    }
                }
            } else {
                None
            };
            self.report.push(serde_json::json!({
                "pr": entry.number,
                "reason": stall_reason(entry),
                "assignee": assignee,
            }));
        }
    }

    /// A PR that has waited too long in the merge queue is the runner's to
    /// advance or dequeue: the loop escalates it, it never assumes a cause.
    fn overdue_queue(&mut self, overdue: &[&PullRequest], revision: u64) {
        for pr in overdue {
            self.signals.push(format!("overdue_queue:{}", pr.number));
            self.escalations.push(OpenGoalAsk {
                id: format!("lead:stuck:queue:{}", pr.number),
                revision,
                question: format!(
                    "PR #{} has waited too long in the merge queue. Should the runner dequeue it or advance it?",
                    pr.number
                ),
            });
        }
    }

    /// Workflow runs queued past the shared stall policy: the loop escalates the
    /// bounded run ids, never a full page of GitHub rows.
    fn queued_runs(&mut self, runs: &[u64], revision: u64) {
        if runs.is_empty() {
            return;
        }
        self.signals.push("queued_runs".into());
        let ids: Vec<String> = runs.iter().map(u64::to_string).collect();
        self.escalations.push(OpenGoalAsk {
            id: "lead:stuck:runs".into(),
            revision,
            question: format!(
                "Workflow runs {} have been queued for more than 15 minutes. Check the runner queue.",
                ids.join(", ")
            ),
        });
    }

    /// A worker's locally accepted commit has no publication: the loop cannot
    /// push, so it escalates at once instead of logging and waiting (#1575).
    fn publish_pending(&mut self, unpublished: &[&LocalAcceptance], revision: u64) {
        let mut pending: BTreeSet<&str> = BTreeSet::new();
        for acceptance in unpublished {
            let task = acceptance.task.as_str();
            if pending.insert(task) {
                self.signals.push(format!("publish_pending:{task}"));
                self.escalations.push(OpenGoalAsk {
                    id: format!("lead:stuck:publish:{task}"),
                    revision,
                    question: format!(
                        "Task {task} has a locally accepted commit with no publication. Push its branch or stop the worker."
                    ),
                });
            }
        }
    }

    /// The reviewer refused the configured requester account: nothing is re-sent under
    /// that identity, so the owner must connect an account or change the requester.
    fn review_rejected(&mut self, rejected: &[u64], revision: u64) {
        for number in rejected {
            self.signals.push(format!("review_rejected:{number}"));
            self.escalations.push(OpenGoalAsk {
                id: format!("lead:stuck:review-identity:{number}"),
                revision,
                question: format!(
                    "The reviewer refused the review requester for PR #{number}; set the review policy requester to an account connected to the reviewer."
                ),
            });
        }
    }

    /// PRs held by unresolved review threads, queued ones first and then the
    /// oldest thread first, within the report bound. A cut-off thread page is
    /// reported as incomplete, never as zero (#1549). A held PR that is queued
    /// at its exact head is also escalated: it is the owner's to dequeue, and
    /// the signal ending on a later pass is the recorded outcome (#1673).
    fn review_held(&mut self, obs: &Observation, revision: u64) {
        // Outdated-only threads do not hold landing (the landing plan holds on
        // `current > 0`); an incomplete page stays reported, never as zero.
        let mut held = obs.thread_holds();
        held.retain(|h| h.hold.current > 0 || h.hold.incomplete);
        if held.is_empty() {
            return;
        }
        held.sort_by_key(|h| {
            let at = if h.hold.oldest_at == 0 {
                u64::MAX
            } else {
                h.hold.oldest_at
            };
            (!h.queued, at, h.number)
        });
        self.signals.push("review_held".into());
        let (now, budget) = (obs.now_ms / 1000, REPORT_ROWS.saturating_sub(self.report.len()));
        for h in held.iter().take(budget) {
            let owner = obs
                .board
                .entries
                .iter()
                .find(|entry| entry.number == h.number)
                .and_then(|entry| assignee(&obs.board, entry, now));
            self.report.push(serde_json::json!({
                "pr": h.number,
                "head": h.head,
                "reason": "unresolved review threads",
                "held": true,
                "threads": h.hold.unresolved,
                "current": h.hold.current,
                "oldest_thread_age_secs": (h.hold.oldest_at > 0).then(|| now.saturating_sub(h.hold.oldest_at)),
                "thread_authors": h.hold.authors,
                "incomplete": h.hold.incomplete,
                "assignee": owner,
            }));
        }
        for h in held.iter().filter(|h| h.queued && h.hold.current > 0) {
            self.signals.push(format!("queued_findings:{}", h.number));
            self.escalations.push(OpenGoalAsk {
                id: format!("lead:stuck:queued-findings:{}", h.number),
                revision,
                question: format!(
                    "PR #{} is in the merge queue at {} with {} current unresolved review threads. Dequeue it so the findings are addressed.",
                    h.number, h.head, h.hold.current
                ),
            });
        }
    }
}

/// Derive this pass's stuck-work observation from its protected inputs. Unobserved
/// alarm sources produce no signal: the loop never invents a stall, and a stale
/// record never manufactures an escalation. A standing ask is its own action, so
/// it is reported through `asks` and never re-escalated here.
fn stuck_signals(obs: &Observation, revision: u64) -> Stuck {
    let mut stuck = Stuck::default();
    if obs.fresh() {
        let (board, alarms, now) = (&obs.board, &obs.alarms, obs.now_ms / 1000);
        if alarms.main_red == Some(true) {
            stuck.main_red(board, revision, now);
        }
        if alarms.main_merge_stalled == Some(true) {
            stuck.merge_stalled(board, revision, now);
        }
        if let Some(overdue) = obs.overdue_in_queue().filter(|overdue| !overdue.is_empty()) {
            stuck.overdue_queue(&overdue, revision);
        }
        if let Some(runs) = alarms.stalled_runs.as_ref().filter(|runs| !runs.is_empty()) {
            stuck.queued_runs(runs, revision);
        }
        stuck.review_held(obs, revision);
        stuck.review_rejected(&obs.review_rejected, revision);
    }
    stuck.publish_pending(&obs.unpublished(), revision);
    stuck
}

fn tick(binding: &LeadOwnerBinding, obs: &Observation) -> Result<Value, OxaError> {
    let (board, now_ms, fresh) = (&obs.board, obs.now_ms, obs.fresh());
    binding.with_goal_store(|store, session| {
        let entries = store.load(session)?;
        let revision = Goal::revision(&entries);
        let saved = Goal::latest(&entries)?;
        let create = saved.is_none() && revision == 0;
        let mut goal = match saved {
            Some(goal) => goal,
            None if create => {
                let mut goal = Goal::new(OBJECTIVE.into(), Default::default(), now_ms)?;
                goal.pause();
                goal
            }
            None => return Ok(view(None, revision, "goal_cleared", false)),
        };
        if goal.objective != OBJECTIVE || !matches!(goal.status, GoalStatus::Paused | GoalStatus::Pursuing) {
            return Ok(view(Some(&goal), revision, "goal_inactive", false));
        }
        let next = entries.len() as u64 + 1;
        // Stuck work outranks a new proposal: every signal is acted on in this
        // pass, by assignment or escalation, before the loop waits again (#1575).
        let stuck = stuck_signals(obs, next);
        // Signals with neither a report row that has an owner nor an escalation
        // (a stall whose only rows are ready PRs) leave the pass free to propose
        // the next Board action instead of ending in a silent assignment (#2088).
        // A review_held row is information, not an assignment: it never ends the
        // pass, or the proposal loop starves on the normal lab state.
        let acted = !stuck.escalations.is_empty()
            || stuck
                .report
                .iter()
                .any(|row| row["held"] != true && row["assignee"].is_string());
        let attach = |mut value: Value| {
            if !stuck.empty() {
                value["stuck"] = serde_json::json!({
                    "signals": stuck.signals,
                    "report": stuck.report,
                    "escalated": Vec::<String>::new(),
                });
            }
            value
        };
        if acted {
            let open: BTreeSet<String> = goal
                .lead
                .as_ref()
                .into_iter()
                .flat_map(|lead| lead.open_asks.iter().map(|ask| ask.id.clone()))
                .collect();
            let fresh_escalations: Vec<OpenGoalAsk> = stuck
                .escalations
                .iter()
                .filter(|ask| !open.contains(&ask.id))
                .cloned()
                .collect();
            let escalated_ids: Vec<String> = fresh_escalations.iter().map(|ask| ask.id.clone()).collect();
            let escalated = !fresh_escalations.is_empty();
            if escalated {
                goal.lead
                    .get_or_insert_with(Default::default)
                    .open_asks
                    .extend(fresh_escalations);
            }
            let revision = if create || escalated {
                oxa_core::goal::checkpoint_idle_lead_goal(store, session, &entries, &goal)?
            } else {
                revision
            };
            let awaiting = goal.lead.as_ref().is_some_and(|lead| !lead.open_asks.is_empty());
            let state = if escalated {
                "stuck_escalated"
            } else if awaiting {
                "awaiting_owner"
            } else {
                "stuck_assigned"
            };
            let mut value = view(Some(&goal), revision, state, false);
            value["stuck"] = serde_json::json!({
                "signals": stuck.signals,
                "report": stuck.report,
                "escalated": escalated_ids,
            });
            return Ok(value);
        }
        if goal.lead.as_ref().is_some_and(|lead| !lead.open_asks.is_empty()) {
            // An earlier process may have appended and stopped before it could sync.
            store.sync(session)?;
            return Ok(attach(view(Some(&goal), revision, "awaiting_owner", false)));
        }
        let ask = fresh.then(|| proposal(board, next)).flatten();
        let has_ask = ask.is_some();
        if let Some(ask) = ask {
            goal.lead.get_or_insert_with(Default::default).open_asks.push(ask);
        }
        let revision = if create || has_ask {
            oxa_core::goal::checkpoint_idle_lead_goal(store, session, &entries, &goal)?
        } else {
            revision
        };
        let status = match (fresh, has_ask) {
            (_, true) => "awaiting_owner",
            (true, false) => "no_eligible_action",
            (false, false) => "board_unavailable",
        };
        Ok(attach(view(Some(&goal), revision, status, false)))
    })
}

fn view(goal: Option<&Goal>, revision: u64, state: &str, include_text: bool) -> Value {
    let asks: Vec<_> = goal
        .and_then(|goal| goal.lead.as_ref())
        .into_iter()
        .flat_map(|lead| &lead.open_asks)
        .map(|ask| {
            let mut value = serde_json::json!({"id": ask.id, "revision": ask.revision});
            if include_text {
                value["question"] = ask.question.clone().into();
            }
            value
        })
        .collect();
    serde_json::json!({
        "state": state,
        "goal_revision": revision,
        "goal_status": goal.map(|goal| goal.status.as_str()),
        "asks": asks,
    })
}

fn write(out: &mut dyn Write, value: &Value) -> Result<(), OxaError> {
    writeln!(out, "{value}")
        .and_then(|()| out.flush())
        .map_err(|error| OxaError::io("cannot write Lead observation", &error))
}

pub(crate) fn asks(dirs: &Dirs, cwd: &Path, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let (binding, _) = binding(dirs, cwd, env)?;
    let result = binding.with_goal_store(|store, session| {
        let entries = store.load(session)?;
        let goal = Goal::latest(&entries)?;
        store.sync(session)?;
        Ok(view(goal.as_ref(), Goal::revision(&entries), "observed", true))
    })?;
    write(out, &result)?;
    Ok(0)
}

pub(crate) fn run(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let once = match words {
        [] => false,
        [arg] if arg == "--once" => true,
        _ => return Err(OxaError::new(codes::USAGE, "Use oxa lead run [--once].")),
    };
    let board_path = env
        .get("OXA_LEAD_BOARD_LOG")
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| OxaError::new(codes::USAGE, "Supply the protected Lead Board log."))?;
    let situation_path = match env.get("OXA_LEAD_SITUATION").map(Path::new) {
        None => None,
        Some(path) if path.is_absolute() => Some(path),
        Some(_) => return Err(OxaError::new(codes::USAGE, "OXA_LEAD_SITUATION must be absolute")),
    };
    let mut previous = None;
    loop {
        let (binding, uid) = binding(dirs, cwd, env)?;
        let obs = Observation::read(board_path, situation_path, uid, oxa_proto::time::now_ms());
        let mut state = match tick(&binding, &obs) {
            Ok(state) => state,
            Err(error) if !once && error.code == codes::BUSY => {
                std::thread::sleep(INTERVAL);
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(cause) = obs.cause {
            state["cause"] = cause.text().into();
            state["hint"] = cause.hint().into();
        }
        if !obs.quota_deferred.is_empty() {
            // Say why a source is absent: the owner's App reserve deferred it.
            state["quota_deferred"] = serde_json::to_value(&obs.quota_deferred).unwrap_or_default();
        }
        if obs.fresh() {
            state["board_observed_at"] = obs.board.observed_at.into();
        }
        if previous.as_ref() != Some(&state) {
            write(out, &state)?;
            previous = Some(state);
        }
        if once {
            return Ok(0);
        }
        std::thread::sleep(INTERVAL);
    }
}
