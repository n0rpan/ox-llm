//! F8: a Rust RPC client.
// ported from pi packages/coding-agent/src/modes/rpc/rpc-client.ts
use oxa_proto::{codes, OxaError, Value};
use serde_json::Map;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// Drives an `oxa --mode rpc` child process (the SDK for other processes; also used by the
/// parity tests and the `examples/rpc-client` flow).
pub struct RpcClient {
    child: Child,
    records: Receiver<Value>,
    next_id: u64,
    /// Events that arrived while a request waited for its response, oldest first.
    buffered: VecDeque<Value>,
}

fn is_response(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("response")
}

fn type_of(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

impl RpcClient {
    /// Spawn `<bin> --mode rpc <args..>` with the given environment and cwd.
    pub fn spawn(
        bin: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RpcClient, OxaError> {
        let mut child = Command::new(bin)
            .arg("--mode")
            .arg("rpc")
            .args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| OxaError::io(&format!("cannot start {}", bin.display()), &e))?;
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                return Err(OxaError::new(codes::IO, "the rpc process has no stdout"));
            }
        };
        let (tx, records) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                // Lines that are not JSON are not records; pi's client ignores them too.
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(RpcClient {
            child,
            records,
            next_id: 0,
            buffered: VecDeque::new(),
        })
    }

    /// Send a command object (an `id` is added) and wait for its response, buffering events.
    /// Returns the whole response record (its `data` is the command's result). `Err` when the
    /// response has `success: false` (message = its `error`) or on timeout.
    pub fn request(&mut self, command: Value, timeout_ms: u64) -> Result<Value, OxaError> {
        let mut body = match command {
            Value::Object(m) => m,
            _ => return Err(OxaError::new(codes::INVALID, "a command is a JSON object")),
        };
        self.next_id += 1;
        let id = format!("req_{}", self.next_id);
        body.insert("id".to_string(), Value::from(id.as_str()));
        let kind = body.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        self.write_record(&Value::Object(body))?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self
                .records
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(v) if is_response(&v) => {
                    if v.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                        // A response to a request that was given up: nobody waits for it.
                        continue;
                    }
                    if v.get("success").and_then(Value::as_bool) == Some(true) {
                        return Ok(v);
                    }
                    let message = v
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the command failed")
                        .to_string();
                    return Err(OxaError::new(codes::INTERNAL, message));
                }
                Ok(v) => self.buffered.push_back(v),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(OxaError::new(
                        codes::TIMEOUT,
                        format!("no response to {kind} within {timeout_ms} ms"),
                    ))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(OxaError::new(
                        codes::IO,
                        format!("the rpc process ended before it answered {kind}"),
                    ))
                }
            }
        }
    }

    /// Next buffered or incoming event record (not a response); `None` on timeout.
    pub fn next_event(&mut self, timeout_ms: u64) -> Option<Value> {
        self.next_event_until(Instant::now() + Duration::from_millis(timeout_ms))
    }

    /// Collect events until one of type `kind` arrives (inclusive).
    pub fn wait_for(&mut self, kind: &str, timeout_ms: u64) -> Result<Vec<Value>, OxaError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut events = Vec::new();
        loop {
            match self.next_event_until(deadline) {
                Some(v) => {
                    let done = type_of(&v) == kind;
                    events.push(v);
                    if done {
                        return Ok(events);
                    }
                }
                None => {
                    return Err(OxaError::new(
                        codes::TIMEOUT,
                        format!("no {kind} record within {timeout_ms} ms (or the process ended)"),
                    ))
                }
            }
        }
    }

    /// Answer an `extension_ui_request`. `value` holds the answer fields of pi's response
    /// (`{"value": ..}`, `{"confirmed": true}`, `{"cancelled": true}`); a non-object value is
    /// sent as `value`.
    pub fn respond_ui(&mut self, id: &str, value: Value) -> Result<(), OxaError> {
        let mut m = Map::new();
        match value {
            Value::Object(fields) => m.extend(fields),
            other => {
                m.insert("value".to_string(), other);
            }
        }
        m.insert("type".to_string(), Value::from("extension_ui_response"));
        m.insert("id".to_string(), Value::from(id));
        self.write_record(&Value::Object(m))
    }

    /// Close stdin and wait for the exit code.
    pub fn close(mut self) -> Result<i32, OxaError> {
        drop(self.child.stdin.take());
        let status = self
            .child
            .wait()
            .map_err(|e| OxaError::io("cannot wait for the rpc process", &e))?;
        Ok(status.code().unwrap_or(-1))
    }

    fn next_event_until(&mut self, deadline: Instant) -> Option<Value> {
        if let Some(v) = self.buffered.pop_front() {
            return Some(v);
        }
        loop {
            match self
                .records
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(v) if is_response(&v) => continue,
                Ok(v) => return Some(v),
                Err(_) => return None,
            }
        }
    }

    fn write_record(&mut self, v: &Value) -> Result<(), OxaError> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| OxaError::new(codes::IO, "the rpc process stdin is closed"))?;
        let mut line = v.to_string();
        line.push('\n');
        let written = stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush());
        written.map_err(|e| OxaError::io("cannot write to the rpc process", &e))
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        // A client dropped without `close` must not leave its process running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
