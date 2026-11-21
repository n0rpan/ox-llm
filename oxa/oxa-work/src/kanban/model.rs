//! Typed kanban board state: items, goal, capacity and the operations that change them.
//! The board is owner-private work state, not GitHub policy (`crate::board`) and not a
//! standing rule (`crate::standing`). Text is bounded; nothing here starts a worker.
use crate::board::{Action, RequiredCi};
use crate::standing::Authority;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub type HostId = String;

/// Supplied by the trusted frontend, never read from an op or request body (as `Authority`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Owner,
    Lead,
    /// A worker may only note and stick the one item the dispatcher gave it.
    Worker(u32),
    System,
}
impl Actor {
    pub(crate) fn authority(&self) -> Option<Authority> {
        match self {
            Self::Owner => Some(Authority::Owner),
            Self::Lead => Some(Authority::Lead),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Backlog,
    Ready,
    Running,
    PrOpen,
    InQueue,
    Merged,
}
impl Column {
    /// The GitHub-following lanes only the system (dispatcher/reconciler) may move into.
    pub(crate) fn system_only(self) -> bool {
        matches!(self, Self::Running | Self::PrOpen | Self::InQueue | Self::Merged)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dispatch {
    Paused,
    Running,
    GoalMet,
    Starved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    Arm64,
    X86,
    RamHeavy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPref {
    Any,
    Prefer(HostId),
    Only(HostId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    WorkerFailed,
    WorkerStalled,
    NoPr,
    CiRed,
    ReviewBlocked,
    Conflict,
    QueueRejected,
    PrClosed,
    NeedsOwner(String),
    HostDown,
    NoModel,
    Manual,
}
impl Cause {
    /// `Manual` is the one cause a person sets; the rest are automatic (system) causes.
    pub(crate) fn automatic(&self) -> bool {
        !matches!(self, Self::Manual)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stuck {
    pub cause: Cause,
    pub detail: String,
    pub since: u64,
    pub by: Actor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub worker: String,
    pub host: HostId,
    pub model: String,
    pub session_id: String,
    pub started_at: u64,
    pub attempt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pr {
    pub number: u64,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default = "pending_ci")]
    pub ci: RequiredCi,
    #[serde(default = "wait_ci")]
    pub next: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
}
fn pending_ci() -> RequiredCi {
    RequiredCi::Pending
}
fn wait_ci() -> Action {
    Action::WaitRequiredCi
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: u32,
    pub rev: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
    pub done_when: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub host: HostPref,
    pub needs: BTreeSet<Need>,
    pub column: Column,
    pub rank: u32,
    pub rank_by: Authority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck: Option<Stuck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<Pr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: Authority,
    pub created_at: u64,
    pub updated_at: u64,
    pub column_since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<u64>,
}

/// A measurable done-when for the goal. Evaluation of the non-manual kinds lands with the
/// reconciler; a manual measure is ticked by the owner (or the lead, with evidence).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measure {
    pub id: String,
    #[serde(flatten)]
    pub kind: MeasureKind,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "measure", rename_all = "snake_case", deny_unknown_fields)]
pub enum MeasureKind {
    IssuesClosed {
        #[serde(default)]
        numbers: Vec<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        milestone: Option<String>,
    },
    ItemsMerged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<u32>,
    },
    MergedCount {
        n: u32,
        since: u64,
    },
    Manual {
        acceptance: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<MeasureResult>,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasureResult {
    pub passed: bool,
    pub evidence: String,
    pub at: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub text: String,
    pub measures: Vec<Measure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_per_hour: Option<f32>,
    pub set_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub met_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    pub slots: u32,
    pub paused: bool,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kanban {
    pub schema: u32,
    pub revision: u64,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
    pub capacity: BTreeMap<HostId, Capacity>,
    pub dispatch: Dispatch,
    pub items: Vec<Item>,
    pub next_id: u32,
}
impl Kanban {
    pub(crate) fn new(repo: &str) -> Self {
        Self {
            schema: 1,
            revision: 0,
            repo: repo.to_string(),
            goal: None,
            capacity: BTreeMap::new(),
            dispatch: Dispatch::Paused,
            items: Vec::new(),
            next_id: 1,
        }
    }
    pub fn item(&self, id: u32) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }
    /// Items of one column in pull order (dense rank).
    pub fn lane(&self, column: Column) -> Vec<&Item> {
        let mut lane: Vec<&Item> = self.items.iter().filter(|i| i.column == column).collect();
        lane.sort_by_key(|i| i.rank);
        lane
    }
}

/// The editable text fields of an item. `None` leaves a field unchanged; a nullable field
/// uses `Some(None)` to clear it. Only a text edit carries the item `rev`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostPref>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<BTreeSet<Need>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<Option<u64>>,
}
impl EditFields {
    pub(crate) fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.done_when.is_none()
            && self.kind.is_none()
            && self.model.is_none()
            && self.host.is_none()
            && self.needs.is_none()
            && self.issue.is_none()
    }
    /// The item as the editor wants it: the current state with these fields applied.
    pub(crate) fn applied_to(&self, item: &Item) -> Item {
        let mut next = item.clone();
        if let Some(v) = &self.title {
            next.title = v.clone();
        }
        if let Some(v) = &self.done_when {
            next.done_when = v.clone();
        }
        if let Some(v) = &self.kind {
            next.kind = v.clone();
        }
        if let Some(v) = &self.model {
            next.model = v.clone();
        }
        if let Some(v) = &self.host {
            next.host = v.clone();
        }
        if let Some(v) = &self.needs {
            next.needs = v.clone();
        }
        if let Some(v) = &self.issue {
            next.issue = *v;
        }
        next
    }
}

/// A child produced by a split; it inherits the parent's kind, host, needs and column.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewChild {
    pub title: String,
    pub done_when: String,
}

/// One typed change. The actor is a separate argument to `Store::apply`, never a field here.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    Add {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue: Option<u64>,
        #[serde(default)]
        done_when: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default = "any_host")]
        host: HostPref,
        #[serde(default)]
        needs: BTreeSet<Need>,
        #[serde(default = "backlog")]
        column: Column,
    },
    Edit {
        id: u32,
        rev: u32,
        fields: EditFields,
    },
    Move {
        id: u32,
        to: Column,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<u32>,
    },
    Reorder {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<u32>,
    },
    Split {
        id: u32,
        rev: u32,
        children: Vec<NewChild>,
    },
    SetStuck {
        id: u32,
        cause: Cause,
        #[serde(default)]
        detail: String,
    },
    Unstick {
        id: u32,
    },
    Remove {
        id: u32,
    },
    Note {
        id: u32,
        text: String,
    },
    SetCapacity {
        host: HostId,
        slots: u32,
        #[serde(default)]
        paused: bool,
    },
    SetGoal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<Goal>,
    },
    CheckMeasure {
        id: String,
        result: MeasureResult,
    },
    SetDispatch {
        state: Dispatch,
    },
    LinkPr {
        id: u32,
        number: u64,
    },
    Undo {
        seq: u64,
    },
}
fn any_host() -> HostPref {
    HostPref::Any
}
fn backlog() -> Column {
    Column::Backlog
}

