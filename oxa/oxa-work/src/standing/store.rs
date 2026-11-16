use super::guard::{self, Constraint, ConstraintKind};
use super::types::{self, EntryState};
use super::{
    Action, Answer, AnswerRequest, Ask, Authority, Entry, Kind, Question, Refusal, Rule, Scope, Subject,
    Value,
};
use crate::{events::Facts, facts_io::Root, Result};
use oxa_proto::work::{Checkpoint, Event, Fact, Incident, IncidentKind, StandingChanged};
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path};

const FILE: &str = "standing.json";
const LOCK: &str = ".standing.lock";
const MAX_FILE: usize = 1024 * 1024;
const MAX_VIEW: usize = 256 * 1024;
const MAX_ACTIVE: usize = 128;
const HISTORY: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub revision: u64,
    pub at: u64,
    pub entries: Vec<Entry>,
    pub asks: Vec<Ask>,
    pub history_truncated: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    after: Read,
    observation: serde_json::Value,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    current: Read,
    pending: Option<Pending>,
}
impl Document {
    fn bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| "cannot encode standing data")?;
        if bytes.len() > MAX_FILE {
            return Err("standing file capacity exceeded".into());
        }
        Ok(bytes)
    }
}
impl Read {
    fn validate(&self) -> Result<()> {
        if self.entries.len() + self.asks.len() > MAX_ACTIVE + HISTORY {
            return Err("standing entry capacity exceeded".into());
        }
        let mut ids = std::collections::HashSet::new();
        let mut live = vec![];
        for entry in &self.entries {
            entry.validate()?;
            if !ids.insert(&entry.id) {
                return Err("duplicate standing entry identity".into());
            }
            if entry.live() {
                if live
                    .iter()
                    .any(|(kind, scope)| *kind == entry.kind && *scope == &entry.scope)
                {
                    return Err("conflicting live standing rules".into());
                }
                live.push((entry.kind, &entry.scope));
            }
        }
        for ask in &self.asks {
            ask.validate()?;
            if !ids.insert(&ask.id) {
                return Err("duplicate standing ask identity".into());
            }
            if ask.revision > self.revision {
                return Err("ask is ahead of standing revision".into());
            }
            if ask
                .answer
                .as_ref()
                .is_some_and(|answer| answer.revision <= ask.revision || answer.revision > self.revision)
            {
                return Err("answer is outside its standing revision".into());
            }
        }
        if live.len() + self.asks.iter().filter(|ask| ask.answer.is_none()).count() > MAX_ACTIVE {
            return Err("active standing capacity exceeded".into());
        }
        if serde_json::to_vec(self)
            .map_err(|_| "cannot encode standing view")?
            .len()
            > MAX_VIEW
        {
            return Err("standing view byte capacity exceeded".into());
        }
        Ok(())
    }
    fn bounded(&mut self) -> Result<()> {
        while self.entries.iter().filter(|entry| !entry.live()).count()
            + self.asks.iter().filter(|ask| ask.answer.is_some()).count()
            > HISTORY
        {
            self.discard_history()?;
        }
        while self.validate().is_err() {
            self.discard_history()?;
        }
        self.validate()
    }
    fn discard_history(&mut self) -> Result<()> {
        if let Some(index) = self.entries.iter().position(|entry| !entry.live()) {
            self.entries.remove(index);
        } else if let Some((index, _)) = self
            .asks
            .iter()
            .enumerate()
            .filter_map(|(index, ask)| ask.answer.as_ref().map(|answer| (index, answer.revision)))
            .min_by_key(|(_, revision)| *revision)
        {
            self.asks.remove(index);
        } else {
            return Err("active standing capacity exceeded".into());
        }
        self.history_truncated = true;
        Ok(())
    }
    fn next(&self, expected: u64, at: u64) -> Result<Self> {
        if self.revision != expected {
            return Err("standing revision changed; read it again".into());
        }
        let mut next = self.clone();
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or("standing revision exhausted")?;
        next.at = at;
        Ok(next)
    }
    fn change(&self, incident: Option<Incident>) -> StandingChanged {
        let goal = self
            .entries
            .iter()
            .find_map(|entry| match (&entry.kind, &entry.value) {
                (Kind::Goal, Value::Text(value)) if entry.live() && entry.scope == Scope::Repository => {
                    Some(value.clone())
                }
                _ => None,
            });
        StandingChanged {
            revision: self.revision,
            goal,
            ask_opened: None,
            ask_closed: None,
            incident,
        }
    }
    fn difference(&self, before: &Read, incident: Option<Incident>) -> Result<StandingChanged> {
        let mut change = self.change(incident);
        for ask in self
            .asks
            .iter()
            .filter(|ask| ask.answer.is_none() && !before.asks.iter().any(|old| old.id == ask.id))
        {
            if change.ask_opened.replace(ask.fact()).is_some() {
                return Err("standing mutation opens multiple asks".into());
            }
        }
        for ask in before.asks.iter().filter(|ask| ask.answer.is_none()) {
            if self
                .asks
                .iter()
                .any(|new| new.id == ask.id && new.answer.is_some())
            {
                if change
                    .ask_closed
                    .replace(oxa_proto::work::AskClosed { id: ask.id.clone() })
                    .is_some()
                {
                    return Err("standing mutation closes multiple asks".into());
                }
            } else if !self.asks.iter().any(|new| new.id == ask.id) {
                return Err("standing mutation drops an unanswered ask".into());
            }
        }
        Ok(change)
    }
}

