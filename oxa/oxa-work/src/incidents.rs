//! Bounded incident reads and pure stall detection. A stall produces an incident, never a kill.
use crate::{events::Facts, Result};
use oxa_proto::work::{Evidence, EvidenceKind, Incident, IncidentKind, Subject};
use serde_json::Value;

pub fn recent(facts: &Facts) -> Result<Vec<Incident>> {
    facts.situation().map(|snapshot| snapshot.incidents)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    pub session_id: String,
    pub pid: u32,
    pub last_progress_ms: u64,
    pub deadline_ms: u64,
}
impl Stall {
    /// Build an incident for the runner to append. This never wraps, cancels or kills a worker.
    pub fn incident(&self, task: &str) -> Incident {
        Incident {
            id: format!("stall:{}:{}:{}", self.session_id, self.pid, self.last_progress_ms),
            subject: Subject::Task(task.into()),
            kind: IncidentKind::WorkerStalled,
            evidence: Some(Evidence {
                kind: EvidenceKind::Status,
                id: format!("session:{}", self.session_id),
            }),
        }
    }
}
/// Consume a bounded status snapshot supplied by the runner, never a transcript or tool output.
pub fn detect_stall(bytes: &[u8], session_id: &str, pid: u32, now_ms: u64) -> Result<Option<Stall>> {
    if pid == 0
        || session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err("invalid expected worker identity".into());
    }
    if bytes.len() > 4096 {
        return Err("worker status capacity exceeded".into());
    }
    let status: Value = serde_json::from_slice(bytes).map_err(|_| "invalid worker status JSON")?;
    if status["type"] != "oxa_status"
        || status["version"] != 1
        || status["session_id"] != session_id
        || status["pid"].as_u64() != Some(pid.into())
    {
        return Err("worker status identity or version mismatch".into());
    }
    let phase = status["phase"].as_str().ok_or("worker status phase is missing")?;
    let outcome = status.get("outcome").ok_or("worker status outcome is missing")?;
    let deadline = status
        .get("stall_deadline_ms")
        .ok_or("worker status deadline is missing")?;
    let timeout = status
        .get("stall_timeout_ms")
        .ok_or("worker status timeout is missing")?;
    let timeout = if timeout.is_null() {
        None
    } else {
        Some(
            timeout
                .as_u64()
                .filter(|timeout| *timeout > 0)
                .ok_or("invalid worker stall timeout")?,
        )
    };
    if !matches!(
        phase,
        "idle" | "working" | "model" | "prefill" | "compaction" | "tool" | "closing" | "finished"
    ) {
        return Err("unknown worker status phase".into());
    }
    if phase == "finished" {
        if outcome["type"] != "oxa_result"
            || !matches!(
                outcome["outcome"].as_str(),
                Some("done" | "nothing_to_do" | "blocked" | "failed" | "interrupted")
            )
            || outcome["exit_code"].as_i64().is_none()
            || !deadline.is_null()
        {
            return Err("finished worker status has no outcome".into());
        }
        return Ok(None);
    }
    if !outcome.is_null() {
        return Err("active worker status has a terminal outcome".into());
    }
    let progress = status["last_progress_ms"]
        .as_u64()
        .ok_or("worker status progress is missing")?;
    if deadline.is_null() {
        return Ok(None);
    }
    let deadline = deadline.as_u64().ok_or("invalid worker status deadline")?;
    let timeout = timeout.ok_or("worker deadline has no configured timeout")?;
    let earliest = progress.saturating_add(timeout);
    if deadline < earliest || deadline > earliest.saturating_add(1000) {
        return Err("worker status deadline is inconsistent with its timeout".into());
    }
    Ok((now_ms >= deadline).then(|| Stall {
        session_id: session_id.into(),
        pid,
        last_progress_ms: progress,
        deadline_ms: deadline,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::work::{Checkpoint, Event, Fact};
    use serde_json::json;
    fn status() -> Value {
        json!({"type":"oxa_status", "version":1, "session_id":"saved", "pid":123,
            "phase":"model", "last_progress_ms":100, "stall_timeout_ms":100, "stall_deadline_ms":200, "outcome":null})
    }
    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    #[test]
    fn deadline_is_conservative_and_detection_never_changes_the_input_or_takes_action() {
        let input = bytes(&status());
        let original = input.clone();
        assert!(detect_stall(&input, "saved", 123, 199).unwrap().is_none());
        let stall = detect_stall(&input, "saved", 123, 200).unwrap().unwrap();
        assert_eq!(stall.last_progress_ms, 100);
        assert_eq!(stall.deadline_ms, 200);
        let event = Event {
            seq: 1,
            at_ms: 200,
            checkpoint: Checkpoint {
                source: "runner".into(),
                revision: 1,
            },
            fact: Fact::Incident(stall.incident("task-1")),
        };
        assert_eq!(Event::from_json(&event.to_json().unwrap()).unwrap(), event);
        assert_eq!(input, original);
        let mut progress = status();
        progress["last_progress_ms"] = json!(190);
        progress["stall_deadline_ms"] = json!(290);
        assert!(detect_stall(&bytes(&progress), "saved", 123, 200)
            .unwrap()
            .is_none());
    }

    #[test]
    fn stale_identity_missing_fields_and_invalid_status_are_errors() {
        let input = bytes(&status());
        assert!(detect_stall(&input, "different", 123, 300).is_err());
        assert!(detect_stall(&input, "saved", 456, 300).is_err());
        for field in [
            "pid",
            "phase",
            "last_progress_ms",
            "stall_deadline_ms",
            "stall_timeout_ms",
            "outcome",
        ] {
            let mut invalid = status();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(
                detect_stall(&bytes(&invalid), "saved", 123, 300).is_err(),
                "{field}"
            );
        }
        let mut invalid = status();
        invalid["stall_deadline_ms"] = json!(50);
        assert!(detect_stall(&bytes(&invalid), "saved", 123, 300).is_err());
        assert!(detect_stall(&vec![b' '; 4097], "saved", 123, 300).is_err());
        assert!(detect_stall(b"{}", "saved", 123, 300).is_err());
    }

    #[test]
    fn idle_disabled_and_finished_sessions_do_not_stall() {
        let mut value = status();
        value["phase"] = json!("idle");
        value["stall_deadline_ms"] = Value::Null;
        assert!(detect_stall(&bytes(&value), "saved", 123, 300).unwrap().is_none());
        value["phase"] = json!("model");
        assert!(detect_stall(&bytes(&value), "saved", 123, 300).unwrap().is_none());
        value["phase"] = json!("finished");
        value["outcome"] = json!({"type":"oxa_result", "outcome":"done", "exit_code":0});
        assert!(detect_stall(&bytes(&value), "saved", 123, 300).unwrap().is_none());
        value["outcome"] = Value::Null;
        assert!(detect_stall(&bytes(&value), "saved", 123, 300).is_err());
    }
}