/// The reverse of an applied op, recorded so `Undo` can restore the prior state.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "inverse", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Inverse {
    /// An item op: restore `prior` (or remove the item when `prior` is none, undoing an add).
    Item {
        id: u32,
        prior: Option<Box<Item>>,
        post_rev: u32,
    },
    /// Undo a remove: re-add the exact item.
    Restore {
        item: Box<Item>,
    },
    /// Undo a split: drop the children, restore the parent.
    Unsplit {
        parent: Box<Item>,
        children: Vec<u32>,
        child_revs: Vec<(u32, u32)>,
    },
    Capacity {
        host: HostId,
        prior: Option<Capacity>,
    },
    Goal {
        prior: Option<Box<Goal>>,
    },
    Measure {
        id: String,
        prior: Option<MeasureResult>,
    },
    Dispatch {
        prior: Dispatch,
    },
    /// An undo is itself not undoable in this slice.
    None,
}

/// One applied operation in `ops.jsonl`: the audit trail, undo source and flow-metrics input.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogEntry {
    pub seq: u64,
    pub at: u64,
    pub actor: Actor,
    pub op: Op,
    pub inverse: Inverse,
}

/// What a successful apply returns: the operation's sequence and the new board revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    pub seq: u64,
    pub revision: u64,
}

/// A refusal keeps the two safe versions of a stale text edit so the front end can show both.
#[derive(Clone, PartialEq, Eq)]
pub enum Error {
    Storage(String),
    Refused(String),
    Stale {
        current: Box<Item>,
        attempted: Box<Item>,
    },
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(e) => write!(f, "kanban store unavailable: {e}"),
            Self::Refused(e) => write!(f, "kanban op refused: {e}"),
            Self::Stale { .. } => write!(f, "kanban item changed since it was opened"),
        }
    }
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(e) => write!(f, "Storage({e:?})"),
            Self::Refused(e) => write!(f, "Refused({e:?})"),
            Self::Stale { .. } => f.write_str("Stale(<both versions withheld>)"),
        }
    }
}
impl std::error::Error for Error {}
impl From<String> for Error {
    fn from(e: String) -> Self {
        Self::Storage(e)
    }
}

pub(crate) const MAX_ITEMS: usize = 512;
pub(crate) const MAX_BOARD_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_OPS_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_TITLE: usize = 200;
pub(crate) const MAX_DONE_WHEN: usize = 500;
pub(crate) const MAX_DETAIL: usize = 500;
pub(crate) const MAX_NOTE: usize = 1000;
pub(crate) const MAX_GOAL_TEXT: usize = 1000;
pub(crate) const MAX_MEASURES: usize = 32;
pub(crate) const MAX_CHILDREN: usize = 16;
pub(crate) const MAX_SLOTS: u32 = 1024;

