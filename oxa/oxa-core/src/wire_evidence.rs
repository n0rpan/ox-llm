//! Owner-terminal-only events. Not an AgentEvent: those reach hooks and other frontends.
use oxa_proto::{AssistantTurn, ChatRequest, OxaError, Role, TurnKind, WireMsg};
use serde::Serialize;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io;
use std::sync::{Arc, Mutex};

const MAX_ROWS: usize = 64;
const MAX_MESSAGES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct WireEvidence(Arc<Mutex<State>>);

impl std::fmt::Debug for WireEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WireEvidence")
    }
}

#[derive(Default)]
struct State {
    report: Report,
    capture: Option<Capture>,
}

struct Capture {
    key: RandomState,
    previous: Option<Snapshot>,
    request: usize,
    attempt: usize,
    new_request: bool,
    gap: bool,
    rebases: Option<usize>,
}

struct Snapshot {
    messages: Vec<u64>,
    system: u64,
    tools: u64,
}

#[derive(Default, Serialize)]
pub struct Report {
    rows: Vec<Row>,
    row_limit_reached: bool,
}

#[derive(Serialize)]
struct Row {
    request: usize,
    attempt: usize,
    status: Status,
    gap: bool,
    rebase: bool,
    tool_continuation: bool,
    over_allowance: bool,
    messages: usize,
    adjacent: Option<Adjacent>,
    input: Option<u32>,
    output: Option<u32>,
    cached: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Returned,
    Cancelled,
    Failed,
}

#[derive(Serialize)]
struct Adjacent {
    previous_messages: usize,
    common_leading_messages: usize,
    previous_messages_preserved: bool,
    system_same: bool,
    tools_same: bool,
}

pub(crate) struct Pending {
    snapshot: Option<Snapshot>,
    row: Row,
}

impl WireEvidence {
    pub(crate) fn start() -> Self {
        let evidence = Self::default();
        evidence.0.lock().unwrap().capture = Some(Capture {
            key: RandomState::new(),
            previous: None,
            request: 0,
            attempt: 0,
            new_request: true,
            gap: false,
            rebases: None,
        });
        evidence
    }

    pub(crate) fn stop(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.capture = None;
        }
    }

    /// Drain allowlisted events for the local owner only. Drops the key and fingerprints.
    pub fn finish(self) -> Report {
        self.0
            .lock()
            .map(|mut state| {
                state.capture = None;
                std::mem::take(&mut state.report)
            })
            .unwrap_or_default()
    }

    pub(crate) fn new_request(&self) {
        if let Ok(mut state) = self.0.lock() {
            if let Some(capture) = &mut state.capture {
                capture.new_request = true;
            }
        }
    }

    pub(crate) fn gap(&self) {
        if let Ok(mut state) = self.0.lock() {
            if let Some(capture) = &mut state.capture {
                capture.gap = true;
                capture.previous = None;
            }
        }
    }

    pub(crate) fn before(&self, req: &ChatRequest<'_>, rebases: usize) -> Option<Pending> {
        if req.hints.kind != TurnKind::Chat || req.hints.background || req.max_tokens == 0 {
            self.gap();
            return None;
        }
        let mut state = self.0.lock().ok()?;
        if state.report.rows.len() == MAX_ROWS {
            state.report.row_limit_reached = true;
            state.capture = None;
        }
        let capture = state.capture.as_mut()?;
        if std::mem::take(&mut capture.new_request) {
            capture.request += 1;
            capture.attempt = 0;
        }
        capture.attempt += 1;
        let rebase = capture
            .rebases
            .replace(rebases)
            .is_some_and(|prior| prior != rebases);
        let snapshot = Snapshot::take(req, &capture.key).ok();
        // Retries are attempts of the same logical request. Never compare one to itself, or
        // silently pair across a failed attempt, excluded request or incomplete snapshot.
        let gap = std::mem::take(&mut capture.gap) || capture.attempt > 1;
        let adjacent = (!gap)
            .then(|| Some(snapshot.as_ref()?.against(capture.previous.as_ref()?)))
            .flatten();
        capture.previous = None;
        Some(Pending {
            row: Row {
                request: capture.request,
                attempt: capture.attempt,
                status: Status::Failed,
                gap,
                rebase,
                tool_continuation: req.messages.last().is_some_and(|m| m.role == Role::Tool),
                over_allowance: snapshot.is_none(),
                messages: req.messages.len(),
                adjacent,
                input: None,
                output: None,
                cached: None,
            },
            snapshot,
        })
    }

    pub(crate) fn after(&self, mut pending: Pending, result: &Result<AssistantTurn, OxaError>) {
        let Ok(mut state) = self.0.lock() else { return };
        let Some(capture) = &mut state.capture else { return };
        let row = &mut pending.row;
        match result {
            Ok(turn) => {
                row.status = Status::Returned;
                if let Some(usage) = turn.usage.reported() {
                    row.input = Some(usage.prompt);
                    row.output = Some(usage.completion);
                    row.cached = (!usage.cached_missing).then_some(usage.cached);
                }
                capture.previous = pending.snapshot;
            }
            Err(e) => {
                row.adjacent = None;
                if e.code == oxa_proto::codes::ABORTED {
                    row.status = Status::Cancelled;
                }
            }
        }
        capture.gap |= capture.previous.is_none();
        state.report.rows.push(pending.row);
    }
}

