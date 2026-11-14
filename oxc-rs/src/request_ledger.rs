//! Private, advisory request records. Request/reply text is reduced to keyed digests in memory.
use crate::{ledger, paths};
use axum::http::HeaderMap;
use chrono::{Duration, Local};
use hmac::{Hmac, Mac};
use nix::{libc, unistd::Uid};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const REPLY_MAX: usize = 1 << 20;
const TAIL_BYTES: usize = 16384;
type Hash = [u8; 16];
type HistoryKey = (String, Value, Value, Value);
#[derive(Default)]
struct State {
    salt: Option<[u8; 32]>,
    pruned_day: Option<String>,
    gaps: VecDeque<(String, Instant)>,
    history: VecDeque<(HistoryKey, History)>,
    sequence: u64,
}
#[derive(Clone)]
struct History {
    sequence: u64,
    hashes: Vec<Hash>,
    roles: Vec<Value>,
    row: Value,
    reply: Option<Hash>,
    expected: Option<f64>,
}
#[derive(Default)]
pub struct Ledger(Mutex<State>);

pub fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite())
}
fn round(value: f64, places: u32) -> f64 {
    let scale = 10_f64.powi(places as i32);
    (value * scale).round_ties_even() / scale
}
fn numeric(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
fn mac(salt: &[u8; 32], data: &[u8]) -> [u8; 32] {
    let mut hash = Hmac::<Sha256>::new_from_slice(salt).expect("HMAC accepts 32 bytes");
    hash.update(data);
    hash.finalize().into_bytes().into()
}
fn private_dir() -> io::Result<std::path::PathBuf> {
    let root = ledger::directory();
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(root)
}
fn salt(state: &mut State) -> Option<[u8; 32]> {
    if let Some(salt) = state.salt {
        return Some(salt);
    }
    let read = || -> io::Result<[u8; 32]> {
        let path = private_dir()?.join(".salt");
        let open = || {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
        };
        let file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut random = [0; 32];
                getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                file.write_all(&random)?;
                drop(file);
                open()?
            }
            Err(error) => return Err(error),
        };
        let info = file.metadata()?;
        if !info.is_file() || info.uid() != Uid::current().as_raw() || info.mode() & 0o077 != 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut value = Vec::new();
        file.take(64).read_to_end(&mut value)?;
        value
            .try_into()
            .map_err(|_| io::ErrorKind::InvalidData.into())
    };
    state.salt = read().ok();
    state.salt
}
fn keyed(salt: Option<&[u8; 32]>, domain: &[u8], data: &[u8]) -> Value {
    salt.map(|salt| json!(&hex(&mac(salt, &[domain, b"\0", data].concat()))[..12]))
        .unwrap_or(Value::Null)
}
fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}
pub(crate) fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<Cow<'a, str>> {
    let value = headers.get(key)?;
    Some(match value.to_str() {
        Ok(value) => Cow::Borrowed(value),
        // Python's HTTP header parser decodes field bytes as ISO-8859-1.
        Err(_) => Cow::Owned(value.as_bytes().iter().map(|b| char::from(*b)).collect()),
    })
}
fn valid_hint(value: &str) -> bool {
    (1..=200).contains(&value.chars().count())
}
fn session(headers: &HeaderMap, payload: &Value, salt: Option<&[u8; 32]>) -> Option<String> {
    let hint = ["x-session-id", "session-id"]
        .iter()
        .find_map(|name| header(headers, name).filter(|v| valid_hint(v)))
        .or_else(|| {
            payload["prompt_cache_key"]
                .as_str()
                .map(Cow::Borrowed)
                .filter(|v| valid_hint(v))
        });
    let raw = match hint {
        Some(hint) => format!("h:{hint}"),
        None => format!(
            "m:{}",
            text(
                &payload["messages"]
                    .as_array()?
                    .iter()
                    .find(|m| m["role"] == "user")?["content"]
            )
        ),
    };
    Some(hex(&mac(salt?, raw.as_bytes()))[..16].into())
}
fn product(agent: &str) -> Option<String> {
    let end = agent
        .bytes()
        .take(32)
        .take_while(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
        .count();
    if end == 0 {
        return None;
    }
    let mut token = agent[..end].to_owned();
    if let Some(version) = agent[end..].strip_prefix('/') {
        let count = version
            .bytes()
            .take(24)
            .take_while(|b| b.is_ascii_alphanumeric() || b"._+-".contains(b))
            .count();
        if count > 0 {
            token.push('/');
            token.push_str(&version[..count]);
        }
    }
    Some(token)
}
pub(crate) fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&map[key])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        _ => value.clone(),
    }
}
fn message_hash(salt: &[u8; 32], message: &Value) -> Hash {
    mac(
        salt,
        &[b"ledger-message-v1\0", message.to_string().as_bytes()].concat(),
    )[..16]
        .try_into()
        .expect("digest length")
}
fn canon_args(value: &Value) -> String {
    let value = match value.as_str() {
        Some(value) => match serde_json::from_str(value) {
            Ok(value) => value,
            Err(_) => return value.into(),
        },
        None => value.clone(),
    };
    sorted(&value).to_string()
}
fn reply_digest(
    salt: &[u8; 32],
    content: &str,
    calls: impl Iterator<Item = (String, String)>,
) -> Hash {
    let calls = calls
        .map(|(name, args)| json!([name, canon_args(&json!(args))]))
        .collect::<Vec<_>>();
    let data = paths::unicode_json(&json!([content.trim(), calls]));
    mac(salt, &[b"ledger-reply-v1\0", data.as_bytes()].concat())[..16]
        .try_into()
        .expect("digest length")
}
fn message_reply(salt: &[u8; 32], message: &Value) -> Hash {
    let calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.is_object())
        .map(|c| {
            (
                c["function"]["name"].as_str().unwrap_or("").into(),
                c["function"]["arguments"].as_str().unwrap_or("").into(),
            )
        });
    reply_digest(salt, &text(&message["content"]), calls)
}
#[derive(Default)]
struct Reply {
    json: Option<bool>,
    buffer: Vec<u8>,
    text: String,
    calls: BTreeMap<i64, (String, String)>,
    size: usize,
    broken: bool,
}
impl Reply {
    fn choice(&mut self, value: &Value, key: &str) {
        let part = &value["choices"][0][key];
        if let Some(text) = part["content"].as_str() {
            self.text.push_str(text);
            self.size += text.chars().count();
        }
        for call in part["tool_calls"].as_array().into_iter().flatten() {
            let index = if key == "delta" {
                call["index"].as_i64().unwrap_or(self.calls.len() as i64)
            } else {
                self.calls.len() as i64
            };
            let slot = self.calls.entry(index).or_default();
            slot.0
                .push_str(call["function"]["name"].as_str().unwrap_or(""));
            let args = &call["function"]["arguments"];
            if let Some(args) = args.as_str() {
                slot.1.push_str(args);
            } else if !args.is_null() {
                slot.1.push_str(&paths::python_json(args));
            }
            self.size += slot.1.chars().count();
        }
    }
    fn feed(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        if self.json.is_none() && !bytes.trim_ascii().is_empty() {
            self.json = Some(bytes.trim_ascii_start().starts_with(b"{"));
        }
        self.buffer.extend_from_slice(bytes);
        if self.json == Some(true) {
            self.broken = self.buffer.len() > REPLY_MAX;
            return;
        }
        while let Some(index) = self.buffer.iter().position(|b| *b == b'\n') {
            let line = self.buffer.drain(..=index).collect::<Vec<_>>();
            if let Some(data) = line.strip_prefix(b"data:").map(<[u8]>::trim_ascii) {
                if !data.is_empty() && data != b"[DONE]" {
                    match serde_json::from_slice::<Value>(data) {
                        Ok(value) => self.choice(&value, "delta"),
                        Err(_) => self.broken = true,
                    }
                }
            }
        }
        self.broken |= self.size > REPLY_MAX || self.buffer.len() > REPLY_MAX;
    }
    fn digest(mut self, salt: &[u8; 32]) -> Option<Hash> {
        if self.broken {
            return None;
        }
        if self.json == Some(true) {
            let value = serde_json::from_slice::<Value>(&self.buffer).ok()?;
            self.choice(&value, "message");
        }
        if self.text.is_empty() && self.calls.is_empty() {
            return None;
        }
        Some(reply_digest(salt, &self.text, self.calls.into_values()))
    }
}

