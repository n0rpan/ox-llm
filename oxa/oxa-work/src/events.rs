//! Durable facts journal and its bounded read view. Never opens runner task-state files.
use crate::facts_io::Root;
use crate::{situation, Result};
use oxa_proto::work::{
    Checkpoint, Event, EventPage, Fact, Snapshot, MAX_EVENT_BYTES, MAX_PAGE_BYTES, MAX_SNAPSHOT_BYTES,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

mod digest;
pub use digest::{Digest, DigestChanges, DigestGap};

const CURRENT: &str = "events.jsonl";
const PREVIOUS: &str = "events.previous.jsonl";
const STATE: &str = "facts.json";
const SITUATION: &str = "situation.json";
const LOCK: &str = ".facts.lock";
const JOURNAL_BYTES: usize = 256 * 1024;
const STATE_BYTES: usize = 256 * 1024;
const MAX_SOURCES: usize = 16;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    last_seq: u64,
    last_by_source: Vec<Value>,
    // Write-ahead observation: a crash can replay it even if the journal line was torn.
    pending: Option<Value>,
    // A cross-store reservation is unpublished until its owner syncs prepared data.
    #[serde(default, skip_serializing_if = "not_preparing")]
    preparing: bool,
    #[serde(default, skip_serializing_if = "not_preparing")]
    standing_reserved: bool,
}
fn not_preparing(value: &bool) -> bool {
    !value
}
impl State {
    fn sources(&self) -> Result<Vec<Event>> {
        if self.version != 1 || self.last_by_source.len() > MAX_SOURCES {
            return Err("unsupported facts metadata or source capacity".into());
        }
        if self.preparing && self.pending.is_none() {
            return Err("facts preparation has no reserved observation".into());
        }
        let events: Vec<Event> = self
            .last_by_source
            .iter()
            .map(read_event_value)
            .collect::<Result<_>>()?;
        let mut sources = std::collections::HashSet::new();
        let mut sequences = std::collections::HashSet::new();
        for event in &events {
            if event.seq > self.last_seq
                || !sources.insert(&event.checkpoint.source)
                || !sequences.insert(event.seq)
            {
                return Err("inconsistent facts source metadata".into());
            }
        }
        if events.iter().map(|event| event.seq).max().unwrap_or(0) != self.last_seq {
            return Err("facts source metadata has no latest observation".into());
        }
        Ok(events)
    }
    fn committed(&mut self, event: &Event) -> Result<()> {
        let mut sources = self.sources()?;
        sources.retain(|old| old.checkpoint.source != event.checkpoint.source);
        sources.push(event.clone());
        if sources.len() > MAX_SOURCES {
            return Err("facts source capacity exceeded".into());
        }
        self.last_seq = event.seq;
        self.last_by_source = sources.iter().map(event_value).collect::<Result<_>>()?;
        self.pending = None;
        self.preparing = false;
        Ok(())
    }
    fn bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| "cannot encode facts metadata")?;
        if bytes.len() > STATE_BYTES {
            return Err("facts metadata capacity exceeded".into());
        }
        Ok(bytes)
    }
}
fn event_value(event: &Event) -> Result<Value> {
    serde_json::from_slice(&event.to_json().map_err(|error| error.to_string())?)
        .map_err(|_| "cannot encode fact".into())
}
fn read_event_value(value: &Value) -> Result<Event> {
    Event::from_json(&serde_json::to_vec(value).map_err(|_| "invalid fact metadata")?)
        .map_err(|error| error.to_string())
}

struct Journal {
    events: Vec<Event>,
    current_bytes: usize,
    torn_at: Option<u64>,
}
impl Journal {
    fn last_seq(&self) -> u64 {
        self.events.last().map_or(0, |event| event.seq)
    }
}