impl Snapshot {
    fn take(req: &ChatRequest<'_>, key: &RandomState) -> io::Result<Self> {
        if req.messages.len() > MAX_MESSAGES {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let mut remaining = MAX_BYTES;
        let mut hash = Fingerprint {
            hash: key.build_hasher(),
            remaining: &mut remaining,
        };
        for (i, (_, text)) in req.prefix.sections.iter().enumerate() {
            if i > 0 {
                io::Write::write_all(&mut hash, b"\n\n")?;
            }
            io::Write::write_all(&mut hash, text.as_bytes())?;
        }
        let system = hash.hash.finish();
        let mut hash = Fingerprint {
            hash: key.build_hasher(),
            remaining: &mut remaining,
        };
        io::Write::write_all(&mut hash, req.prefix.tools_json.as_bytes())?;
        let tools = hash.hash.finish();
        let messages = req
            .messages
            .iter()
            .map(|msg| {
                let mut hash = Fingerprint {
                    hash: key.build_hasher(),
                    remaining: &mut remaining,
                };
                hash.message(msg)?;
                Ok(hash.hash.finish())
            })
            .collect::<io::Result<_>>()?;
        Ok(Self {
            messages,
            system,
            tools,
        })
    }

    fn against(&self, previous: &Self) -> Adjacent {
        let common = previous
            .messages
            .iter()
            .zip(&self.messages)
            .take_while(|(a, b)| a == b)
            .count();
        Adjacent {
            previous_messages: previous.messages.len(),
            common_leading_messages: common,
            previous_messages_preserved: common == previous.messages.len(),
            system_same: self.system == previous.system,
            tools_same: self.tools == previous.tools,
        }
    }
}

/// Hash borrowed fields without making a second text/image/replay buffer. Lengths and option
/// tags distinguish field boundaries. Size failure abandons the entire snapshot, not the chat.
struct Fingerprint<'a> {
    hash: <RandomState as BuildHasher>::Hasher,
    remaining: &'a mut usize,
}