#[derive(Clone, Debug)]
pub struct Standing {
    root: Root,
    facts: Facts,
}
#[derive(Debug)]
pub enum GuardError {
    Storage(String),
    Refused(Refusal),
}
impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "standing data unavailable: {error}"),
            Self::Refused(refusal) => write!(
                f,
                "standing revision {} refuses this action: {:?}",
                refusal.revision, refusal.reason
            ),
        }
    }
}
impl std::error::Error for GuardError {}
impl From<String> for GuardError {
    fn from(error: String) -> Self {
        Self::Storage(error)
    }
}
pub struct Guarded<T> {
    pub revision: u64,
    pub result: Result<T>,
}
pub struct Verification {
    pub revision: u64,
    pub scope: Scope,
    pub value: Value,
    pub at: u64,
    pub evidence: String,
}

impl Standing {
    /// Explicit owner initialization at a protected per-repository configuration root.
    pub fn initialize(root: &Path, authority: Authority, facts: Facts) -> Result<Self> {
        if authority != Authority::Owner {
            return Err("owner initialization required".into());
        }
        let root = Root::open(root, true)?;
        let _lock = root.lock(LOCK)?;
        if root.exists(FILE)? {
            return Err("standing data already exists".into());
        }
        if facts.situation()?.standing_revision != 0 {
            return Err("standing facts already exist".into());
        }
        let document = Document {
            version: 1,
            current: Read {
                revision: 0,
                at: 0,
                entries: vec![],
                asks: vec![],
                history_truncated: false,
            },
            pending: None,
        };
        root.create(FILE, &document.bytes()?)?;
        Ok(Self { root, facts })
    }
    pub fn open(root: &Path, facts: Facts) -> Result<Self> {
        let standing = Self {
            root: Root::open(root, false)?,
            facts,
        };
        standing.read()?;
        Ok(standing)
    }
    pub fn read(&self) -> Result<Read> {
        let _lock = self.root.lock(LOCK)?;
        self.load()
    }
    fn load(&self) -> Result<Read> {
        let mut document: Document = serde_json::from_slice(&self.root.read(FILE, MAX_FILE)?)
            .map_err(|_| "invalid standing document")?;
        if document.version != 1 {
            return Err("unsupported standing version".into());
        }
        document.current.validate()?;
        if let Some(pending) = &document.pending {
            pending.after.validate()?;
            if document.current.revision.checked_add(1) != Some(pending.after.revision) {
                return Err("invalid pending standing revision".into());
            }
            let event = Event::from_json(
                &serde_json::to_vec(&pending.observation).map_err(|_| "invalid standing observation")?,
            )
            .map_err(|error| error.to_string())?;
            if event.checkpoint.source != "standing"
                || event.checkpoint.revision != pending.after.revision
                || event.at_ms != pending.after.at
            {
                return Err("pending standing identity disagrees".into());
            }
            let Fact::StandingChanged(change) = &event.fact else {
                return Err("invalid standing observation kind".into());
            };
            if change
                != &pending
                    .after
                    .difference(&document.current, change.incident.clone())?
            {
                return Err("pending standing projection disagrees".into());
            }
            self.facts.append(event.checkpoint, event.at_ms, event.fact)?;
            document.current = pending.after.clone();
            document.pending = None;
            self.root.atomic(FILE, &document.bytes()?)?;
        }
        // Establish durability after a prior post-rename sync failure before authorizing work.
        self.root.durable(FILE)?;
        let situation = self.facts.situation()?;
        if situation.standing_revision != document.current.revision
            || situation.goal != document.current.change(None).goal
        {
            return Err("standing file disagrees with facts revision".into());
        }
        Ok(document.current)
    }
    fn save(&self, before: Read, mut after: Read, incident: Option<Incident>) -> Result<Read> {
        after.bounded()?;
        let fact = Fact::StandingChanged(after.difference(&before, incident)?);
        let mut prepared = false;
        self.facts.append_prepared(
            Checkpoint {
                source: "standing".into(),
                revision: after.revision,
            },
            after.at,
            fact,
            |event| {
                let observation =
                    serde_json::from_slice(&event.to_json().map_err(|error| error.to_string())?)
                        .map_err(|_| "cannot encode standing observation")?;
                let document = Document {
                    version: 1,
                    current: before,
                    pending: Some(Pending {
                        after: after.clone(),
                        observation,
                    }),
                };
                self.root.atomic(FILE, &document.bytes()?)?;
                prepared = true;
                Ok(())
            },
        )?;
        if !prepared {
            return Err("standing facts revision was already used".into());
        }
        let document = Document {
            version: 1,
            current: after.clone(),
            pending: None,
        };
        self.root.atomic(FILE, &document.bytes()?)?;
        Ok(after)
    }
    pub fn rule(&self, expected: u64, authority: Authority, at: u64, request: Rule) -> Result<Entry> {
        request.validate()?;
        let _lock = self.root.lock(LOCK)?;
        let before = self.load()?;
        let mut after = before.next(expected, at)?;
        let prior = after
            .entries
            .iter_mut()
            .find(|entry| entry.live() && entry.kind == request.kind && entry.scope == request.scope);
        let mut protection = authority;
        if let Some(prior) = prior {
            if request.supersedes.as_deref() != Some(&prior.id) {
                return Err("live standing collision requires named supersession".into());
            }
            let releasing = matches!(
                request.value,
                Value::Retired | Value::Constraint { active: false }
            );
            if matches!(request.kind, Kind::Hold | Kind::Freeze) && prior.authority == Authority::Owner {
                if releasing && authority != Authority::Owner {
                    return Err("only the owner releases this constraint".into());
                }
                protection = Authority::Owner;
            }
            prior.state = EntryState::Superseded;
        } else {
            if request.supersedes.is_some() {
                return Err("superseded standing entry is not live".into());
            }
            if matches!(
                request.value,
                Value::Retired | Value::Constraint { active: false }
            ) {
                return Err("no live standing entry to release".into());
            }
        }
        let entry = Entry {
            id: format!("R-{}", after.revision),
            kind: request.kind,
            scope: request.scope,
            value: request.value,
            said: request.said,
            at,
            source: request.source,
            supersedes: request.supersedes,
            state: EntryState::Stored,
            reminders: 0,
            authority: protection,
        };
        after.entries.push(entry.clone());
        self.save(before, after, None)?;
        Ok(entry)
    }
    pub fn remind(&self, expected: u64, entry_id: &str, at: u64) -> Result<Entry> {
        let _lock = self.root.lock(LOCK)?;
        let before = self.load()?;
        let mut after = before.next(expected, at)?;
        let entry = after
            .entries
            .iter_mut()
            .find(|entry| entry.id == entry_id && entry.live())
            .ok_or("standing entry is not live")?;
        entry.reminders = entry
            .reminders
            .checked_add(1)
            .ok_or("standing reminder count exhausted")?;
        let result = entry.clone();
        let subject = match &entry.scope {
            Scope::PullRequest(pr) => oxa_proto::work::Subject::PullRequest(*pr),
            Scope::Task(task) => oxa_proto::work::Subject::Task(task.clone()),
            Scope::Host(host) => oxa_proto::work::Subject::Host(host.clone()),
            Scope::Repository | Scope::Branch(_) => oxa_proto::work::Subject::Repository,
        };
        let incident = Incident {
            id: format!("{}-reminded-{}", entry.id, after.revision),
            subject,
            kind: IncidentKind::RuleReminder,
            evidence: None,
        };
        self.save(before, after, Some(incident))?;
        Ok(result)
    }
    /// A trusted frontend checker supplies actual runtime values/evidence; tool JSON cannot.
    pub fn verify(
        &self,
        expected: u64,
        entry_id: &str,
        check: impl FnOnce(&Entry) -> Result<Verification>,
    ) -> Result<Entry> {
        let _lock = self.root.lock(LOCK)?;
        let before = self.load()?;
        if before.revision != expected {
            return Err("standing revision changed; read it again".into());
        }
        let mut after = before.clone();
        let entry = after
            .entries
            .iter_mut()
            .find(|entry| entry.id == entry_id && entry.live())
            .ok_or("standing entry is not live")?;
        let verified = check(entry)?;
        types::text(&verified.evidence, 256)?;
        if verified.revision != expected || verified.scope != entry.scope || verified.value != entry.value {
            return Err("runtime evidence does not match the standing rule".into());
        }
        if matches!(entry.state, EntryState::Applied { .. }) {
            return Ok(entry.clone());
        }
        entry.state = EntryState::Applied {
            verified_at: verified.at,
            evidence: verified.evidence,
        };
        let result = entry.clone();
        after.revision = expected.checked_add(1).ok_or("standing revision exhausted")?;
        after.at = verified.at;
        self.save(before, after, None)?;
        Ok(result)
    }
    pub fn ask(&self, expected: u64, at: u64, request: Question) -> Result<Ask> {
        request.validate()?;
        let _lock = self.root.lock(LOCK)?;
        let before = self.load()?;
        let mut after = before.next(expected, at)?;
        let ask = Ask {
            id: format!("A-{}", after.revision),
            revision: after.revision,
            scope: request.scope,
            question: request.question,
            options: request.options,
            recommended: request.recommended,
            meanwhile: request.meanwhile,
            at,
            source: request.source,
            answer: None,
        };
        after.asks.push(ask.clone());
        self.save(before, after, None)?;
        Ok(ask)
    }
    /// An answer records an owner decision; it never releases a hold or grants tool permission.
    pub fn answer(
        &self,
        expected: u64,
        authority: Authority,
        at: u64,
        request: AnswerRequest,
    ) -> Result<Ask> {
        if authority != Authority::Owner {
            return Err("only the owner answers standing asks".into());
        }
        request.source.validate()?;
        let _lock = self.root.lock(LOCK)?;
        let before = self.load()?;
        let mut after = before.next(expected, at)?;
        let ask = after
            .asks
            .iter_mut()
            .find(|ask| ask.id == request.ask_id)
            .ok_or("standing ask was not found")?;
        if ask.revision != request.question_revision || ask.answer.is_some() {
            return Err("standing question changed or was already answered".into());
        }
        ask.validate_choice(&request.choice)?;
        ask.answer = Some(Answer {
            revision: after.revision,
            question_revision: request.question_revision,
            choice: request.choice,
            at,
            source: request.source,
        });
        let result = ask.clone();
        self.save(before, after, None)?;
        Ok(result)
    }
    pub fn is_held(&self, pr: u64) -> Result<Option<String>> {
        if pr == 0 {
            return Err("PR number must be positive".into());
        }
        let read = self.read()?;
        Ok(read
            .entries
            .iter()
            .find(|entry| {
                entry.live()
                    && entry.kind == Kind::Hold
                    && (entry.scope == Scope::Repository || entry.scope == Scope::PullRequest(pr))
            })
            .map(|entry| entry.id.clone()))
    }
    pub fn with_guard<T>(
        &self,
        action: Action,
        subject: Subject<'_>,
        effect: impl FnOnce(&Read) -> Result<T>,
    ) -> std::result::Result<Guarded<T>, GuardError> {
        let _lock = self.root.lock(LOCK)?;
        let read = self.load()?;
        let constraints = read.entries.iter().filter_map(|entry| {
            if !entry.live() {
                return None;
            }
            let kind = match entry.kind {
                Kind::Hold => ConstraintKind::Hold,
                Kind::Freeze => ConstraintKind::Freeze,
                _ => return None,
            };
            Some(Constraint {
                rule_id: &entry.id,
                scope: &entry.scope,
                kind,
            })
        });
        guard::check(read.revision, constraints, action, subject).map_err(GuardError::Refused)?;
        Ok(Guarded {
            revision: read.revision,
            result: effect(&read),
        })
    }
}

#[cfg(test)]
mod tests;
