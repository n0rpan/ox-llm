//! Bounded, deterministic cards for the lead commands. Callers fetch the situation
//! through the runner API; this module never reads files or reconstructs logs.
use oxa_proto::work::{Snapshot, Subject};

/// A successfully decoded snapshot is still an observation at its sequence, not a
/// fresh assertion about the machine. The caller supplies its freshness policy.
pub struct Cards {
    snapshot: Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    Invalid,
    Stale,
    Future,
}

impl Cards {
    /// Accept the typed result of the runner's public situation read. Validation
    /// needs no JSON reparse and does not expose the storage location to cards.
    pub fn from_snapshot(snapshot: Snapshot, now_ms: u64, max_age_ms: u64) -> Result<Self, Unavailable> {
        snapshot.to_json().map_err(|_| Unavailable::Invalid)?;
        Self::observed(snapshot, now_ms, max_age_ms)
    }

    pub fn decode(bytes: &[u8], now_ms: u64, max_age_ms: u64) -> Result<Self, Unavailable> {
        let snapshot = Snapshot::from_json(bytes).map_err(|_| Unavailable::Invalid)?;
        Self::observed(snapshot, now_ms, max_age_ms)
    }

    fn observed(snapshot: Snapshot, now_ms: u64, max_age_ms: u64) -> Result<Self, Unavailable> {
        let age = now_ms.checked_sub(snapshot.at_ms).ok_or(Unavailable::Future)?;
        if age > max_age_ms {
            return Err(Unavailable::Stale);
        }
        Ok(Self { snapshot })
    }

    fn source(&self) -> String {
        let s = &self.snapshot;
        let standing = if s.standing_revision == 0 {
            "unavailable".to_owned()
        } else {
            s.standing_revision.to_string()
        };
        format!(
            "Runner observation: seq {} · standing {standing} · at {} ms",
            s.as_of_seq, s.at_ms
        )
    }

    /// At most 12 physical lines, source included. No queue or hold fact exists in
    /// the current Snapshot type, so neither is inferred from blocks or chat.
    pub fn status(&self) -> String {
        let s = &self.snapshot;
        let mut lines = vec![
            self.source(),
            format!(
                "Goal: {}",
                s.goal
                    .as_deref()
                    .map(field)
                    .unwrap_or_else(|| "unavailable".into())
            ),
            format!(
                "Running: {} · locally accepted: {} · published: {} · merged: {}",
                s.running.len(),
                s.local_accepted.len(),
                s.published.len(),
                s.landed.len()
            ),
            format!(
                "Blocked: {} · asks: {} · incidents: {}",
                s.blocked.len(),
                s.asks.len(),
                s.incidents.len()
            ),
            "Queue / holds / external validation: unavailable in this snapshot".into(),
        ];
        for task in s.running.iter().take(2) {
            lines.push(format!(
                "Running {} on {} · {}",
                field(&task.task),
                field(&task.host),
                field(&task.branch)
            ));
        }
        if s.running.len() > 2 {
            lines.push(format!("{} more running tasks", s.running.len() - 2));
        }
        if let Some(block) = s.blocked.first() {
            lines.push(format!(
                "Block: {} · {}",
                subject(&block.subject),
                field(&block.code)
            ));
        }
        if let Some(ask) = s.asks.first() {
            lines.push(format!("Ask {}: {}", field(&ask.id), field(&ask.question)));
        }
        if s.history_truncated {
            lines.push("Terminal / incident history shortened by runner".into());
        }
        lines.join("\n")
    }

    /// One item at a time. Missing decision metadata is explicit, never invented
    /// from the question. This renderer grants no permission to resolve an ask.
    pub fn asks(&self) -> String {
        let s = &self.snapshot;
        let mut lines = vec![self.source()];
        if let Some(ask) = s.asks.first() {
            lines.extend([
                format!("Ask {} · {}", field(&ask.id), subject(&ask.subject)),
                format!("Question: {}", field(&ask.question)),
                "Options / recommendation / meanwhile: unavailable in current facts".into(),
                "Ask revision: unavailable; decision write disabled".into(),
                format!("{} other open asks", s.asks.len() - 1),
            ]);
        } else {
            lines.push("No open asks in this runner observation".into());
        }
        lines.join("\n")
    }
}

/// Fetch a fresh typed observation for each supported command. The lead adapter
/// supplies `|| facts.situation()`; no model, private file or log read is involved.
/// Unknown commands never invoke the reader. Storage errors stay opaque.
pub fn command<E>(
    command: &str,
    now_ms: u64,
    max_age_ms: u64,
    read: impl FnOnce() -> Result<Snapshot, E>,
) -> Option<String> {
    let render = match command.trim() {
        "/status" => Cards::status,
        "/asks" => Cards::asks,
        _ => return None,
    };
    Some(
        match read()
            .map_err(|_| Unavailable::Invalid)
            .and_then(|s| Cards::from_snapshot(s, now_ms, max_age_ms))
        {
            Ok(cards) => render(&cards),
            Err(reason) => unavailable(reason).to_owned(),
        },
    )
}

/// No raw error or rejected input is reflected into the terminal.
pub fn unavailable(reason: Unavailable) -> &'static str {
    match reason {
        Unavailable::Invalid => "Runner facts unavailable: missing or invalid situation; status unknown",
        Unavailable::Stale => "Runner facts unavailable: stale situation; status unknown",
        Unavailable::Future => "Runner facts unavailable: observation clock is ahead; status unknown",
    }
}

fn subject(value: &Subject) -> String {
    match value {
        Subject::Repository => "repository".into(),
        Subject::Task(id) => format!("task {}", field(id)),
        Subject::PullRequest(number) => format!("PR #{number}"),
        Subject::Host(id) => format!("host {}", field(id)),
        Subject::Inbox(id) => format!("inbox {}", field(id)),
    }
}

// Bounded single-line terminal text even when typed fixtures bypass the decoder.
// Escape controls and bidi formatting; truncate by characters, not UTF-8 bytes.
fn field(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().take(160) {
        if ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            out.extend(ch.escape_unicode());
        } else {
            out.push(ch);
        }
    }
    if value.chars().count() > 160 {
        out.push('…');
    }
    out
}