#[derive(Clone, Debug)]
pub struct Facts {
    root: Root,
    journal_bytes: usize,
}
impl Facts {
    /// Explicit first initialization. Existing or partial artifacts are never reset to empty.
    /// Supply <protected repository root>/facts, outside writable workspace/Git binds.
    pub fn initialize(root: &Path) -> Result<Self> {
        let root = Root::open(root, true)?;
        let _lock = root.lock(LOCK)?;
        for leaf in [CURRENT, PREVIOUS, STATE, SITUATION] {
            if root.exists(leaf)? {
                return Err("facts store already exists or initialization is incomplete".into());
            }
        }
        root.create(CURRENT, b"")?;
        root.create(
            SITUATION,
            &situation::empty().to_json().map_err(|error| error.to_string())?,
        )?;
        root.create(
            STATE,
            &State {
                version: 1,
                ..State::default()
            }
            .bytes()?,
        )?;
        Ok(Self {
            root,
            journal_bytes: JOURNAL_BYTES,
        })
    }
    pub fn open(root: &Path) -> Result<Self> {
        let facts = Self {
            root: Root::open(root, false)?,
            journal_bytes: JOURNAL_BYTES,
        };
        facts.situation()?;
        Ok(facts)
    }
    /// One event per durable source/revision. A retry of the latest revision returns its
    /// original event; older revisions and different contents under that identity are errors.
    /// Stop before the next action if this call fails. Replaying the same checkpoint repairs
    /// a lost acknowledgement, a torn last line or an interrupted snapshot replacement.
    pub fn append(&self, checkpoint: Checkpoint, at_ms: u64, fact: Fact) -> Result<Event> {
        let reserved_ask = match &fact {
            Fact::AskOpened(ask) => ask.id.starts_with("standing:"),
            Fact::AskClosed(ask) => ask.id.starts_with("standing:"),
            _ => false,
        };
        if checkpoint.source == "standing" || matches!(fact, Fact::StandingChanged(_)) || reserved_ask {
            return Err("standing observations require the protected prepared path".into());
        }
        self.append_with(checkpoint, at_ms, fact, None::<fn(&Event) -> Result<()>>)
    }
    /// Reserve capacity under the facts lock without inventing a standing event.
    /// A failed initialization can retry the same reservation; ordinary stores
    /// retain their full source capacity until standing explicitly claims a slot.
    pub(crate) fn reserve_standing(&self) -> Result<()> {
        let _lock = self.root.lock(LOCK)?;
        let (mut state, _, _) = self.recover()?;
        let sources = state.sources()?;
        if !sources.iter().any(|event| event.checkpoint.source == "standing") && sources.len() == MAX_SOURCES
        {
            return Err("facts source capacity unavailable for standing".into());
        }
        if state.standing_reserved {
            // A previous rename may be visible even though its directory sync failed.
            self.root.durable(STATE)?;
        } else {
            state.standing_reserved = true;
            self.root.atomic(STATE, &state.bytes()?)?;
        }
        Ok(())
    }
    /// Durably reserve capacity before the protected store prepares its mutation. A failed
    /// preparation blocks other appenders until that store commits or cancels the reservation.
    /// Never calls preparation for an already committed identity.
    pub(crate) fn append_prepared(
        &self,
        checkpoint: Checkpoint,
        at_ms: u64,
        fact: Fact,
        prepare: impl FnOnce(&Event) -> Result<()>,
    ) -> Result<Event> {
        self.append_with(checkpoint, at_ms, fact, Some(prepare))
    }
    fn append_with(
        &self,
        checkpoint: Checkpoint,
        at_ms: u64,
        fact: Fact,
        prepare: Option<impl FnOnce(&Event) -> Result<()>>,
    ) -> Result<Event> {
        if prepare.is_some() && (!matches!(fact, Fact::StandingChanged(_)) || checkpoint.source != "standing")
        {
            return Err("only standing observations use protected preparation".into());
        }
        let _lock = self.root.lock(LOCK)?;
        let (mut state, mut snapshot, journal) = self.recover()?;
        let sources = state.sources()?;
        let reserved = state.preparing;
        let event = if reserved {
            if prepare.is_none() {
                return Err("standing preparation must recover before another fact".into());
            }
            let event = read_event_value(state.pending.as_ref().ok_or("missing reserved preparation")?)?;
            if event.checkpoint != checkpoint || event.at_ms != at_ms || event.fact != fact {
                return Err("another standing preparation is reserved".into());
            }
            self.root.durable(STATE)?;
            event
        } else {
            if let Some(previous) = sources
                .iter()
                .find(|event| event.checkpoint.source == checkpoint.source)
            {
                if checkpoint.revision == previous.checkpoint.revision {
                    return if previous.fact == fact {
                        Ok(previous.clone())
                    } else {
                        Err("checkpoint identity has conflicting facts".into())
                    };
                }
                if checkpoint.revision < previous.checkpoint.revision {
                    return Err("stale facts checkpoint".into());
                }
            } else if sources.len() == MAX_SOURCES
                || (state.standing_reserved
                    && checkpoint.source != "standing"
                    && !sources.iter().any(|event| event.checkpoint.source == "standing")
                    && sources.len() == MAX_SOURCES - 1)
            {
                return Err("facts source capacity exceeded".into());
            }
            Event {
                seq: state.last_seq.checked_add(1).ok_or("facts sequence exhausted")?,
                at_ms,
                checkpoint,
                fact,
            }
        };
        let mut bytes = event.to_json().map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        if bytes.len() > self.journal_bytes {
            return Err("event exceeds journal capacity".into());
        }
        // Fail capacity/semantic checks before durably reserving an observation.
        situation::apply(&mut snapshot, &event)?;
        if !reserved {
            state.pending = Some(event_value(&event)?);
            state.preparing = prepare.is_some();
            self.root.atomic(STATE, &state.bytes()?)?;
        }
        if let Some(prepare) = prepare {
            prepare(&event)?;
            state.preparing = false;
            self.root.atomic(STATE, &state.bytes()?)?;
        }
        if journal.current_bytes + bytes.len() > self.journal_bytes {
            self.root.rotate(CURRENT, PREVIOUS)?;
        }
        self.root.append(CURRENT, &bytes)?;
        state.committed(&event)?;
        self.root.atomic(STATE, &state.bytes()?)?;
        self.root
            .atomic(SITUATION, &snapshot.to_json().map_err(|error| error.to_string())?)?;
        Ok(event)
    }
    /// The standing lock is held and its durable document has no pending mutation. A matching
    /// next-revision reservation therefore has no prepared data and can be discarded safely.
    pub(crate) fn cancel_unprepared_standing(&self, revision: u64, goal: Option<&str>) -> Result<Snapshot> {
        let _lock = self.root.lock(LOCK)?;
        let (mut state, snapshot, _) = self.recover()?;
        if snapshot.standing_revision != revision || snapshot.goal.as_deref() != goal {
            return Err("standing file disagrees with facts revision".into());
        }
        if state.preparing {
            let event = read_event_value(state.pending.as_ref().ok_or("missing reserved preparation")?)?;
            if event.checkpoint.source != "standing"
                || revision.checked_add(1) != Some(event.checkpoint.revision)
            {
                return Err("standing preparation disagrees with current policy".into());
            }
            state.pending = None;
            state.preparing = false;
            self.root.atomic(STATE, &state.bytes()?)?;
        }
        Ok(snapshot)
    }
    pub fn situation(&self) -> Result<Snapshot> {
        let _lock = self.root.lock(LOCK)?;
        self.recover().map(|(_, snapshot, _)| snapshot)
    }
    pub fn events_after(&self, after: u64, limit: usize) -> Result<EventPage> {
        self.read_window(after, limit).map(|(_, page)| page)
    }
    fn read_window(&self, after: u64, limit: usize) -> Result<(Snapshot, EventPage)> {
        if !(1..=64).contains(&limit) {
            return Err("event page limit must be between 1 and 64".into());
        }
        let _lock = self.root.lock(LOCK)?;
        let (state, snapshot, journal) = self.recover()?;
        if after > state.last_seq {
            return Err("event cursor is ahead of the journal".into());
        }
        let first_available = journal.events.first().map_or(1, |event| event.seq);
        let mut page = EventPage {
            after,
            events: Vec::new(),
            next: after.max(first_available - 1),
            first_available,
            gap: after.saturating_add(1) < first_available,
        };
        // The envelope's next cursor can grow to last_seq. An event's standalone wire
        // encoding bounds its nested encoding; counting one comma per event is conservative.
        // Encode each candidate once instead of repeatedly serializing a growing page.
        let mut bytes = page.to_json().map_err(|error| error.to_string())?.len()
            + state
                .last_seq
                .to_string()
                .len()
                .saturating_sub(page.next.to_string().len());
        for event in journal
            .events
            .into_iter()
            .filter(|event| event.seq > after)
            .take(limit)
        {
            let event_bytes = event.to_json().map_err(|error| error.to_string())?.len() + 1;
            if bytes + event_bytes > MAX_PAGE_BYTES {
                if page.events.is_empty() {
                    return Err("event exceeds bounded page capacity".into());
                }
                break;
            }
            bytes += event_bytes;
            page.next = event.seq;
            page.events.push(event);
        }
        page.to_json().map_err(|error| error.to_string())?;
        Ok((snapshot, page))
    }
    fn journal(&self, pending: bool) -> Result<Journal> {
        // The only allowed missing current file is an interrupted, durably reserved rotation.
        if !self.root.exists(CURRENT)? {
            if !pending {
                return Err("facts journal is missing".into());
            }
            self.root.create(CURRENT, b"")?;
        }
        let mut events = Vec::new();
        if self.root.exists(PREVIOUS)? {
            let bytes = self.root.read(PREVIOUS, self.journal_bytes)?;
            parse_lines(&bytes, false, &mut events)?;
        }
        let bytes = self.root.read(CURRENT, self.journal_bytes)?;
        let torn_at = parse_lines(&bytes, pending, &mut events)?;
        for pair in events.windows(2) {
            if pair[0].seq.checked_add(1) != Some(pair[1].seq) {
                return Err("facts journal has a sequence gap".into());
            }
        }
        Ok(Journal {
            events,
            current_bytes: bytes.len(),
            torn_at,
        })
    }
    fn recover(&self) -> Result<(State, Snapshot, Journal)> {
        let mut state: State = serde_json::from_slice(&self.root.read(STATE, STATE_BYTES)?)
            .map_err(|_| "invalid facts metadata")?;
        let sources = state.sources()?;
        let pending = state.pending.as_ref().map(read_event_value).transpose()?;
        let mut journal = self.journal(pending.is_some() && !state.preparing)?;
        let mut snapshot = Snapshot::from_json(&self.root.read(SITUATION, MAX_SNAPSHOT_BYTES)?)
            .map_err(|error| error.to_string())?;
        let original_seq = snapshot.as_of_seq;
        if snapshot.as_of_seq > state.last_seq {
            return Err("situation is ahead of committed facts".into());
        }
        for event in journal
            .events
            .iter()
            .filter(|event| event.seq > original_seq && event.seq <= state.last_seq)
        {
            situation::apply(&mut snapshot, event)?;
        }
        if snapshot.as_of_seq != state.last_seq {
            return Err("retained journal cannot recover the situation".into());
        }
        if let Some(event) = pending {
            if state.last_seq.checked_add(1) != Some(event.seq)
                || sources.iter().any(|old| {
                    old.checkpoint.source == event.checkpoint.source
                        && old.checkpoint.revision >= event.checkpoint.revision
                })
                || !sources
                    .iter()
                    .any(|old| old.checkpoint.source == event.checkpoint.source)
                    && sources.len() >= MAX_SOURCES
            {
                return Err("invalid pending facts checkpoint".into());
            }
            if state.preparing {
                if !matches!(event.fact, Fact::StandingChanged(_))
                    || journal.last_seq() != state.last_seq
                    || journal.torn_at.is_some()
                {
                    return Err("invalid unpublished standing preparation".into());
                }
                // Reads report committed facts only. Append cannot consume this capacity;
                // the owning standing store must first commit or cancel its preparation.
                situation::apply(&mut snapshot.clone(), &event)?;
            } else {
                situation::apply(&mut snapshot, &event)?;
                if journal.last_seq() == state.last_seq {
                    if let Some(length) = journal.torn_at {
                        self.root.truncate(CURRENT, length)?;
                        journal.current_bytes = length as usize;
                    }
                    let mut bytes = event.to_json().map_err(|error| error.to_string())?;
                    bytes.push(b'\n');
                    if bytes.len() > self.journal_bytes {
                        return Err("event exceeds journal capacity".into());
                    }
                    if journal.current_bytes + bytes.len() > self.journal_bytes {
                        self.root.rotate(CURRENT, PREVIOUS)?;
                    }
                    self.root.append(CURRENT, &bytes)?;
                } else if journal.events.last() != Some(&event) || journal.torn_at.is_some() {
                    return Err("pending fact disagrees with the journal".into());
                } else {
                    // Visibility is not durability: the original append may have died before sync.
                    let leaf = if journal.current_bytes == 0 {
                        PREVIOUS
                    } else {
                        CURRENT
                    };
                    self.root.append(leaf, b"")?;
                }
                state.committed(&event)?;
                self.root.atomic(STATE, &state.bytes()?)?;
                journal = self.journal(false)?;
            }
        } else if journal.last_seq() != state.last_seq {
            return Err("journal disagrees with committed facts".into());
        }
        if snapshot.as_of_seq != original_seq {
            self.root
                .atomic(SITUATION, &snapshot.to_json().map_err(|error| error.to_string())?)?;
        }
        Ok((state, snapshot, journal))
    }
}

fn parse_lines(bytes: &[u8], allow_torn: bool, events: &mut Vec<Event>) -> Result<Option<u64>> {
    let mut offset = 0usize;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            return if allow_torn {
                Ok(Some(offset as u64))
            } else {
                Err("facts journal has an unreserved torn line".into())
            };
        }
        if line.len() > MAX_EVENT_BYTES + 1 {
            return Err("facts journal line capacity exceeded".into());
        }
        events.push(Event::from_json(&line[..line.len() - 1]).map_err(|error| error.to_string())?);
        offset += line.len();
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
