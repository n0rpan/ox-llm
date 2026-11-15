//! Opt-in numeric records from a dedicated inherited pipe. Raw content never reaches storage.
use crate::{diagnostics, paths};
use nix::{fcntl::OFlag, unistd::pipe2};
use serde_json::{json, Map, Value};
use std::{
    io,
    os::fd::OwnedFd,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::unix::pipe, process::Command, sync::watch, task::JoinHandle};

pub const PIPE_ARGUMENT: &str = "__oxc_inherited_ledger_pipe__";
const NUMBERS: &[&str] = &[
    "t",
    "n_prompt",
    "cache_n",
    "prompt_n",
    "prompt_ms",
    "predicted_n",
    "predicted_ms",
    "draft_n",
    "draft_n_accepted",
    "parks",
    "unparks",
    "park_ms",
    "unpark_ms",
    "park_evictions",
    "checkpoints",
    "parked",
    "parked_mib",
    "prefix_lookup_ms",
    "prefix_load_ms",
    "session_lookup_ms",
    "session_load_ms",
    "resume_ms",
    "preload_ms",
];
const REASONS: &[&str] = &[
    "bypass",
    "disabled",
    "below-minimum",
    "empty-index",
    "prefix-mismatch",
    "covered",
    "hit",
    "load-error",
];
const ENUMS: &[(&str, &[&str])] = &[
    ("prefix_reason", REASONS),
    ("session_reason", REASONS),
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
        ],
    ),
    ("resume_src", &["live", "ram", "disk", "cold"]),
    ("preload_src", &["disk"]),
    ("stop", &["none", "eos", "word", "limit"]),
];

pub fn enabled(profile: &Value, args: &[String]) -> bool {
    profile["backend_kind"] == "oxe"
        && profile["environment"]["LOCAL_AI_ENGINE_LEDGER"] == "1"
        && !args
            .iter()
            .any(|arg| arg.split('=').next() == Some("--ledger"))
}

struct Counts {
    records: usize,
    dropped: usize,
    error: Option<&'static str>,
}
#[derive(Clone)]
pub struct Metadata {
    path: PathBuf,
    counts: Arc<Mutex<Counts>>,
}
impl Metadata {
    pub fn value(&self) -> Value {
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        json!({"enabled":true,"path":self.path,"records":counts.records,"dropped":counts.dropped,"error":counts.error})
    }
}

pub struct Prepared {
    reader: pipe::Receiver,
    writer: diagnostics::Writer,
    write_fd: OwnedFd,
    metadata: Metadata,
    context: Map<String, Value>,
}
impl Prepared {
    pub fn new(profile: &Value) -> io::Result<Self> {
        let path =
            paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/engine-ledger.jsonl");
        let writer = diagnostics::Writer::new(path.clone());
        if writer.error().is_some() {
            return Err(io::Error::other("native ledger log unavailable"));
        }
        let (read_fd, write_fd) = pipe2(OFlag::O_CLOEXEC)?;
        let reader = pipe::Receiver::from_owned_fd(read_fd)?;
        let context = Map::from_iter([
            ("event".into(), json!("generation")),
            ("model_alias".into(), profile["model_alias"].clone()),
            ("engine_sha256".into(), profile["server_sha256"].clone()),
        ]);
        Ok(Self {
            reader,
            writer,
            write_fd,
            context,
            metadata: Metadata {
                path,
                counts: Arc::new(Mutex::new(Counts {
                    records: 0,
                    dropped: 0,
                    error: None,
                })),
            },
        })
    }
    pub fn start(self, command: &mut Command) -> (Reader, Metadata) {
        // std::process safely maps this endpoint to fd 0 in the synchronous
        // exec helper. The helper duplicates it for inheritance, then restores
        // /dev/null stdin before exec. No parent-wide descriptor flags change.
        command.stdin(Stdio::from(self.write_fd));
        let metadata = self.metadata.clone();
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(drain(
            self.reader,
            self.writer,
            self.context,
            self.metadata,
            rx,
        ));
        (Reader { stop, task }, metadata)
    }
}

