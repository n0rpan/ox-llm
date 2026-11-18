//! Bounded, in-memory follow history for an existing session owner.
//!
//! Extracted from #129 f89ba2e, control.rs::Publisher and Request::Events.
//! This is not a second session/task owner, transport, or persistence format.
//! Callers supply approved summaries: this module does not redact arbitrary
//! payloads, capture transcripts, or authorize their disclosure to a follower.
use serde_json::Value;
use std::collections::VecDeque;

pub mod publisher;

const HISTORY_BYTES: usize = 512 * 1024;
const HISTORY_EVENTS: usize = 128;
const PAGE_EVENTS: usize = 16;

/// One page uses an exclusive `after` cursor, matching #129's control API.
#[derive(Debug)]
pub struct Page {
    pub events: Vec<(u64, Value)>,
    /// Last returned sequence, or the requested cursor for an empty page.
    /// Resume here, never at `latest` while unread events remain.
    pub next: u64,
    pub latest: u64,
    /// Events after the requested cursor have been evicted. Recover a snapshot
    /// from the owning session before relying on incremental updates again.
    pub gap: bool,
}

/// Session-owned retention; construction performs no I/O or runtime registration.
#[derive(Default)]
pub struct History {
    sequence: u64,
    events: VecDeque<(u64, Value, usize)>,
    bytes: usize,
}

impl History {
    /// Publish one approved summary. Invalid input leaves history and cursors
    /// unchanged. An individual event must fit the total retention budget.
    pub fn publish(&mut self, event: Value) -> Result<u64, &'static str> {
        let bytes = event.to_string().len();
        if bytes > HISTORY_BYTES {
            return Err("event exceeds follow history byte limit");
        }
        let sequence = self.sequence.checked_add(1).ok_or("sequence exhausted")?;
        self.events.push_back((sequence, event, bytes));
        self.sequence = sequence;
        self.bytes += bytes;
        while self.events.len() > HISTORY_EVENTS || self.bytes > HISTORY_BYTES {
            if let Some((_, _, bytes)) = self.events.pop_front() {
                self.bytes -= bytes;
            }
        }
        Ok(sequence)
    }

    /// Read a bounded page without consuming another follower's history.
    pub fn page(&self, after: u64) -> Result<Page, &'static str> {
        if after > self.sequence {
            return Err("invalid follow cursor");
        }
        let first = self.events.front().map_or(self.sequence, |e| e.0);
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.0 > after)
            .take(PAGE_EVENTS)
            .map(|(sequence, event, _)| (*sequence, event.clone()))
            .collect();
        let next = events.last().map_or(after, |e| e.0);
        Ok(Page {
            events,
            next,
            latest: self.sequence,
            gap: after.saturating_add(1) < first,
        })
    }
}
