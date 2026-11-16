//! A4: the transcript fold (W1).
use oxa_proto::{rebase_why, Entry, EntryBody, FrozenPrefix, TurnKind, Usage, WireMsg};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// One recorded rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseLog {
    /// One of `oxa_proto::rebase_why`.
    pub why: &'static str,
    /// Wire length (messages) before and after.
    pub before: usize,
    pub after: usize,
    /// Chain hash after the rebase.
    pub chain: [u8; 32],
}

/// The wire of the active branch plus the bookkeeping the cache check needs.
///
/// Invariants (property-tested by A4):
/// * `push` and every non-rebasing entry in `project`/`apply` only append: the previous wire is
///   a prefix of the new one.
/// * `chain()` is `sha256(prev_chain || canonical bytes of the message)` folded over the wire
///   from an all-zero start; after a rebase it is recomputed from the new head.
/// * `expected_cache()` is the number of prompt tokens the engine should report as cached on
///   the next request: `prompt + completion - 1` of the last usage set (F1: the last generated
///   token is never fed; `prompt` for an H6 prefill), or 0 after a rebase until the next
///   `set_usage`. `context()` stays `prompt + completion`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transcript {
    prefix: Option<FrozenPrefix>,
    wire: Arc<Vec<WireMsg>>,
    /// Entry id that produced each wire message (`None` for a synthetic head).
    origins: Vec<Option<String>>,
    chain: [u8; 32],
    usage: Option<Usage>,
    /// L5: the wire length when `usage` was set (the wire then ends with the reply it counts).
    /// Messages after it were appended since and are not in `usage`. In memory only.
    usage_len: usize,
    expected: u32,
    /// B6: `prompt` of the last request that answered a user message (the first request of
    /// the user turn in progress); 0 after a rebase.
    turn_base: u32,
    rebases: Vec<RebaseLog>,
}

impl Transcript {
    /// Fold a branch (root first) into a transcript. See `EntryBody` for the per-body rule:
    /// `Msg` appends its `wire`; in-context `BashExecution` (not `excluded`) and `CustomMessage`
    /// append one user message; the last `Prefix` sets the prefix (a later one is an `epoch`
    /// rebase that keeps the messages); `Compaction` rebases to `[user:<summary>]` + the
    /// messages from `first_kept` on; `BranchSummary` appends one user message at its position;
    /// `ContextEdit` rebases by replacing or removing its target message; all other bodies
    /// leave the wire untouched. Summary and bash message texts are produced by the callers of
    /// `rebase`/`push` at run time and by the same pure functions here on replay, so a resumed
    /// session reproduces the same bytes.
    pub fn project(branch: &[Entry]) -> Transcript {
        let mut t = Transcript::default();
        for e in branch {
            t.apply(e);
        }
        t
    }