pub struct Reader {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
impl Reader {
    pub async fn stop(mut self) {
        self.stop.send_replace(true);
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

// Python json.loads accepts these three constants. Replace only bare constants
// outside strings; null fails numeric validation and leaves valid fields intact.
fn python_json(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let (mut index, mut string, mut escape) = (0, false, false);
    while index < bytes.len() {
        let byte = bytes[index];
        if string {
            out.push(byte);
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            string = true;
        }
        let constant = [b"NaN".as_slice(), b"Infinity", b"-Infinity"]
            .into_iter()
            .find(|word| {
                bytes[index..].starts_with(word)
                    && (index == 0 || b" \t\r\n[:,".contains(&bytes[index - 1]))
                    && bytes
                        .get(index + word.len())
                        .is_none_or(|b| b" \t\r\n,]}".contains(b))
            });
        if let Some(word) = constant {
            out.extend(b"null");
            index += word.len();
        } else {
            out.push(byte);
            index += 1;
        }
    }
    out
}
fn emit(
    line: &[u8],
    truncated: bool,
    writer: &mut diagnostics::Writer,
    context: &Map<String, Value>,
    metadata: &Metadata,
) {
    let row = (!truncated)
        .then(|| serde_json::from_slice::<Value>(&python_json(line)).ok())
        .flatten();
    let safe = row.and_then(|row| {
        let row = row.as_object()?;
        let mut safe = context.clone();
        for key in NUMBERS {
            if let Some(value) = row
                .get(*key)
                .filter(|v| v.is_number() && v.as_f64().is_some_and(f64::is_finite))
            {
                safe.insert((*key).into(), value.clone());
            }
        }
        if !["n_prompt", "predicted_n", "prompt_ms"]
            .iter()
            .all(|key| safe.contains_key(*key))
        {
            return None;
        }
        for (key, choices) in ENUMS {
            if let Some(value) = row
                .get(*key)
                .and_then(Value::as_str)
                .filter(|s| choices.contains(s))
            {
                safe.insert((*key).into(), json!(value));
            }
        }
        Some(safe)
    });
    if let Some(safe) = safe {
        // Match the reference's sorted, ASCII JSON with one complete record per write.
        let sorted: std::collections::BTreeMap<_, _> = safe.into_iter().collect();
        let mut bytes = paths::python_json(&json!(sorted)).into_bytes();
        bytes.push(b'\n');
        writer.write(&bytes);
        let mut counts = metadata.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.records += 1;
        counts.error = writer.error().or(counts.error);
    } else {
        metadata
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped += 1;
    }
}
async fn drain(
    reader: pipe::Receiver,
    mut writer: diagnostics::Writer,
    context: Map<String, Value>,
    metadata: Metadata,
    mut stop: watch::Receiver<bool>,
) {
    let mut pending = Vec::with_capacity(8192);
    let mut buffer = [0; 4096];
    let mut remaining = 256 * 1024;
    loop {
        let stopping = *stop.borrow();
        if stopping && remaining == 0 {
            break;
        }
        let result = if stopping {
            reader.try_read(&mut buffer[..remaining.min(4096)])
        } else {
            let ready = tokio::select! {
                _ = stop.changed() => continue,
                ready = reader.readable() => ready,
            };
            if let Err(error) = ready {
                metadata
                    .counts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error
                    .get_or_insert(diagnostics::error_name(&error));
                break;
            }
            reader.try_read(&mut buffer)
        };
        let size = match result {
            Ok(0) => break,
            Ok(size) => size,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stopping {
                    break;
                }
                continue;
            }
            Err(error) => {
                metadata
                    .counts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error
                    .get_or_insert(diagnostics::error_name(&error));
                break;
            }
        };
        if stopping {
            remaining -= size;
        }
        for byte in &buffer[..size] {
            if *byte == b'\n' {
                emit(&pending, false, &mut writer, &context, &metadata);
                pending.clear();
            } else {
                pending.push(*byte);
                if pending.len() == 8192 {
                    emit(&pending, true, &mut writer, &context, &metadata);
                    pending.clear();
                }
            }
        }
    }
    if !pending.is_empty() {
        emit(&pending, false, &mut writer, &context, &metadata);
    }
}
