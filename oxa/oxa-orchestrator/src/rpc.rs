//! Bounded tool-free RPC handshake and observed worker accounting. No I/O.
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Canceled,
    Truncated,
    TokenBudget,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Check,
    Configure,
    Verify,
    Running,
    Settled,
    Failed,
}

/// Commands returned here are admissions to the caller's pipe queue, not proof
/// of model consumption. The owner must force cleanup on error/terminal outcome
/// and independently confirm process stop before publishing a finished worker.
pub struct Update {
    pub commands: Vec<Value>,
    pub outcome: Option<Outcome>,
}

/// In-memory protocol state for an explicitly configured tool-free worker.
/// The caller supplies bounded records from JsonPipes and drives process/time
/// ownership separately. Observed token thresholds cannot preempt an in-flight
/// inference request or establish a hard monetary ceiling.
#[derive(Clone)]
pub struct RpcSession {
    model: String,
    task: String,
    phase: Phase,
    outcome: Option<Outcome>,
    configured: BTreeSet<String>,
    prompt_acked: bool,
    settled: Option<bool>,
    last_stop: Option<String>,
    result: String,
    completed_tokens: u64,
    pending_tokens: Option<u64>,
    usage_seen: bool,
    assistant_open: bool,
    token_limit: u64,
    events: usize,
    steering: u64,
    pending_steers: BTreeSet<String>,
}
impl RpcSession {
    pub fn new(model: String, task: String, token_limit: u64) -> io::Result<Self> {
        if model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
            || task.trim().is_empty()
            || task.len() > 64 * 1024
            || token_limit == 0
        {
            return Err(invalid("invalid worker RPC policy"));
        }
        bounded_limit(
            &json!({"id":"prompt","type":"prompt","message":task}),
            256 * 1024 - 1,
        )?;
        Ok(Self {
            model,
            task,
            phase: Phase::Check,
            outcome: None,
            configured: BTreeSet::new(),
            prompt_acked: false,
            settled: None,
            last_stop: None,
            result: String::new(),
            completed_tokens: 0,
            pending_tokens: None,
            usage_seen: false,
            assistant_open: false,
            token_limit,
            events: 0,
            steering: 0,
            pending_steers: BTreeSet::new(),
        })
    }
    pub fn initial_command(&self) -> Value {
        json!({"id":"check","type":"get_state"})
    }
    pub fn result(&self) -> &str {
        &self.result
    }
    /// Last valid observed usage, including in-progress usage; not a billing
    /// total or a claim that stopped/aborted requests have settled usage.
    pub fn observed_tokens(&self) -> Option<u64> {
        self.usage_seen.then(|| {
            self.completed_tokens
                .saturating_add(self.pending_tokens.unwrap_or(0))
        })
    }
    /// Terminal protocol/settlement result, independent of process cleanup.
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }
    /// Protocol rejection or failed settlement; other terminal reasons remain
    /// distinct in outcome(). This does not prove a process has stopped.
    pub fn is_failed(&self) -> bool {
        self.outcome == Some(Outcome::Failed)
    }
    pub fn accept(&mut self, record: Value) -> io::Result<Update> {
        if matches!(self.phase, Phase::Settled | Phase::Failed) {
            return Err(invalid("worker RPC already terminal"));
        }
        let mut candidate = self.clone();
        match candidate.accept_inner(record) {
            Ok(update) => {
                *self = candidate;
                Ok(update)
            }
            Err(error) => {
                self.phase = Phase::Failed;
                self.outcome = Some(Outcome::Failed);
                Err(error)
            }
        }
    }
    /// Bounded admission, not delivery acknowledgement. A queue failure must
    /// terminate the owning worker rather than retry or silently lose a steer.
    pub fn steer(&mut self, text: &str) -> io::Result<Value> {
        if self.phase != Phase::Running
            || self.settled.is_some()
            || text.trim().is_empty()
            || text.len() > 64 * 1024
            || self.pending_steers.len() >= 32
        {
            return Err(invalid("worker steer not admissible"));
        }
        let next = self
            .steering
            .checked_add(1)
            .ok_or_else(|| invalid("steer identity exhausted"))?;
        let id = format!("steer-{next}");
        let command = json!({"id":id,"type":"steer","message":text});
        bounded_limit(&command, 256 * 1024 - 1)?;
        self.steering = next;
        self.pending_steers.insert(id);
        Ok(command)
    }

    fn accept_inner(&mut self, record: Value) -> io::Result<Update> {
        bounded(&record)?;
        self.events += 1;
        if self.events > 16_384 {
            return Err(invalid("worker event allowance exceeded"));
        }
        let kind = record["type"]
            .as_str()
            .ok_or_else(|| invalid("worker record has no type"))?;
        let mut commands = Vec::new();
        match kind {
            "response" => self.response(&record, &mut commands)?,
            "mode_changed" if record["mode"] != "plan" => {
                return Err(invalid("worker permission policy changed"))
            }
            "model_select" if record["model"].as_str() != Some(&self.model) => {
                return Err(invalid("worker model changed"))
            }
            "tool_execution_start" => return Err(invalid("tool-free worker attempted a tool")),
            "extension_ui_request" => return Err(invalid("tool-free worker requested extension UI")),
            "message_start" if self.phase == Phase::Running && record["message"]["role"] == "assistant" => {
                if self.assistant_open {
                    return Err(invalid("duplicate assistant start"));
                }
                self.assistant_open = true;
                self.last_stop = None;
                self.pending_tokens = None;
            }
            "message_update" if self.phase == Phase::Running => {
                self.assistant_open = true;
                self.last_stop = None;
                if let Some(usage) = record.get("usage").filter(|v| !v.is_null()) {
                    let tokens = tokens(usage)?;
                    if self.pending_tokens.is_some_and(|old| tokens < old) {
                        return Err(invalid("worker usage decreased"));
                    }
                    self.completed_tokens
                        .checked_add(tokens)
                        .ok_or_else(|| invalid("worker usage overflow"))?;
                    self.pending_tokens = Some(tokens);
                    self.usage_seen = true;
                }
            }
            "message_end" if self.phase == Phase::Running => {
                let message = &record["message"];
                if message["role"] == "assistant" {
                    let total = tokens(&message["usage"])?;
                    if self.pending_tokens.is_some_and(|old| total < old) {
                        return Err(invalid("worker final usage decreased"));
                    }
                    self.completed_tokens = self
                        .completed_tokens
                        .checked_add(total)
                        .ok_or_else(|| invalid("worker usage overflow"))?;
                    self.pending_tokens = None;
                    self.assistant_open = false;
                    self.usage_seen = true;
                    self.last_stop = Some(
                        message["stopReason"]
                            .as_str()
                            .ok_or_else(|| invalid("missing worker stop reason"))?
                            .into(),
                    );
                    let content = message["content"]
                        .as_array()
                        .ok_or_else(|| invalid("invalid assistant content"))?;
                    let mut text = String::new();
                    for part in content {
                        match part["type"].as_str() {
                            Some("text") => text.push_str(
                                part["text"]
                                    .as_str()
                                    .ok_or_else(|| invalid("invalid worker text"))?,
                            ),
                            Some("thinking") => {}
                            Some("toolCall") => return Err(invalid("tool-free worker emitted a tool call")),
                            _ => return Err(invalid("unsupported worker content")),
                        }
                    }
                    self.result = text;
                }
            }
            "agent_settled" if self.phase == Phase::Running => {
                if self.settled.is_some() {
                    return Err(invalid("duplicate worker settlement"));
                }
                self.settled = Some(
                    record["aborted"]
                        .as_bool()
                        .ok_or_else(|| invalid("invalid worker settlement"))?,
                );
            }
            _ => {} // Unknown optional events still consume the same event/byte bounds.
        }
        let outcome = if self.observed_tokens().is_some_and(|n| n > self.token_limit) {
            Some(Outcome::TokenBudget)
        } else if self.prompt_acked && self.settled.is_some() {
            Some(if self.settled == Some(true) {
                Outcome::Canceled
            } else if !self.usage_seen || self.pending_tokens.is_some() || self.assistant_open {
                Outcome::Failed
            } else {
                match self.last_stop.as_deref() {
                    Some("stop") => Outcome::Completed,
                    Some("length") => Outcome::Truncated,
                    _ => Outcome::Failed,
                }
            })
        } else {
            None
        };
        if outcome.is_some() {
            self.phase = Phase::Settled;
            self.outcome = outcome;
        }
        Ok(Update { commands, outcome })
    }
    fn response(&mut self, record: &Value, commands: &mut Vec<Value>) -> io::Result<()> {
        if record["success"] != true {
            return Err(invalid("worker command rejected"));
        }
        let id = record["id"]
            .as_str()
            .ok_or_else(|| invalid("worker response missing identity"))?;
        let command = record["command"]
            .as_str()
            .ok_or_else(|| invalid("worker response missing command"))?;
        match (self.phase, id, command) {
            (Phase::Check, "check", "get_state") => {
                self.check_policy(&record["data"], false)?;
                self.phase = Phase::Configure;
                for (id, kind) in [("retry", "set_auto_retry"), ("compact", "set_auto_compaction")] {
                    self.configured.insert(id.into());
                    commands.push(json!({"id":id,"type":kind,"enabled":false}));
                }
            }
            (Phase::Configure, "retry", "set_auto_retry")
            | (Phase::Configure, "compact", "set_auto_compaction") => {
                if !self.configured.remove(id) {
                    return Err(invalid("duplicate worker configuration response"));
                }
                if self.configured.is_empty() {
                    self.phase = Phase::Verify;
                    commands.push(json!({"id":"verify","type":"get_state"}));
                }
            }
            (Phase::Verify, "verify", "get_state") => {
                self.check_policy(&record["data"], true)?;
                self.phase = Phase::Running;
                commands.push(json!({"id":"prompt","type":"prompt","message":self.task}));
            }
            (Phase::Running, "prompt", "prompt") if !self.prompt_acked => {
                self.prompt_acked = true;
            }
            (Phase::Running, _, "steer") if self.pending_steers.remove(id) => {}
            _ => return Err(invalid("unexpected worker command response")),
        }
        Ok(())
    }
    fn check_policy(&self, data: &Value, configured: bool) -> io::Result<()> {
        if data["mode"] != "plan"
            || data["model"]["id"].as_str() != Some(&self.model)
            || data["isStreaming"] != false
            || (configured && (data["autoRetryEnabled"] != false || data["autoCompactionEnabled"] != false))
        {
            return Err(invalid("worker policy verification failed"));
        }
        Ok(())
    }
}
fn tokens(usage: &Value) -> io::Result<u64> {
    ["input", "output", "cacheRead", "cacheWrite"]
        .iter()
        .try_fold(0u64, |total, key| {
            total
                .checked_add(
                    usage[key]
                        .as_u64()
                        .ok_or_else(|| invalid("invalid worker usage"))?,
                )
                .ok_or_else(|| invalid("worker usage overflow"))
        })
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn bounded(value: &Value) -> io::Result<()> {
    bounded_limit(value, 256 * 1024)
}
fn bounded_limit(value: &Value, limit: usize) -> io::Result<()> {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(invalid("worker record allowance exceeded"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Count(limit), value).map_err(|_| invalid("worker record allowance exceeded"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(limit: u64) -> RpcSession {
        RpcSession::new("mock".into(), "synthetic task".into(), limit).unwrap()
    }
    fn reply(id: &str, command: &str, data: Value) -> Value {
        json!({"type":"response","id":id,"command":command,"success":true,"data":data})
    }
    fn state() -> Value {
        json!({"model":{"id":"mock"},"mode":"plan","isStreaming":false,"autoRetryEnabled":false,"autoCompactionEnabled":false})
    }
    fn running(s: &mut RpcSession) {
        assert_eq!(s.initial_command(), json!({"id":"check","type":"get_state"}));
        let update = s.accept(reply("check", "get_state", state())).unwrap();
        assert_eq!(
            update.commands,
            [
                json!({"id":"retry","type":"set_auto_retry","enabled":false}),
                json!({"id":"compact","type":"set_auto_compaction","enabled":false})
            ]
        );
        assert!(s
            .accept(reply("compact", "set_auto_compaction", Value::Null))
            .unwrap()
            .commands
            .is_empty());
        assert_eq!(
            s.accept(reply("retry", "set_auto_retry", Value::Null))
                .unwrap()
                .commands,
            [json!({"id":"verify","type":"get_state"})]
        );
        assert_eq!(
            s.accept(reply("verify", "get_state", state())).unwrap().commands,
            [json!({"id":"prompt","type":"prompt","message":"synthetic task"})]
        );
    }
    fn usage(n: u64) -> Value {
        json!({"input":n,"output":2,"cacheRead":3,"cacheWrite":4})
    }
    fn end(stop: &str, n: u64, text: &str) -> Value {
        json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":text}],"stopReason":stop,"usage":usage(n)}})
    }
    fn settled(aborted: bool) -> Value {
        json!({"type":"agent_settled","aborted":aborted})
    }

    #[test]
    fn completed_requires_policy_verification_prompt_ack_usage_and_settlement() {
        let mut s = session(1000);
        running(&mut s);
        assert_eq!(s.observed_tokens(), None);
        assert!(s.accept(end("stop", 10, "first")).unwrap().outcome.is_none());
        assert_eq!(s.observed_tokens(), Some(19));
        assert!(s.accept(settled(false)).unwrap().outcome.is_none());
        assert_eq!(
            s.accept(reply("prompt", "prompt", Value::Null)).unwrap().outcome,
            Some(Outcome::Completed)
        );
        assert_eq!(s.result(), "first");
        assert_eq!(s.outcome(), Some(Outcome::Completed));
        assert!(s.accept(json!({"type":"optional"})).is_err());
    }
    #[test]
    fn usage_is_cumulative_and_progress_is_replaced_not_double_counted() {
        let mut s = session(1000);
        running(&mut s);
        s.accept(reply("prompt", "prompt", Value::Null)).unwrap();
        s.accept(json!({"type":"message_update","usage":usage(10)}))
            .unwrap();
        assert_eq!(s.observed_tokens(), Some(19));
        s.accept(json!({"type":"message_update","usage":usage(11)}))
            .unwrap();
        assert_eq!(s.observed_tokens(), Some(20));
        s.accept(end("stop", 12, "first")).unwrap();
        assert_eq!(s.observed_tokens(), Some(21));
        s.accept(json!({"type":"message_update","usage":usage(13)}))
            .unwrap();
        assert_eq!(s.observed_tokens(), Some(43));
        s.accept(end("stop", 14, "second")).unwrap();
        assert_eq!(s.observed_tokens(), Some(44));
        assert_eq!(
            s.accept(settled(false)).unwrap().outcome,
            Some(Outcome::Completed)
        );
    }
    #[test]
    fn length_is_attributed_as_truncation_with_partial_result_and_known_usage() {
        let mut s = session(1000);
        running(&mut s);
        s.accept(reply("prompt", "prompt", Value::Null)).unwrap();
        s.accept(end("length", 512, "partial tool-free answer")).unwrap();
        assert_eq!(
            s.accept(settled(false)).unwrap().outcome,
            Some(Outcome::Truncated)
        );
        assert_eq!(s.result(), "partial tool-free answer");
        assert_eq!(s.outcome(), Some(Outcome::Truncated));
        assert_eq!(s.observed_tokens(), Some(521));
    }
    #[test]
    fn exhaustion_retains_actual_observed_overrun_and_never_returns_completed() {
        let mut s = session(20);
        running(&mut s);
        s.accept(reply("prompt", "prompt", Value::Null)).unwrap();
        assert_eq!(
            s.accept(end("stop", 12, "attributed partial")).unwrap().outcome,
            Some(Outcome::TokenBudget)
        );
        assert_eq!(s.observed_tokens(), Some(21));
        assert_eq!(s.result(), "attributed partial");
        assert!(s.accept(settled(false)).is_err());
    }
    #[test]
    fn canceled_and_unknown_or_unfinished_usage_are_not_success() {
        for aborted in [false, true] {
            let mut s = session(1000);
            running(&mut s);
            s.accept(reply("prompt", "prompt", Value::Null)).unwrap();
            assert_eq!(
                s.accept(settled(aborted)).unwrap().outcome,
                Some(if aborted {
                    Outcome::Canceled
                } else {
                    Outcome::Failed
                })
            );
            assert_eq!(s.observed_tokens(), None);
        }
        let mut s = session(1000);
        running(&mut s);
        s.accept(reply("prompt", "prompt", Value::Null)).unwrap();
        s.accept(json!({"type":"message_update","usage":usage(10)}))
            .unwrap();
        assert_eq!(s.accept(settled(false)).unwrap().outcome, Some(Outcome::Failed));
        assert_eq!(s.observed_tokens(), Some(19));
    }
    #[test]
    fn policy_and_command_identity_changes_fail_closed_before_prompt_admission() {
        for kind in 0..5 {
            let mut s = session(1000);
            let mut observed = state();
            match kind {
                0 => observed["model"]["id"] = json!("different"),
                1 => observed["mode"] = json!("workspace"),
                2 => observed["isStreaming"] = json!(true),
                3 => observed["model"] = Value::Null,
                _ => {}
            }
            let record = if kind == 4 {
                reply("wrong", "get_state", observed)
            } else {
                reply("check", "get_state", observed)
            };
            assert!(s.accept(record).is_err());
            assert!(s.is_failed());
        }
        let mut s = session(1000);
        s.accept(reply("check", "get_state", state())).unwrap();
        s.accept(reply("retry", "set_auto_retry", Value::Null)).unwrap();
        s.accept(reply("compact", "set_auto_compaction", Value::Null))
            .unwrap();
        let mut changed = state();
        changed["autoRetryEnabled"] = json!(true);
        assert!(s.accept(reply("verify", "get_state", changed)).is_err());
    }
    #[test]
    fn malformed_usage_and_forbidden_tool_content_do_not_replace_valid_partial_state() {
        for kind in 0..4 {
            let mut s = session(1000);
            running(&mut s);
            s.accept(end("stop", 10, "retained")).unwrap();
            let mut next = end("stop", 11, "must not replace");
            match kind {
                0 => next["message"]["usage"]["output"] = json!(-1),
                1 => next["message"]["usage"]["input"] = json!(u64::MAX),
                2 => next["message"]["content"] = json!([{"type":"toolCall","name":"bash"}]),
                _ => next["message"]["stopReason"] = Value::Null,
            }
            assert!(s.accept(next).is_err());
            assert!(s.is_failed());
            assert_eq!(s.result(), "retained");
            assert_eq!(s.observed_tokens(), Some(19));
        }
        let mut s = session(1000);
        running(&mut s);
        s.accept(json!({"type":"message_update","usage":usage(10)}))
            .unwrap();
        assert!(s
            .accept(json!({"type":"message_update","usage":usage(9)}))
            .is_err());
        assert_eq!(s.observed_tokens(), Some(19));
    }
    #[test]
    fn steering_has_bounded_pending_identity_and_rejects_unadmitted_acknowledgements() {
        let mut s = session(1000);
        assert!(s.steer("too early").is_err());
        running(&mut s);
        for i in 1..=32 {
            assert_eq!(s.steer("synthetic steer").unwrap()["id"], format!("steer-{i}"));
        }
        assert!(s.steer("overflow").is_err());
        s.accept(reply("steer-1", "steer", Value::Null)).unwrap();
        assert_eq!(s.steer("replacement").unwrap()["id"], "steer-33");
        assert!(s.accept(reply("steer-100", "steer", Value::Null)).is_err());
    }
    #[test]
    fn unknown_events_and_record_sizes_cannot_bypass_bounds() {
        let mut s = session(1000);
        for _ in 0..16_384 {
            s.accept(json!({"type":"optional"})).unwrap();
        }
        assert!(s.accept(json!({"type":"optional"})).is_err());
        let mut s = session(1000);
        assert!(s
            .accept(json!({"type":"optional","large":"x".repeat(256*1024)}))
            .is_err());
        assert!(s.is_failed());
    }
    #[test]
    fn sandboxed_rpc_handshake_retains_truncated_result_and_then_confirms_stop() {
        use crate::native::{Launch, NativeWorker, VerifiedLocal};
        use oxa_fake::TestEnv;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let env = TestEnv::new();
        let script = r#"#!/usr/bin/python3 -I
import sys,json
state={'model':{'id':'mock'},'mode':'plan','isStreaming':False,'autoRetryEnabled':True,'autoCompactionEnabled':True}
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
 request=json.loads(line); command=request['type']
 if command=='abort': break
 data=None
 if command=='get_state': data=state
 elif command=='set_auto_retry': state['autoRetryEnabled']=request['enabled']
 elif command=='set_auto_compaction': state['autoCompactionEnabled']=request['enabled']
 send({'type':'response','id':request['id'],'command':command,'success':True,'data':data})
 if command=='prompt':
  send({'type':'message_end','message':{'role':'assistant','content':[{'type':'text','text':'partial answer'}],'stopReason':'length','usage':{'input':10,'output':512,'cacheRead':0,'cacheWrite':0}}})
  send({'type':'agent_settled','aborted':False})
"#;
        let binary = env.write("rpc-runtime.py", script);
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut worker = NativeWorker::spawn(Launch {
            worker_binary: binary,
            bwrap_binary: "/usr/bin/bwrap".into(),
            inference: VerifiedLocal::register("127.0.0.1:1".parse().unwrap(), "mock".into()).unwrap(),
            role: "synthetic role".into(),
            wall_time: Duration::from_secs(5),
            output_bytes: 128 * 1024,
        })
        .unwrap();
        let mut state = session(1000);
        worker.queue(&state.initial_command()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            let mut outcome = None;
            for record in worker.poll().unwrap() {
                let update = state.accept(record).unwrap();
                for command in update.commands {
                    worker.queue(&command).unwrap();
                }
                if let Some(result) = update.outcome {
                    outcome = Some(result);
                    break;
                }
            }
            if let Some(outcome) = outcome {
                break outcome;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(outcome, Outcome::Truncated);
        assert_eq!(state.result(), "partial answer");
        assert_eq!(state.observed_tokens(), Some(522));
        assert!(!worker.process_stopped().unwrap());
        worker.stop(Duration::from_millis(100)).unwrap();
        assert!(worker.process_stopped().unwrap());
    }
    #[test]
    fn a_new_unfinished_assistant_cannot_reuse_a_previous_completion() {
        let mut s = session(1000);
        running(&mut s);
        s.accept(reply("prompt", "prompt", Value::Null)).unwrap();
        s.accept(end("stop", 10, "prior valid partial")).unwrap();
        s.accept(json!({"type":"message_start","message":{"role":"assistant"}}))
            .unwrap();
        assert_eq!(s.accept(settled(false)).unwrap().outcome, Some(Outcome::Failed));
        assert_eq!(s.result(), "prior valid partial");
        assert_eq!(s.observed_tokens(), Some(19));
    }
    #[test]
    fn escaped_command_size_is_checked_before_admission_or_identity_mutation() {
        assert!(RpcSession::new("mock".into(), "\u{0001}".repeat(64 * 1024), 1000).is_err());
        let mut s = session(1000);
        running(&mut s);
        assert!(s.steer(&"\u{0001}".repeat(64 * 1024)).is_err());
        assert_eq!(s.steer("bounded").unwrap()["id"], "steer-1");
    }
}