impl Fingerprint<'_> {
    fn text(&mut self, text: &str) -> io::Result<()> {
        text.len().hash(&mut self.hash);
        io::Write::write_all(self, text.as_bytes())
    }

    fn optional(&mut self, text: Option<&str>) -> io::Result<()> {
        text.is_some().hash(&mut self.hash);
        if let Some(text) = text {
            self.text(text)?;
        }
        Ok(())
    }

    fn message(&mut self, msg: &WireMsg) -> io::Result<()> {
        msg.role.hash(&mut self.hash);
        self.optional(msg.content.as_deref())?;
        self.optional(msg.reasoning.as_deref())?;
        self.optional(msg.tool_call_id.as_deref())?;
        msg.images.len().hash(&mut self.hash);
        for image in &msg.images {
            self.text(&image.mime)?;
            self.text(&image.base64)?;
        }
        msg.tool_calls.len().hash(&mut self.hash);
        for call in &msg.tool_calls {
            self.text(&call.id)?;
            self.text(&call.name)?;
            self.text(&call.args_raw)?;
        }
        msg.provider_history.is_some().hash(&mut self.hash);
        if let Some(history) = &msg.provider_history {
            io::Write::write_all(self, history.scope())?;
            io::Write::write_all(self, history.prefix())?;
            serde_json::to_writer(self, history.items())?;
        }
        Ok(())
    }
}

impl io::Write for Fingerprint<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Charge at least one byte per field even when its content is empty.
        *self.remaining = self
            .remaining
            .checked_sub(bytes.len().max(1))
            .ok_or(io::ErrorKind::FileTooLarge)?;
        self.hash.write(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::{Finish, FrozenPrefix, ThinkingLevel, Usage};

    #[test]
    fn incomplete_pairs_reanchor_and_limits_do_not_export_partial_matches() {
        let capture = WireEvidence::start();
        let prefix = FrozenPrefix {
            sections: vec![("preamble".into(), "synthetic-system".into())],
            tools_json: "[]".into(),
            sha256: [0; 32],
        };
        let changed = FrozenPrefix {
            tools_json: "[{}]".into(),
            ..prefix.clone()
        };
        let original = [WireMsg::user("synthetic-one")];
        let changed_messages = [WireMsg::user("synthetic-two")];
        let too_large = [WireMsg::user("x".repeat(MAX_BYTES))];
        let record = |prefix: &FrozenPrefix, messages: &[WireMsg], new, failed, rebases| {
            if new {
                capture.new_request();
            }
            let req = ChatRequest::new(prefix, messages, "synthetic-model");
            if let Some(pending) = capture.before(&req, rebases) {
                let result = if failed {
                    Err(OxaError::new(
                        oxa_proto::codes::ABORTED,
                        "synthetic-private-error",
                    ))
                } else {
                    Ok(AssistantTurn {
                        msg: WireMsg::assistant("synthetic-reply"),
                        finish: Finish::Stop,
                        usage: Usage {
                            prompt: 10,
                            cached_missing: true,
                            ..Usage::default()
                        },
                        timings: None,
                        model: "synthetic-model".into(),
                        level: ThinkingLevel::Off,
                    })
                };
                capture.after(pending, &result);
            }
        };
        record(&prefix, &original, true, false, 0);
        record(&changed, &changed_messages, true, false, 1);
        record(&changed, &original, true, true, 1);
        record(&changed, &original, false, false, 1);
        record(&changed, &too_large, true, false, 1);
        record(&changed, &original, true, false, 1);
        for _ in 0..MAX_ROWS {
            record(&changed, &original, true, false, 1);
        }
        let report = capture.finish();
        assert_eq!(report.rows.len(), MAX_ROWS);
        assert!(report.row_limit_reached);
        let rows = &report.rows;
        let difference = rows[1].adjacent.as_ref().unwrap();
        assert_eq!(difference.common_leading_messages, 0);
        assert!(rows[1].rebase && difference.system_same && !difference.tools_same);
        assert!(matches!(rows[2].status, Status::Cancelled) && rows[2].adjacent.is_none());
        assert_eq!((rows[3].request, rows[3].attempt), (3, 2));
        assert!(rows[3].gap && rows[3].adjacent.is_none());
        assert_eq!((rows[3].input, rows[3].cached), (Some(10), None));
        assert!(rows[4].over_allowance && rows[4].adjacent.is_none());
        assert!(rows[5].gap && rows[5].adjacent.is_none());
        assert!(rows[6].adjacent.as_ref().unwrap().previous_messages_preserved);
        assert!(!serde_json::to_string(&report).unwrap().contains("synthetic"));
    }
}
