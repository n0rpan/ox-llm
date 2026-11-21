//! One Lead observation per tick. The loop reads the protected Board sample and the
//! runner situation once; every stuck-work rule is a pure derivation over the result,
//! so no rule makes its own partial read (#2088, #2090, #2091).
use crate::workers::tool::{Binding, BindingsError};
use crate::{
    lead_input::{self, BoardCause},
    lead_page::{self, Alarms},
};
use oxa_proto::work::{LocalAcceptance, PullRequest, Snapshot};
use oxa_work::{
    board::Board,
    shadow::{AppQuota, Deferral, ThreadHold},
};
use std::{collections::BTreeMap, path::Path};

/// A runner situation more than this far in the future or past is not claimed.
const SITUATION_FUTURE_MS: u64 = 60_000;
const SITUATION_AGE_MS: u64 = 900_000;

pub(crate) struct Observation {
    pub board: Board,
    /// Why the Board sample could not be read; the board is then the default one.
    pub cause: Option<BoardCause>,
    pub alarms: Alarms,
    /// The App's GitHub quota windows as the Reader sampled them.
    pub app_quota: Option<AppQuota>,
    /// Unresolved review threads by PR; `None` when the sample has no landing record.
    pub threads: Option<BTreeMap<u64, ThreadHold>>,
    /// PRs whose reviewer refused the configured requester account.
    pub review_rejected: Vec<u64>,
    /// Optional GitHub work the owner's App reserve deferred in this sample.
    pub quota_deferred: Vec<Deferral>,
    /// Verified main merges observed across the sample tail (#2346), decoded in
    /// the same read as the board; the throughput projection's only input.
    pub merges: crate::lead_projection::MergeHistory,
    /// PRs held by `Restricts:` without `owner-accepted`; `None` when unobserved.
    pub restricts_held: Option<Vec<u64>>,
    /// The Lead session's workers with their lanes and live state; `None` when no owner
    /// session was bound, an explicit error when the registry is absent or refused.
    pub bindings: Option<Result<Vec<Binding>, BindingsError>>,
    /// The runner situation, only while it is inside its freshness window.
    pub situation: Option<Snapshot>,
    pub now_ms: u64,
}

/// One PR held by review threads, with its exact head and queue membership.
pub(crate) struct ThreadHeld<'a> {
    pub number: u64,
    pub head: &'a str,
    pub hold: &'a ThreadHold,
    pub queued: bool,
}

impl Observation {
    pub(crate) fn read(board_path: &Path, situation_path: Option<&Path>, uid: u32, now_ms: u64) -> Self {
        // The default board keeps held asks and the stale policy; only a fixed
        // allowlisted cause and its hint reach operator status.
        let (board, alarms, quota, threads, rejected, deferred, merges, restricts, cause) =
            match lead_input::sample(board_path, uid) {
                Ok(s) => (
                    s.board,
                    s.alarms,
                    s.app_quota,
                    s.threads,
                    s.review_rejected,
                    s.quota_deferred,
                    s.merges,
                    s.restricts_held,
                    None,
                ),
                Err(error) => (
                    Board::default(),
                    Alarms::default(),
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    crate::lead_projection::MergeHistory::default(),
                    None,
                    Some(lead_input::board_cause(error)),
                ),
            };
        let situation = situation_path
            .and_then(|path| lead_input::situation(path, uid).ok())
            .filter(|s| {
                s.at_ms <= now_ms.saturating_add(SITUATION_FUTURE_MS)
                    && now_ms.saturating_sub(s.at_ms) <= SITUATION_AGE_MS
            });
        Self {
            board,
            cause,
            alarms,
            app_quota: quota,
            threads,
            review_rejected: rejected,
            quota_deferred: deferred,
            merges,
            restricts_held: restricts,
            bindings: None,
            situation,
            now_ms,
        }
    }

    /// Add the worker bindings of the Lead's own session: its owner binding's session
    /// id, never a session list.
    pub(crate) fn with_workers(mut self, agent_dir: &Path, sessions_dir: &Path, parent: &str) -> Self {
        self.bindings = Some(crate::workers::tool::bindings(agent_dir, sessions_dir, parent));
        self
    }

    /// Workers whose process is gone or was stopped.
    pub(crate) fn gone_workers(&self) -> Vec<&str> {
        use crate::workers::registry::WorkerState;
        match &self.bindings {
            Some(Ok(bindings)) => bindings
                .iter()
                .filter(|b| matches!(b.state, WorkerState::Dead | WorkerState::Stopped))
                .map(|b| b.name.as_str())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A stale Board record never manufactures an escalation.
    pub(crate) fn fresh(&self) -> bool {
        !lead_page::stale(&self.board, self.now_ms / 1000)
    }

    /// Queue-age alarms for PRs still in the queue at the alarmed head.
    pub(crate) fn overdue_in_queue(&self) -> Option<Vec<&PullRequest>> {
        self.alarms.overdue_in_queue()
    }

    /// A local acceptance is published only by a publication of the same task whose
    /// PR head is the accepted commit; a later local commit is not published.
    pub(crate) fn unpublished(&self) -> Vec<&LocalAcceptance> {
        let Some(situation) = &self.situation else {
            return Vec::new();
        };
        situation
            .local_accepted
            .iter()
            .filter(|acceptance| {
                !situation
                    .published
                    .iter()
                    .any(|p| p.task == acceptance.task && p.pr.head == acceptance.commit)
            })
            .collect()
    }

    /// PRs with unresolved review threads, with exact-head queue membership.
    pub(crate) fn thread_holds(&self) -> Vec<ThreadHeld<'_>> {
        let Some(threads) = &self.threads else {
            return Vec::new();
        };
        self.board
            .entries
            .iter()
            .filter_map(|entry| {
                let hold = threads.get(&entry.number).filter(|hold| hold.unresolved > 0)?;
                let queued = self.alarms.queue.as_deref().is_some_and(|queue| {
                    queue
                        .iter()
                        .any(|pr| pr.number == entry.number && pr.head == entry.head)
                });
                Some(ThreadHeld {
                    number: entry.number,
                    head: &entry.head,
                    hold,
                    queued,
                })
            })
            .collect()
    }
}
