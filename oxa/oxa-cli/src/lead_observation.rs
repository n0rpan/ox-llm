//! One Lead observation per tick. The loop reads the protected Board sample and the
//! runner situation once; every stuck-work rule is a pure derivation over the result,
//! so no rule makes its own partial read (#2088, #2090, #2091).
use crate::{
    lead_input::{self, BoardCause},
    lead_page::{self, Alarms},
};
use oxa_proto::work::{LocalAcceptance, PullRequest, Snapshot};
use oxa_work::{
    board::Board,
    shadow::{Deferral, ThreadHold},
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
    /// Unresolved review threads by PR; `None` when the sample has no landing record.
    pub threads: Option<BTreeMap<u64, ThreadHold>>,
    /// PRs whose reviewer refused the configured requester account.
    pub review_rejected: Vec<u64>,
    /// Optional GitHub work the owner's App reserve deferred in this sample.
    pub quota_deferred: Vec<Deferral>,
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
        let (board, alarms, threads, rejected, deferred, cause) = match lead_input::sample(board_path, uid) {
            Ok(s) => (
                s.board,
                s.alarms,
                s.threads,
                s.review_rejected,
                s.quota_deferred,
                None,
            ),
            Err(error) => (
                Board::default(),
                Alarms::default(),
                None,
                Vec::new(),
                Vec::new(),
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
            threads,
            review_rejected: rejected,
            quota_deferred: deferred,
            situation,
            now_ms,
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