    /// Fold one more entry (the incremental form of [`Transcript::project`]).
    pub fn apply(&mut self, e: &Entry) {
        let origin = Some(e.id.clone());
        match &e.body {
            EntryBody::Prefix(p) => {
                let later_epoch = self.prefix.is_some();
                let same_prefix = self.prefix.as_ref() == Some(p);
                self.prefix = Some(p.clone());
                if later_epoch {
                    let (wire, origins) = (self.wire.to_vec(), self.origins.clone());
                    self.install_head(wire, origins, rebase_why::EPOCH, same_prefix);
                }
            }
            EntryBody::Msg { wire, .. } => {
                self.push_from(wire.clone(), origin);
            }
            EntryBody::BashExecution {
                command,
                output,
                exit,
                excluded,
            } => {
                if !*excluded {
                    let text = bash_text(command, output, *exit);
                    self.push_from(WireMsg::user(text), origin);
                }
            }
            // Output contracts are process-local. Older versions stored them as
            // messages; keep their audit entries, but never restore their instructions.
            EntryBody::CustomMessage { custom_type, .. }
                if matches!(custom_type.as_str(), "oxa.output_schema" | "oxa.schema_repair") => {}
            EntryBody::CustomMessage { content, .. } => {
                self.push_from(WireMsg::user(content.clone()), origin);
            }
            EntryBody::BranchSummary { summary, .. } => {
                self.push_from(WireMsg::user(summary.clone()), origin);
            }
            EntryBody::Compaction {
                summary,
                first_kept,
                details,
                ..
            } => {
                // Head = [user:summary] + every message from `first_kept` on. An unknown
                // `first_kept` keeps no tail (only the summary remains). A tail without entry
                // origins (messages rewritten by a declared extension rebase) is stored in
                // `details.keptMessages` and replayed from there. `details.elided` (A1) replaces
                // the content of oversized kept tool results: `[{"at": <index in the tail>,
                // "text": ..}]`.
                let cut = self
                    .origins
                    .iter()
                    .position(|o| o.as_deref() == Some(first_kept.as_str()))
                    .unwrap_or(self.wire.len());
                let (mut tail, tail_origins) = match Self::decode_kept(details) {
                    Some(kept) if first_kept.is_empty() => {
                        let n = kept.len();
                        (kept, vec![None; n])
                    }
                    _ => (self.wire[cut..].to_vec(), self.origins[cut..].to_vec()),
                };
                for (at, text) in Self::decode_elided(details) {
                    if let Some(m) = tail.get_mut(at) {
                        m.content = Some(text);
                    }
                }
                let mut wire = vec![WireMsg::user(summary.clone())];
                let mut origins = vec![origin];
                wire.extend(tail);
                origins.extend(tail_origins);
                self.install(wire, origins, rebase_why::COMPACTION);
            }
            EntryBody::ContextEdit { target, replacement } => {
                // A target that is not on the wire changes nothing and logs nothing.
                if let Some(i) = self
                    .origins
                    .iter()
                    .position(|o| o.as_deref() == Some(target.as_str()))
                {
                    let mut wire = self.wire.to_vec();
                    let mut origins = self.origins.clone();
                    match replacement {
                        Some(text) => wire[i].content = Some(text.clone()),
                        None => {
                            wire.remove(i);
                            origins.remove(i);
                        }
                    }
                    self.install(wire, origins, rebase_why::CONTEXT_EDIT);
                }
            }
            EntryBody::Usage { kind, usage } => {
                // Only requests over this wire (chat, prefill) set the expected cache; side
                // requests (compaction, branch summary, subagent) do not.
                if kind == TurnKind::Chat.as_str() || kind == TurnKind::Prefill.as_str() {
                    self.set_usage(usage);
                }
            }
            EntryBody::Custom { custom_type, data } if custom_type == Self::EXTENSION_REBASE_ENTRY => {
                // A declared extension rebase (E-INT 3): the stored message list is the wire
                // that was sent. A marker without `messages` (sessions written before the list
                // was stored) or with an unreadable list changes nothing.
                let Some(head) = Self::decode_rebase(data) else {
                    return;
                };
                // A message keeps its entry origin where it is byte-identical to the message it
                // replaces at the same position; any other position has no origin (`None`).
                let origins = head
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        (self.wire.get(i) == Some(m))
                            .then(|| self.origins[i].clone())
                            .flatten()
                    })
                    .collect();
                self.install(head, origins, rebase_why::EXTENSION);
            }
            // Never on the wire.
            EntryBody::Shown { .. }
            | EntryBody::ModelChange(_)
            | EntryBody::ThinkingLevelChange(_)
            | EntryBody::ModeChange(_)
            | EntryBody::Custom { .. }
            | EntryBody::Label { .. }
            | EntryBody::SessionInfo { .. } => {}
        }
    }
    /// `customType` of the `custom` entry that records a declared extension rebase. Its `data`
    /// carries the rebased wire under `messages` ([`Transcript::encode_rebase`]); the fold
    /// applies it as a rebase with `rebase_why::EXTENSION`.
    pub const EXTENSION_REBASE_ENTRY: &'static str = "extension_rebase";

    /// The `messages` value of an [`Transcript::EXTENSION_REBASE_ENTRY`] entry: each message in
    /// the encoding of a `Msg` entry's `wire` object (content, reasoning, tool calls with ids
    /// and `arguments` verbatim; W2).
    pub fn encode_rebase(wire: &[WireMsg]) -> serde_json::Value {
        serde_json::Value::Array(wire.iter().map(crate::store::wire_value).collect())
    }

    /// The message list of an extension-rebase entry's `data`; `None` without a readable list.
    fn decode_rebase(data: &serde_json::Value) -> Option<Vec<WireMsg>> {
        data.get("messages")?
            .as_array()?
            .iter()
            .map(|m| crate::store::decode_wire_value(m).ok())
            .collect()
    }

    /// `details.keptMessages` of a compaction whose kept tail has no entry origins (same
    /// encoding as [`Transcript::encode_rebase`]).
    fn decode_kept(details: &serde_json::Value) -> Option<Vec<WireMsg>> {
        Self::decode_rebase(&serde_json::json!({ "messages": details.get("keptMessages")? }))
    }

    /// `details.elided` of a compaction: `(index in the kept tail, replacement text)`.
    fn decode_elided(details: &serde_json::Value) -> Vec<(usize, String)> {
        details
            .get("elided")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let at = usize::try_from(e.get("at")?.as_u64()?).ok()?;
                        Some((at, e.get("text")?.as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn prefix(&self) -> Option<&FrozenPrefix> {
        self.prefix.as_ref()
    }
    pub fn wire(&self) -> &[WireMsg] {
        &self.wire
    }
    /// The wire as a shared handle, so a request can hold it while the session state is used
    /// elsewhere, without copying the messages. A later `push` or rebase leaves a held handle
    /// unchanged (copy on write).
    pub fn wire_shared(&self) -> Arc<Vec<WireMsg>> {
        Arc::clone(&self.wire)
    }
    /// Append a message; returns its index. Never changes existing messages.
    pub fn push(&mut self, m: WireMsg) -> usize {
        self.push_from(m, None)
    }
    /// Replace the wire by `head`. `why` must be one of `oxa_proto::rebase_why`; anything else
    /// panics (a bug). Logs a [`RebaseLog`] and zeroes `expected_cache`.
    pub fn rebase(&mut self, head: Vec<WireMsg>, why: &'static str) {
        let origins = vec![None; head.len()];
        self.install(head, origins, why);
    }
    /// Refuse a compaction cut inside a tool exchange or retaining an unfinished batch.
    pub fn validate_compaction_cut(&self, cut: usize) -> Result<(), oxa_proto::OxaError> {
        let cut = cut.min(self.wire.len());
        closed_tools(&self.wire[..cut])?;
        closed_tools(&self.wire[cut..])
    }

    pub(crate) fn validate_compaction(&self, body: &EntryBody) -> Result<(), oxa_proto::OxaError> {
        if let EntryBody::Compaction {
            first_kept, details, ..
        } = body
        {
            let cut = self
                .origins
                .iter()
                .position(|o| o.as_deref() == Some(first_kept))
                .unwrap_or(self.wire.len());
            self.validate_compaction_cut(cut)?;
            if first_kept.is_empty() {
                if let Some(kept) = Self::decode_kept(details) {
                    closed_tools(&kept)?;
                }
            }
        }
        Ok(())
    }

    /// Retain only existing authority whose complete visible prefix is unchanged.
    /// Never accept a replacement carrier supplied by a rebase or hook.
    pub fn prepare_rebase(&self, head: &mut [WireMsg]) {
        let mut unchanged = true;
        for (i, message) in head.iter_mut().enumerate() {
            let old = self.wire.get(i);
            unchanged &= old.is_some_and(|old| {
                old.role == message.role
                    && old.content == message.content
                    && old.reasoning == message.reasoning
                    && old.tool_calls == message.tool_calls
                    && old.tool_call_id == message.tool_call_id
            });
            message.provider_history = old
                .filter(|_| unchanged)
                .and_then(|old| old.provider_history.clone());
        }
    }

    pub fn chain(&self) -> [u8; 32] {
        self.chain
    }
    /// Record the usage of the request that just completed over the current wire (called after
    /// the reply was pushed).
    ///
    /// F1: the engine never feeds the last generated token (`tokens_cached = prompt +
    /// completion - 1`), so a perfect continuation reports `prompt + completion - 1` cached
    /// tokens; an H6 prefill (`completion == 0`) leaves exactly `prompt`.
    pub fn set_usage(&mut self, u: &Usage) {
        self.usage = Some(*u);
        self.usage_len = self.wire.len();
        self.expected = u.prompt.saturating_add(u.completion.saturating_sub(1));
        let n = self.wire.len();
        if u.completion > 0 && n >= 2 && self.wire[n - 2].role == oxa_proto::Role::User {
            self.turn_base = u.prompt;
        }
    }
    /// B6: `prompt` of the request that answered the last user message (the first assistant
    /// turn after it starts right after those tokens); 0 when unknown (after a rebase). A
    /// template that drops the reasoning of earlier turns at the next user message re-renders
    /// the history from there.
    pub fn turn_base(&self) -> u32 {
        self.turn_base
    }
    /// Context size in tokens after the last reply: `prompt + completion` of the last usage.
    /// A rebase drops the usage (the size of the new head is unknown until the next reply), so
    /// this is 0 after a rebase until the next `set_usage`.
    pub fn context(&self) -> u32 {
        self.usage.map_or(0, |u| u.prompt.saturating_add(u.completion))
    }
    /// L5: the wire length at the last usage (`None` when there is no usage). The messages from
    /// this index on were appended after the counted reply.
    pub fn usage_wire_len(&self) -> Option<usize> {
        self.usage.map(|_| self.usage_len)
    }
    pub fn expected_cache(&self) -> u32 {
        self.expected
    }
    pub fn rebases(&self) -> &[RebaseLog] {
        &self.rebases
    }
    /// Entry id that produced wire message `index`.
    pub fn origin(&self, index: usize) -> Option<&str> {
        self.origins.get(index).and_then(|o| o.as_deref())
    }

    /// Append one message produced by entry `origin`; extends the chain.
    fn push_from(&mut self, m: WireMsg, origin: Option<String>) -> usize {
        let idx = self.wire.len();
        self.chain = chain_step(&self.chain, &m);
        Arc::make_mut(&mut self.wire).push(m);
        self.origins.push(origin);
        idx
    }

    /// Replace the wire (and its origins) by a head, recompute the chain, drop the usage and
    /// log the rebase. The only place the wire may shrink or change.
    fn install(&mut self, wire: Vec<WireMsg>, origins: Vec<Option<String>>, why: &'static str) {
        self.install_head(wire, origins, why, true);
    }
    fn install_head(
        &mut self,
        mut wire: Vec<WireMsg>,
        origins: Vec<Option<String>>,
        why: &'static str,
        same_prefix: bool,
    ) {
        if same_prefix {
            self.prepare_rebase(&mut wire);
        } else {
            for message in &mut wire {
                message.provider_history = None;
            }
        }
        assert!(rebase_why::ALL.contains(&why), "unknown rebase cause: {why:?}");
        let before = self.wire.len();
        self.wire = Arc::new(wire);
        self.origins = origins;
        self.chain = chain_of(&self.wire);
        self.usage = None;
        self.usage_len = 0;
        self.expected = 0;
        self.turn_base = 0;
        self.rebases.push(RebaseLog {
            why,
            before,
            after: self.wire.len(),
            chain: self.chain,
        });
    }
}

/// One step of the chain: `sha256(prev || canonical bytes of m)`.
fn chain_step(prev: &[u8; 32], m: &WireMsg) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(m.to_json());
    h.finalize().into()
}

/// The chain of a whole wire, folded from an all-zero start.
fn chain_of(wire: &[WireMsg]) -> [u8; 32] {
    wire.iter().fold([0u8; 32], |prev, m| chain_step(&prev, m))
}

/// The user message text of an included `!cmd`, in pi's format (`bashExecutionToText`).
/// Public so the live `!cmd` path (oxa-core) pushes the same bytes the fold replays (W1).
pub fn bash_text(command: &str, output: &str, exit: Option<i32>) -> String {
    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(output);
        text.push_str("\n```");
    }
    if let Some(code) = exit.filter(|c| *c != 0) {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    text
}

/// Check both sides independently: no orphan/duplicate result, pending call or split batch.
fn closed_tools(wire: &[WireMsg]) -> Result<(), oxa_proto::OxaError> {
    use oxa_proto::Role;
    let mut pending = std::collections::BTreeSet::new();
    let invalid = || {
        oxa_proto::OxaError::new(
            oxa_proto::codes::INVALID,
            "Compaction requires closed tool-call/result boundaries.",
        )
    };
    for message in wire {
        if message.role == Role::Tool {
            if !message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| pending.remove(id))
            {
                return Err(invalid());
            }
        } else {
            if !pending.is_empty() {
                return Err(invalid());
            }
            for call in &message.tool_calls {
                if call.id.is_empty() || !pending.insert(call.id.as_str()) {
                    return Err(invalid());
                }
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(invalid())
    }
}

/// Only compactions need ancestry projection; ordinary appends do no extra work.
pub(crate) fn validate_append(entries: &[Entry], entry: &Entry) -> Result<(), oxa_proto::OxaError> {
    if !matches!(entry.body, EntryBody::Compaction { .. }) {
        return Ok(());
    }
    let indices: std::collections::HashMap<_, _> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut branch = Vec::new();
    let mut parent = entry.parent.as_deref();
    while let Some(e) = parent.and_then(|id| indices.get(id)).copied() {
        if branch.len() >= entries.len() {
            return Err(oxa_proto::OxaError::new(
                oxa_proto::codes::INVALID,
                "Cyclic compaction ancestry.",
            ));
        }
        branch.push(e);
        parent = e.parent.as_deref();
    }
    let mut transcript = Transcript::default();
    for e in branch.into_iter().rev() {
        transcript.apply(e);
    }
    transcript.validate_compaction(&entry.body)
}
