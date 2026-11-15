use crate::paths;
use chrono::{Duration, Local};
use nix::libc;
use serde_json::{json, Map, Value};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
};

pub fn directory() -> std::path::PathBuf {
    std::env::var_os("OXC_LEDGER_DIR")
        .map(|value| {
            std::path::PathBuf::from(if value.is_empty() {
                std::ffi::OsStr::new(".")
            } else {
                &value
            })
        })
        .unwrap_or_else(|| paths::home().join(".local/state/oxc/ledger"))
}
fn number(value: &Value) -> Value {
    if value.as_f64().is_some_and(|v| (0.0..=1e18).contains(&v)) {
        value.clone()
    } else {
        Value::Null
    }
}
fn label(value: &Value, allowed: &[&str]) -> Option<Value> {
    value
        .as_str()
        .filter(|v| allowed.contains(v))
        .map(|_| value.clone())
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn row(value: &Value) -> Option<Value> {
    let path = value.get("path")?.as_str()?;
    if !path.ends_with("/completions") && path != "/completion" {
        return None;
    }
    let harness = value["harness"].as_str().filter(|v| name(v)).or_else(|| {
        value["client"]
            .as_str()
            .map(|s| s.split('/').next().unwrap_or(s))
            .filter(|v| name(v))
    });
    let mut out = Map::new();
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
        ("resume_src", &["live", "ram", "disk", "cold"][..]),
        ("preload_src", &["disk"][..]),
        (
            "miss_class",
            &[
                "append-ok",
                "retry",
                "history-edit",
                "reasoning-drop",
                "tool-rerender",
                "compaction",
                "template-drift",
                "unknown",
            ][..],
        ),
        ("mode", &["fast", "deep", "max"][..]),
        ("origin", &["organic", "lab", "remote"][..]),
    ] {
        if let Some(found) = label(&value[key], allowed) {
            out.insert(key.into(), found);
        }
    }
    for key in ["echo_tokens", "miss_tokens", "resume_ms", "preload_ms"] {
        let parsed = number(&value[key]);
        if !parsed.is_null() {
            out.insert(key.into(), parsed);
        }
    }
    out.insert("t".into(), number(&value["t"]));
    out.insert(
        "model".into(),
        match value["model"].as_str() {
            Some("ornith-35b-a3b-local") => json!("ornith"),
            Some("qwen3.6-35b-a3b-local") => json!("moe"),
            Some("qwen3.8-flash-next-local") => json!("flash"),
            _ => Value::Null,
        },
    );
    out.insert("harness".into(), json!(harness));
    out.insert("status".into(), number(&value["status"]));
    out.insert(
        "stream".into(),
        if value["stream"].is_boolean() {
            value["stream"].clone()
        } else {
            Value::Null
        },
    );
    for key in [
        "queue_wait_ms",
        "admit_ms",
        "ttfb_ms",
        "wall_ms",
        "cache_n",
        "prompt_n",
        "prompt_ms",
        "predicted_n",
        "predicted_ms",
    ] {
        out.insert(key.into(), number(&value[key]));
    }
    out.insert(
        "finish".into(),
        label(&value["finish_reason"], &["stop", "length", "tool_calls"]).unwrap_or(Value::Null),
    );
    Some(Value::Object(out))
}

fn read_day(path: std::path::PathBuf, limit: usize) -> Vec<Value> {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    else {
        return Vec::new();
    };
    let Ok(info) = file.metadata() else {
        return Vec::new();
    };
    if !info.is_file() {
        return Vec::new();
    }
    let start = info.len().saturating_sub(512 * 1024);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut data = Vec::new();
    if file.take(512 * 1024).read_to_end(&mut data).is_err() {
        return Vec::new();
    }
    let data = if start != 0 {
        match data.iter().position(|v| *v == b'\n') {
            Some(index) => &data[index + 1..],
            None => &[],
        }
    } else {
        &data
    };
    String::from_utf8_lossy(data)
        .lines()
        .rev()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| row(&value))
        })
        .take(limit)
        .collect()
}

pub fn recent(limit: usize) -> Value {
    let directory = directory();
    if !directory.is_dir() {
        return json!({"ledger_available":false,"rows":[]});
    }
    let limit = limit.clamp(1, 200);
    let mut rows = Vec::new();
    let now = Local::now();
    for date in [now, now - Duration::days(1)] {
        rows.extend(read_day(
            directory.join(format!("requests-{}.jsonl", date.format("%Y%m%d"))),
            limit - rows.len(),
        ));
        if rows.len() >= limit {
            break;
        }
    }
    json!({"ledger_available":true,"rows":rows})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_pipe_is_refused_without_waiting_for_a_writer() {
        use std::{
            os::unix::fs::DirBuilderExt,
            sync::mpsc,
            time::{SystemTime, UNIX_EPOCH},
        };
        let directory = std::env::temp_dir().join(format!(
            "oxc-ledger-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("synthetic-fifo");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel();
        let read_path = path.clone();
        let worker = std::thread::spawn(move || sender.send(read_day(read_path, 1)).unwrap());
        let refused = receiver
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_ok_and(|rows| rows.is_empty());
        if !refused {
            // Unblock the pre-fix reader so the failing regression cleans up too.
            drop(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)
                    .unwrap(),
            );
        }
        worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(refused, "ledger reader blocked opening a FIFO");
    }
}