pub struct Entry {
    ledger: Arc<Ledger>,
    pub row: Value,
    start: Instant,
    admitted: Option<Instant>,
    first_byte: Option<Instant>,
    tail: Vec<u8>,
    reply: Option<Reply>,
    history_key: Option<HistoryKey>,
    history: Option<History>,
    previous: Option<Value>,
}
impl Ledger {
    pub fn session_hint(&self, hint: &str) -> Option<String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let salt = salt(&mut state)?;
        Some(hex(&mac(&salt, format!("h:{hint}").as_bytes()))[..16].into())
    }
    pub fn entry(
        self: &Arc<Self>,
        path: &str,
        payload: &Value,
        headers: &HeaderMap,
        model: &Value,
    ) -> Entry {
        let start = Instant::now();
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let salt = salt(&mut state);
        let mut id = [0; 6];
        let _ = getrandom::fill(&mut id);
        let messages = payload["messages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let system = messages
            .iter()
            .find(|m| m["role"] == "system")
            .map(|m| text(&m["content"]));
        let session = session(headers, payload, salt.as_ref());
        let gap = session
            .as_ref()
            .and_then(|s| state.gaps.iter().find(|(key, _)| key == s))
            .map(|(_, time)| round(start.duration_since(*time).as_secs_f64() * 1000.0, 1));
        let origin = if header(headers, "x-local-ai-origin")
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("lab"))
        {
            "lab"
        } else {
            "organic"
        };
        let tools = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect::<Vec<_>>();
        let row = json!({"id":hex(&id),"t":round(now(),3),"path":path,"model":model,"origin":origin,
            "client":header(headers,"user-agent").and_then(|v|product(&v)),
            "agent":system.as_ref().map(|s|keyed(salt.as_ref(),b"ledger-agent-v1",s.as_bytes())).unwrap_or(Value::Null),
            "n_messages":messages.len(),"last_role":messages.last().and_then(|m|m["role"].as_str()),
            "n_tools":payload["tools"].as_array().map(Vec::len).unwrap_or(0),"stream":truth(&payload["stream"]),
            "max_tokens":payload.get("max_tokens").unwrap_or(&payload["n_predict"]).as_number(),
            "temperature":payload["temperature"].as_number(),"n_tool_msgs":tools.len(),
            "tool_bytes":tools.iter().take(128).map(|m|text(&m["content"]).len()).collect::<Vec<_>>(),
            "session":session,"gap_ms":gap});
        let mut entry = Entry {
            ledger: self.clone(),
            row,
            start,
            admitted: None,
            first_byte: None,
            tail: Vec::new(),
            reply: chat_path(path).then(Reply::default),
            history_key: None,
            history: None,
            previous: None,
        };
        if let (Some(session), Some(salt)) = (session, salt) {
            if chat_path(path) && payload["messages"].is_array() {
                let key = (
                    session,
                    model.clone(),
                    entry.row["agent"].clone(),
                    json!(origin),
                );
                if messages.len() > 4096 {
                    entry.row["history_change"] = json!("over_limit");
                    state.history.retain(|(k, _)| k != &key);
                } else {
                    state.sequence += 1;
                    let hashes = messages
                        .iter()
                        .map(|m| message_hash(&salt, m))
                        .collect::<Vec<_>>();
                    let roles = messages
                        .iter()
                        .map(|m| match m["role"].as_str() {
                            Some("system" | "user" | "assistant" | "tool") => m["role"].clone(),
                            _ => Value::Null,
                        })
                        .collect();
                    let previous = state
                        .history
                        .iter()
                        .find(|(k, _)| k == &key)
                        .map(|(_, record)| record);
                    let common = previous
                        .map(|p| {
                            p.hashes
                                .iter()
                                .zip(&hashes)
                                .take_while(|(a, b)| a == b)
                                .count()
                        })
                        .unwrap_or(0);
                    let change = if let Some(previous) = previous {
                        entry.row["previous_request_id"] = previous.row["id"].clone();
                        if let Some(expected) = previous.expected {
                            entry.row["previous_expected_cache"] = numeric(expected);
                        }
                        entry.previous = Some(previous.row.clone());
                        if common < hashes.len().min(previous.hashes.len()) {
                            "edited"
                        } else if hashes.len() < previous.hashes.len() {
                            "truncated"
                        } else if hashes.len() == previous.hashes.len() {
                            "same"
                        } else {
                            "append"
                        }
                    } else {
                        "initial"
                    };
                    entry.row["history_change"] = json!(change);
                    entry.row["history_common_messages"] = json!(common);
                    if let Some(previous) = previous {
                        if ["edited", "truncated"].contains(&change) {
                            entry.row["first_divergent_message"] = json!(common);
                            entry.row["first_divergent_message_hash"] =
                                json!(hashes.get(common).map(|v| hex(v)));
                            entry.row["previous_divergent_message_hash"] =
                                json!(hex(&previous.hashes[common]));
                            entry.row["first_divergent_role"] = previous.roles[common].clone();
                        } else if ["same", "append"].contains(&change) {
                            let next = if change == "append" {
                                messages.get(previous.hashes.len())
                            } else {
                                None
                            };
                            let reply = if !next.is_some_and(|m| m["role"] == "assistant") {
                                "absent"
                            } else if let Some(reply) = previous.reply {
                                if message_reply(&salt, next.expect("assistant")) == reply {
                                    "present"
                                } else {
                                    "edited"
                                }
                            } else {
                                "unknown"
                            };
                            entry.row["previous_reply"] = json!(reply);
                        }
                    }
                    entry.history_key = Some(key);
                    entry.history = Some(History {
                        sequence: state.sequence,
                        hashes,
                        roles,
                        row: Value::Null,
                        reply: None,
                        expected: None,
                    });
                }
            }
        }
        entry
    }
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
fn chat_path(path: &str) -> bool {
    ["/v1/chat/completions", "/chat/completions"].contains(&path.split('?').next().unwrap_or(path))
}
impl Entry {
    pub fn label(
        &mut self,
        payload: &Value,
        headers: &HeaderMap,
        config: &Value,
        prefix: Option<&str>,
    ) {
        if chat_path(self.row["path"].as_str().unwrap_or("")) {
            self.row["harness"] = json!(crate::harness::client_label(
                payload,
                header(headers, "user-agent").as_deref()
            ));
            if let Some(prefix) = prefix {
                self.row["prefix_key"] = json!(&prefix[..prefix.len().min(16)]);
            }
            if let Some(label) = header(headers, "x-oxe-turn-kind")
                .map(|s| s.trim().to_owned())
                .filter(|s| valid_label(s))
            {
                self.row["turn_kind"] = json!(label);
            }
            if payload["tools"].as_array().is_some_and(|v| !v.is_empty()) {
                let hash = format!(
                    "{:x}",
                    Sha256::digest(sorted(&payload["tools"]).to_string().as_bytes())
                );
                let mut state = self.ledger.0.lock().unwrap_or_else(|e| e.into_inner());
                self.row["tools_hash"] = keyed(
                    salt(&mut state).as_ref(),
                    b"ledger-turn-v1",
                    &hash.as_bytes()[..12],
                );
            }
        }
        if ["ornith-35b-a3b-local", "qwen3.6-35b-a3b-local"]
            .iter()
            .any(|m| self.row["model"] == *m)
        {
            let valid = |v: &Value| {
                v.as_str()
                    .filter(|s| ["fast", "deep", "max"].contains(s))
                    .map(str::to_owned)
            };
            let mode =
                valid(&config["context_modes"]["models"][self.row["model"].as_str().unwrap_or("")])
                    .or_else(|| valid(&config["context_modes"]["default"]))
                    .unwrap_or_else(|| "fast".into());
            self.row["mode"] = json!(mode);
        }
    }
    pub fn prep(&mut self, elapsed: std::time::Duration) {
        self.row["ctl_prep_ms"] = json!(round(elapsed.as_secs_f64() * 1000.0, 2));
    }
    pub fn admitted(&mut self, queue: f64, begin: f64) {
        self.admitted = Some(Instant::now());
        self.row["queue_wait_ms"] = json!(round(queue * 1000.0, 1));
        self.row["admit_ms"] = json!(round(begin * 1000.0, 1));
    }
    pub fn backend_prep(&mut self, elapsed: std::time::Duration) {
        self.row["ctl_backend_ms"] = json!(round(elapsed.as_secs_f64() * 1000.0, 2));
    }
    pub fn observe(&mut self, bytes: &[u8]) {
        let mut lines = bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.trim_ascii().is_empty())
            .peekable();
        let comment =
            lines.peek().is_some() && lines.all(|line| line.trim_ascii_start().starts_with(b":"));
        if self.first_byte.is_none() && !comment {
            self.first_byte = Some(Instant::now());
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > TAIL_BYTES {
            self.tail.drain(..self.tail.len() - TAIL_BYTES);
        }
        if let Some(reply) = self.reply.as_mut() {
            reply.feed(bytes);
        }
    }
    pub fn finish(mut self, status: u16, error: Option<&str>) {
        self.row["status"] = json!(status);
        self.row["error"] = json!(error);
        self.row["wall_ms"] = json!(round(self.start.elapsed().as_secs_f64() * 1000.0, 1));
        if let (Some(admitted), Some(first)) = (self.admitted, self.first_byte) {
            self.row["ttfb_ms"] = json!(round(
                first.duration_since(admitted).as_secs_f64() * 1000.0,
                1
            ));
        }
        for (key, value) in parse_tail(&self.tail) {
            self.row[&key] = value;
        }
        for (source, destination) in [
            ("cache_n", "cached_tokens"),
            ("prompt_n", "new_tokens"),
            ("predicted_n", "output_tokens"),
            ("prompt_ms", "prefill_ms"),
            ("predicted_ms", "decode_ms"),
        ] {
            if number(&self.row[source]).is_some() {
                self.row[destination] = self.row[source].clone();
            }
        }
        if let (Some(cache), Some(prompt)) =
            (number(&self.row["cache_n"]), number(&self.row["prompt_n"]))
        {
            self.row["ctx_tokens"] = numeric(cache + prompt);
        }
        let complete = if status != 200 || error.is_some_and(|e| e != "client_disconnected") {
            "errored"
        } else if number(&self.row["predicted_n"]).is_none()
            || number(&self.row["cache_n"]).is_none()
        {
            "incomplete"
        } else if error.is_some() {
            "closed-after-done"
        } else {
            "complete"
        };
        self.row["completeness"] = json!(complete);
        if let (Some(expected), Some(cache)) = (
            number(&self.row["previous_expected_cache"]),
            number(&self.row["cache_n"]),
        ) {
            self.row["cache_shortfall_tokens"] = numeric((expected - cache).max(0.0));
        }
        if self.admitted.is_some() {
            thermal(&mut self.row);
        }
        let mut state = self.ledger.0.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(key), Some(mut history)) = (self.history_key.take(), self.history.take()) {
            if ["complete", "closed-after-done"].contains(&complete) {
                classify(self.previous.as_ref(), &mut self.row);
                history.row = self.row.clone();
                history.reply = self.reply.take().and_then(|r| r.digest(&salt(&mut state)?));
                history.expected = ["cache_n", "prompt_n", "predicted_n"]
                    .iter()
                    .map(|k| number(&self.row[k]).filter(|v| *v >= 0.0))
                    .collect::<Option<Vec<_>>>()
                    .map(|v| v.iter().sum());
                if !state
                    .history
                    .iter()
                    .any(|(k, h)| k == &key && h.sequence >= history.sequence)
                {
                    state.history.retain(|(k, _)| k != &key);
                    state.history.push_back((key, history));
                }
                while state.history.len() > 256
                    || state
                        .history
                        .iter()
                        .map(|(_, h)| h.hashes.len())
                        .sum::<usize>()
                        > 32768
                {
                    state.history.pop_front();
                }
            }
        }
        if let Some(session) = self.row["session"].as_str() {
            state.gaps.retain(|(key, _)| key != session);
            state.gaps.push_back((session.into(), Instant::now()));
            while state.gaps.len() > 512 {
                state.gaps.pop_front();
            }
        }
        let _ = write(&mut state, &self.row);
    }
}
fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn parse_tail(tail: &[u8]) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(i) = tail.windows(9).rposition(|w| w == b"\"timings\"") {
        if let Some(j) = tail[i..].iter().position(|b| *b == b'{') {
            if let Some(Ok(t)) = serde_json::Deserializer::from_slice(&tail[i + j..])
                .into_iter::<Value>()
                .next()
            {
                for key in [
                    "cache_n",
                    "prompt_n",
                    "prompt_ms",
                    "predicted_n",
                    "predicted_ms",
                    "draft_n",
                    "draft_n_accepted",
                    "prefix_lookup_ms",
                    "prefix_load_ms",
                    "session_lookup_ms",
                    "session_load_ms",
                    "echo_blocks",
                    "echo_n",
                    "echo_fixed",
                    "echo_tokens",
                    "roundtrip_miss",
                    "resume_ms",
                    "preload_ms",
                    "render_ms",
                    "tokenize_ms",
                    "restore_ms",
                    "ckpt_ms",
                    "queue_ms",
                ] {
                    if number(&t[key]).is_some() {
                        out.insert(key.into(), t[key].clone());
                    }
                }
                let reasons = &[
                    "bypass",
                    "disabled",
                    "below-minimum",
                    "empty-index",
                    "prefix-mismatch",
                    "covered",
                    "hit",
                    "load-error",
                ][..];
                for (key, allowed) in [
                    (
                        "cache_src",
                        &[
                            "none",
                            "live",
                            "parked",
                            "parked-checkpoint",
                            "checkpoint",
                            "session",
                            "store",
                            "state",
                            "preload",
                        ][..],
                    ),
                    ("prefix_reason", reasons),
                    ("session_reason", reasons),
                    ("resume_src", &["live", "ram", "disk", "cold"][..]),
                    ("preload_src", &["disk"][..]),
                    (
                        "echo_miss_class",
                        &[
                            "no-record",
                            "content-mismatch",
                            "tool-mismatch",
                            "reasoning-mismatch",
                            "layout",
                        ][..],
                    ),
                ] {
                    if t[key].as_str().is_some_and(|v| allowed.contains(&v)) {
                        out.insert(key.into(), t[key].clone());
                    }
                }
            }
        }
    }
    for start in tail
        .windows(15)
        .enumerate()
        .filter_map(|(i, w)| (w == b"\"finish_reason\"").then_some(i + 15))
    {
        let rest = tail[start..].trim_ascii_start();
        let Some(rest) = rest
            .strip_prefix(b":")
            .map(<[u8]>::trim_ascii_start)
            .and_then(|s| s.strip_prefix(b"\""))
        else {
            continue;
        };
        let end = rest
            .iter()
            .take_while(|b| b.is_ascii_lowercase() || **b == b'_')
            .count();
        if (1..=24).contains(&end) && rest.get(end) == Some(&b'"') {
            out.insert(
                "finish_reason".into(),
                json!(String::from_utf8_lossy(&rest[..end])),
            );
        }
    }
    out
}