/// Bounded single-line text without control characters (as the standing store requires).
pub(crate) fn text(value: &str, max_chars: usize, required: bool) -> Result<(), String> {
    if value.chars().count() > max_chars
        || value
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        || (required && value.trim().is_empty())
    {
        return Err("kanban text must be bounded and single-line".into());
    }
    Ok(())
}

pub(crate) fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 64
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// A job kind follows the worker model policy rule: lowercase letters, digits, dashes, <= 32.
pub(crate) fn valid_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 32
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn validate_item(item: &Item) -> Result<(), String> {
    text(&item.title, MAX_TITLE, true)?;
    text(&item.done_when, MAX_DONE_WHEN, false)?;
    if !valid_kind(&item.kind) {
        return Err("kanban kind must be a valid job kind".into());
    }
    if let Some(model) = &item.model {
        text(model, 200, true)?;
    }
    if let Some(note) = &item.note {
        text(note, MAX_NOTE, true)?;
    }
    if let Some(stuck) = &item.stuck {
        text(&stuck.detail, MAX_DETAIL, false)?;
        if let Cause::NeedsOwner(ask) = &stuck.cause {
            text(ask, 128, true)?;
        }
    }
    match &item.host {
        HostPref::Any => {}
        HostPref::Prefer(h) | HostPref::Only(h) => {
            if !valid_host(h) {
                return Err("kanban host must be a valid host id".into());
            }
        }
    }
    if item.column == Column::Ready && item.done_when.trim().is_empty() {
        return Err("a Ready item needs a done-when".into());
    }
    Ok(())
}

pub(crate) fn validate_goal(goal: &Goal) -> Result<(), String> {
    text(&goal.text, MAX_GOAL_TEXT, true)?;
    if goal.measures.len() > MAX_MEASURES {
        return Err("kanban goal has too many measures".into());
    }
    if let Some(target) = goal.target_per_hour {
        if !target.is_finite() || target < 0.0 {
            return Err("kanban target must be a finite non-negative rate".into());
        }
    }
    let mut ids = BTreeSet::new();
    for measure in &goal.measures {
        text(&measure.id, 64, true)?;
        if !ids.insert(&measure.id) {
            return Err("kanban measure ids must be unique".into());
        }
        match &measure.kind {
            MeasureKind::IssuesClosed {
                numbers,
                label,
                milestone,
            } => {
                if numbers.is_empty() && label.is_none() && milestone.is_none() {
                    return Err("an issues-closed measure needs a selector".into());
                }
                if numbers.len() > 256 {
                    return Err("too many issue numbers in a measure".into());
                }
                if let Some(l) = label {
                    text(l, 128, true)?;
                }
                if let Some(m) = milestone {
                    text(m, 128, true)?;
                }
            }
            MeasureKind::ItemsMerged { .. } => {}
            MeasureKind::MergedCount { n, .. } => {
                if *n == 0 {
                    return Err("a merged-count measure needs a positive target".into());
                }
            }
            MeasureKind::Manual { acceptance, result } => {
                text(acceptance, MAX_DONE_WHEN, true)?;
                if let Some(r) = result {
                    validate_measure_result(r)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_measure_result(result: &MeasureResult) -> Result<(), String> {
    text(&result.evidence, MAX_DETAIL, true)?;
    Ok(())
}

/// Structural invariants checked on load and after every apply.
pub(crate) fn validate_board(board: &Kanban) -> Result<(), String> {
    if board.schema != 1 {
        return Err("unsupported kanban schema".into());
    }
    if board.items.len() > MAX_ITEMS {
        return Err("kanban item capacity exceeded".into());
    }
    if board.next_id == 0 {
        return Err("kanban next_id must be positive".into());
    }
    let mut ids = BTreeSet::new();
    for item in &board.items {
        if item.id == 0 || item.id >= board.next_id || !ids.insert(item.id) {
            return Err("kanban item ids must be unique and below next_id".into());
        }
        if item.rev == 0 {
            return Err("kanban item rev must be positive".into());
        }
        validate_item(item)?;
    }
    for column in [
        Column::Backlog,
        Column::Ready,
        Column::Running,
        Column::PrOpen,
        Column::InQueue,
        Column::Merged,
    ] {
        let ranks: Vec<u32> = board.lane(column).iter().map(|i| i.rank).collect();
        if ranks
            .iter()
            .enumerate()
            .any(|(index, rank)| *rank != index as u32)
        {
            return Err("kanban ranks must be dense within a column".into());
        }
    }
    for (host, capacity) in &board.capacity {
        if !valid_host(host) || capacity.slots > MAX_SLOTS {
            return Err("kanban capacity must name a valid host and bounded slots".into());
        }
    }
    if let Some(goal) = &board.goal {
        validate_goal(goal)?;
    }
    Ok(())
}

private_debug!(
    Stuck,
    Item,
    Measure,
    MeasureKind,
    MeasureResult,
    Goal,
    Kanban,
    EditFields,
    NewChild,
    Op,
    Inverse,
    LogEntry,
);
