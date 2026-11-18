//! Assignment policy and mirror plans. No independent facts store or external writes.
use super::{Action, Entry};
use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    Oxa,
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Builder,
    Reviewer,
    Orchestrator,
    Owner,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Worker {
    pub id: String,
    pub kind: WorkerKind,
    pub skills: BTreeSet<String>,
    pub roles: BTreeSet<Role>,
}

pub use super::valid_worker_id;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Pin {
    Worker(String),
    Kind(WorkerKind),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    pub kinds: BTreeSet<WorkerKind>,
    pub skills: BTreeSet<String>,
    /// Any one role suffices; action-specific role checks still apply.
    pub any_roles: BTreeSet<Role>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentPolicy {
    pub pin: Option<Pin>,
    pub owner_only: bool,
    pub required: Requirements,
}

/// Selected by authenticated user/lead frontend code, never deserialized from tool input.
#[derive(Clone, Copy)]
pub enum ManualAuthority {
    Owner,
    Lead,
}

pub enum Control {
    Pin(Pin),
    Unpin,
    OwnerOnly(bool),
    Require(Requirements),
}

impl AssignmentPolicy {
    /// Release pins remain first; a matching manual pin precedes automatic picks.
    pub fn rank(&self, worker: &Worker, entry: &Entry) -> Option<(bool, bool, Action, u64)> {
        (entry.eligible && self.allows(worker, entry.action)).then_some((
            !entry.release_pinned,
            self.pin.is_none(),
            entry.action,
            entry.number,
        ))
    }

    pub fn control(&mut self, authority: Option<ManualAuthority>, change: Control) -> Result<()> {
        if authority.is_none() {
            return Err("owner or lead control required".into());
        }
        match change {
            Control::Pin(Pin::Worker(ref id)) if !valid_worker_id(id) => {
                return Err("invalid worker pin".into())
            }
            Control::Pin(pin) => self.pin = Some(pin),
            Control::Unpin => self.pin = None,
            Control::OwnerOnly(value) => self.owner_only = value,
            Control::Require(required) => self.required = required,
        }
        Ok(())
    }

    /// A manual pin narrows eligibility; it cannot bypass skills or action authority.
    pub fn allows(&self, worker: &Worker, action: Action) -> bool {
        let base_role = match action {
            Action::FixFindings
            | Action::FixRequiredCi
            | Action::ResolveConflict
            | Action::AddRemoves
            | Action::BuildIssue => Role::Builder,
            Action::PromoteDraft | Action::Ready => Role::Orchestrator,
            _ => return false,
        };
        let owner = worker.roles.contains(&Role::Owner);
        valid_worker_id(&worker.id)
            && !(worker.roles.contains(&Role::Builder) && worker.roles.contains(&Role::Reviewer))
            && (!self.owner_only || owner)
            && (owner || worker.roles.contains(&base_role))
            && (self.required.kinds.is_empty() || self.required.kinds.contains(&worker.kind))
            && self.required.skills.is_subset(&worker.skills)
            && (self.required.any_roles.is_empty() || !self.required.any_roles.is_disjoint(&worker.roles))
            && match &self.pin {
                None => true,
                Some(Pin::Worker(id)) => id == &worker.id,
                Some(Pin::Kind(kind)) => kind == &worker.kind,
            }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MirrorObservation {
    pub labels: BTreeSet<String>,
    pub has_project: bool,
}

/// Display-only deltas. No apply/write interface is enabled while write policy is unsettled.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "op", content = "worker", rename_all = "snake_case")]
pub enum ProjectWorker {
    Unchanged,
    Clear,
    Set(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MirrorPlan {
    pub add_labels: BTreeSet<String>,
    pub remove_labels: BTreeSet<String>,
    pub project_worker: ProjectWorker,
}

impl MirrorPlan {
    pub fn derive(assignee: Option<&str>, observed: &MirrorObservation) -> Result<Self> {
        if assignee.is_some_and(|id| !valid_worker_id(id)) {
            return Err("invalid mirror assignee".into());
        }
        let desired: BTreeSet<String> = assignee
            .map(|id| format!("worker:{}", id.rsplit([':', '/']).next().unwrap_or(id)))
            .into_iter()
            .collect();
        let current: BTreeSet<String> = observed
            .labels
            .iter()
            .filter(|label| label.starts_with("worker:"))
            .cloned()
            .collect();
        Ok(Self {
            add_labels: desired.difference(&current).cloned().collect(),
            remove_labels: current.difference(&desired).cloned().collect(),
            project_worker: if !observed.has_project {
                ProjectWorker::Unchanged
            } else {
                assignee.map_or(ProjectWorker::Clear, |id| ProjectWorker::Set(id.into()))
            },
        })
    }
}
