//! The opt-in Lead observation loop. It can write a question, never a Board action.
use crate::{lead_input, lead_page, LeadOwnerBinding};
use oxa_config::{Dirs, Env};
use oxa_proto::{codes, OxaError, Value};
use oxa_session::{Goal, GoalStatus, OpenGoalAsk, SessionStore};
use oxa_work::board::{Action, Board};
use std::{fs, io::Write, os::unix::fs::MetadataExt, path::Path, time::Duration};

const OBJECTIVE: &str = "Observe the Board and ask the owner before taking the next action.";
const INTERVAL: Duration = Duration::from_secs(60);

fn unavailable() -> OxaError {
    OxaError::new(codes::INVALID, "The dedicated Lead Session is unavailable.")
}

fn binding(dirs: &Dirs, cwd: &Path, env: &Env) -> Result<(LeadOwnerBinding, u32), OxaError> {
    if env.pairs().iter().any(|(key, _)| {
        matches!(
            key.as_str(),
            "OXA_CODING_AGENT" | "AI_AGENT" | "OXA_SUBAGENT_DEPTH"
        )
    }) {
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
        .filter(|entry| entry.eligible && entry.lease.is_none() && entry.action <= Action::Ready)
        .min_by_key(|entry| (!entry.release_pinned, entry.action, entry.number))?;
    let action = match entry.action {
        Action::FixFindings => "address the review findings",
        Action::FixRequiredCi => "investigate the required CI failure",
        Action::ResolveConflict => "resolve the merge conflict",
        Action::AddRemoves => "review the removes documentation",
        Action::BuildIssue => "start the admitted issue",
        Action::PromoteDraft => "review the draft for promotion",
        Action::Ready => "review the pull request for the merge queue",
        Action::WaitRequiredCi | Action::WaitMergeability | Action::WaitReview | Action::WaitIssue => {
            return None;
        }
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

fn tick(binding: &LeadOwnerBinding, board: &Board, now_ms: u64) -> Result<Value, OxaError> {
    let fresh = !lead_page::stale(board, now_ms / 1000);
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
            None => return Ok(view(None, revision, "goal_cleared")),
        };
        if goal.objective != OBJECTIVE || !matches!(goal.status, GoalStatus::Paused | GoalStatus::Pursuing) {
            return Ok(view(Some(&goal), revision, "goal_inactive"));
        }
        if goal.lead.as_ref().is_some_and(|lead| !lead.open_asks.is_empty()) {
            // An earlier process may have appended and stopped before it could sync.
            store.sync(session)?;
            return Ok(view(Some(&goal), revision, "awaiting_owner"));
        }
        let next = entries.len() as u64 + 1;
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
        Ok(view(Some(&goal), revision, status))
    })
}

fn view(goal: Option<&Goal>, revision: u64, state: &str) -> Value {
    let asks: Vec<_> = goal
        .and_then(|goal| goal.lead.as_ref())
        .into_iter()
        .flat_map(|lead| &lead.open_asks)
        .map(|ask| serde_json::json!({"id": ask.id, "revision": ask.revision, "question": ask.question}))
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
        Ok(view(goal.as_ref(), Goal::revision(&entries), "observed"))
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
    let mut previous = None;
    loop {
        let (binding, uid) = binding(dirs, cwd, env)?;
        let board = lead_input::board_and_app_quota(board_path, uid)
            .map(|(board, _)| board)
            .unwrap_or_default();
        let now_ms = oxa_proto::time::now_ms();
        let mut state = match tick(&binding, &board, now_ms) {
            Ok(state) => state,
            Err(error) if !once && error.code == codes::BUSY => {
                std::thread::sleep(INTERVAL);
                continue;
            }
            Err(error) => return Err(error),
        };
        if !lead_page::stale(&board, now_ms / 1000) {
            state["board_observed_at"] = board.observed_at.into();
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