fn classify(previous: Option<&Value>, row: &mut Value) {
    let Some(prev) = previous else { return };
    let (Some(ctx), Some(cache)) = (number(&prev["ctx_tokens"]), number(&row["cache_n"])) else {
        return;
    };
    let predicted = number(&prev["predicted_n"]).unwrap_or(0.0).trunc();
    let ctx = ctx.trunc();
    let cache = cache.trunc();
    let hist = row["history_change"].as_str().unwrap_or("");
    let reply = row["previous_reply"].as_str().unwrap_or("");
    let (eligible, basis) =
        if ["present", "unknown"].contains(&reply) || ["edited", "truncated"].contains(&hist) {
            (true, "history")
        } else if ["edited", "absent"].contains(&reply) || hist == "same" {
            (false, "history")
        } else if matches!(
            row["echo_miss_class"].as_str(),
            Some("content-mismatch" | "tool-mismatch")
        ) {
            (false, "echo")
        } else {
            (true, "counts")
        };
    let mut expected = ctx + predicted - 1.0;
    let (class, miss, basis) = if cache >= expected {
        ("append-ok", 0.0, basis)
    } else {
        if !eligible {
            expected = ctx - 1.0;
        }
        let miss = expected - cache;
        if !eligible && cache >= expected {
            ("retry", 0.0, basis)
        } else if row["turn_kind"] == "compaction"
            || (row["turn_kind"] == "post-compaction" && prev["turn_kind"] != "post-compaction")
        {
            ("history-rewrite", miss, "label")
        } else if prev["template_sha"].as_str().is_some_and(|s| !s.is_empty())
            && row["template_sha"].as_str().is_some_and(|s| !s.is_empty())
            && prev["template_sha"] != row["template_sha"]
        {
            ("template-drift", miss, "counts")
        } else if let Some(class) = match row["echo_miss_class"].as_str() {
            Some("content-mismatch" | "tool-mismatch") => Some("history-edit"),
            Some("reasoning-mismatch") => Some("reasoning-drop"),
            Some("layout") => Some("template-drift"),
            _ => None,
        } {
            (class, miss, "echo")
        } else if hist == "truncated" {
            ("history-rewrite", miss, "history")
        } else if row["n_messages"]
            .as_i64()
            .zip(prev["n_messages"].as_i64())
            .is_some_and(|(a, b)| a < b)
        {
            ("history-rewrite", miss, "counts")
        } else if let Some(role) = row["first_divergent_role"].as_str() {
            if role == "tool" {
                ("tool-rerender", miss, "divergence")
            } else if role == "assistant" {
                let last = row["first_divergent_message"]
                    .as_i64()
                    .zip(prev["n_messages"].as_i64())
                    .is_some_and(|(a, b)| a >= b - 1);
                (
                    if last && prev["finish_reason"] == "tool_calls" {
                        "tool-rerender"
                    } else {
                        "reasoning-drop"
                    },
                    miss,
                    "inferred",
                )
            } else {
                ("history-edit", miss, "divergence")
            }
        } else if !eligible {
            ("unknown", miss, basis)
        } else if predicted != 0.0
            && (miss - predicted).abs() <= 2.0_f64.max((predicted * 0.05).trunc())
        {
            (
                if prev["finish_reason"] == "tool_calls" {
                    "tool-rerender"
                } else {
                    "reasoning-drop"
                },
                miss,
                "counts",
            )
        } else if row["last_role"] == "tool" {
            ("tool-rerender", miss, "counts")
        } else if row["last_role"] == "user"
            && row["n_messages"]
                .as_i64()
                .zip(prev["n_messages"].as_i64())
                .is_some_and(|(a, b)| a == b + 1)
        {
            ("reasoning-drop", miss, "counts")
        } else {
            ("unknown", miss, "counts")
        }
    };
    row["miss_class"] = json!(class);
    row["miss_tokens"] = numeric(miss);
    row["miss_basis"] = json!(basis);
}
fn sys_int(path: std::path::PathBuf) -> Option<i64> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(64)
        .read_to_string(&mut text)
        .ok()?;
    text.trim().parse().ok()
}
fn thermal(row: &mut Value) {
    let mut zones = fs::read_dir("/sys/class/thermal")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|d| d.file_name().to_string_lossy().starts_with("thermal_zone"))
        .collect::<Vec<_>>();
    zones.sort_by_key(|d| d.file_name());
    let hottest = zones
        .into_iter()
        .take(128)
        .filter_map(|d| sys_int(d.path().join("temp")))
        .filter(|v| (-40000..=150000).contains(v))
        .max();
    row["thermal_max_c"] = json!(hottest.map(|v| round(v as f64 / 1000.0, 1)));
    let mut policies = fs::read_dir("/sys/devices/system/cpu/cpufreq")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|d| {
            let n = d
                .file_name()
                .to_string_lossy()
                .strip_prefix("policy")?
                .parse::<u64>()
                .ok()?;
            Some((n, d))
        })
        .collect::<Vec<_>>();
    policies.sort_by_key(|(n, _)| *n);
    row["cluster_mhz"] = json!(policies
        .into_iter()
        .take(16)
        .filter_map(|(_, d)| sys_int(d.path().join("scaling_cur_freq")))
        .filter(|v| *v > 0 && *v < 100000000)
        .map(|v| v / 1000)
        .collect::<Vec<_>>());
}
fn write(state: &mut State, row: &Value) -> io::Result<()> {
    if std::env::var("LOCAL_AI_LEDGER").as_deref() == Ok("0") {
        return Ok(());
    }
    let root = private_dir()?;
    let day = Local::now().format("%Y%m%d").to_string();
    if state.pruned_day.as_ref() != Some(&day) {
        state.pruned_day = Some(day.clone());
        let keep = std::env::var("LOCAL_AI_LEDGER_DAYS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(14.0)
            * 86400.0;
        for entry in fs::read_dir(&root)?.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("requests-")
                && name.ends_with(".jsonl")
                && entry
                    .metadata()
                    .ok()
                    .and_then(|i| i.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age.as_secs_f64() > keep)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(root.join(format!("requests-{day}.jsonl")))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(paths::compact_ascii_json(row).as_bytes())?;
    file.write_all(b"\n")
}
fn median(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        Value::Null
    } else if n % 2 == 1 {
        numeric(values[n / 2])
    } else {
        numeric(round((values[n / 2 - 1] + values[n / 2]) / 2.0, 3))
    }
}
pub fn recent(limit: usize) -> Value {
    let root = ledger::directory();
    if !root.is_dir() {
        return json!({"object":"list","available":false,"rows":[],"medians":{}});
    }
    let mut rows = Vec::new();
    let now = Local::now();
    for date in [now, now - Duration::days(1)] {
        let read = || -> io::Result<Vec<Value>> {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(root.join(format!("requests-{}.jsonl", date.format("%Y%m%d"))))?;
            let info = file.metadata()?;
            if !info.is_file() {
                return Ok(Vec::new());
            }
            let start = info.len().saturating_sub(1 << 20);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::new();
            file.take(1 << 20).read_to_end(&mut bytes)?;
            let data = if start > 0 {
                bytes
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|i| &bytes[i + 1..])
                    .unwrap_or(&[])
            } else {
                &bytes
            };
            Ok(data
                .split(|b| *b == b'\n')
                .rev()
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .filter(Value::is_object)
                .collect())
        };
        if let Ok(found) = read() {
            rows.extend(found.into_iter().take(limit.clamp(1, 200) - rows.len()));
        }
        if rows.len() >= limit.clamp(1, 200) {
            break;
        }
    }
    let answered = rows
        .iter()
        .filter(|r| r["status"] == 200)
        .collect::<Vec<_>>();
    let mut medians = Map::new();
    for key in ["queue_wait_ms", "admit_ms", "ttfb_ms", "wall_ms"] {
        medians.insert(
            key.into(),
            median(answered.iter().filter_map(|r| number(&r[key])).collect()),
        );
    }
    for (name, count, ms) in [
        ("prompt_tokens_per_second", "prompt_n", "prompt_ms"),
        ("predicted_tokens_per_second", "predicted_n", "predicted_ms"),
    ] {
        medians.insert(
            name.into(),
            median(
                answered
                    .iter()
                    .filter_map(|r| {
                        let n = number(&r[count])?;
                        let ms = number(&r[ms])?;
                        (n >= 8.0 && ms > 0.0).then(|| round(n * 1000.0 / ms, 2))
                    })
                    .collect(),
            ),
        );
    }
    json!({"object":"list","available":true,"rows":rows,"medians":medians,"answered":answered.len()})
}
