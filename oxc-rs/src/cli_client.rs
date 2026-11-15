//! Bounded JSON control requests and CLI notifications.
use serde_json::{json, Value};
use std::process::Stdio;

fn http_client(seconds: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(seconds))
        .read_timeout(std::time::Duration::from_secs(seconds))
        .build()
        .expect("HTTP client")
}
pub(crate) async fn request(path: &str, body: Option<Value>) -> (u16, Value) {
    request_timeout(path, body, 15).await
}
pub(crate) async fn request_timeout(path: &str, body: Option<Value>, seconds: u64) -> (u16, Value) {
    let url = format!("http://127.0.0.1:8092{path}");
    request_url(&url, body, seconds).await
}
pub(crate) async fn request_url(url: &str, body: Option<Value>, seconds: u64) -> (u16, Value) {
    let client = http_client(seconds);
    let request = if let Some(body) = body {
        client.post(url).json(&body)
    } else {
        client.get(url)
    };
    match request.send().await {
        Ok(mut reply) => {
            let code = reply.status().as_u16();
            let mut bytes = Vec::new();
            loop {
                match reply.chunk().await {
                    Ok(Some(chunk)) => {
                        bytes.extend_from_slice(&chunk[..chunk.len().min(65537 - bytes.len())]);
                        if bytes.len() == 65537 {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return (if code >= 400 { code } else { 0 }, json!({})),
                }
            }
            if code < 400 && bytes.len() > 65536 {
                return (0, json!({}));
            }
            bytes.truncate(65536);
            let value = decode_object(&bytes);
            (
                if code < 400 && value.is_none() {
                    0
                } else {
                    code
                },
                value.unwrap_or_else(|| json!({})),
            )
        }
        Err(_) => (0, json!({})),
    }
}
fn decode_object(bytes: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        return value.is_object().then_some(value);
    }
    // Python's legacy CLI accepts these JSON constants. They are not used in
    // its bounded numeric projections or error strings; retain the object and
    // its valid fields without introducing a Python runtime dependency.
    let mut normalized = Vec::with_capacity(bytes.len());
    let (mut index, mut quoted, mut escaped) = (0, false, false);
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            normalized.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
        } else if byte == b'"' {
            quoted = true;
            normalized.push(byte);
            index += 1;
        } else if let Some(token) = [
            b"NaN".as_slice(),
            b"Infinity".as_slice(),
            b"-Infinity".as_slice(),
        ]
        .iter()
        .find(|token| bytes[index..].starts_with(token))
        {
            normalized.extend_from_slice(b"null");
            index += token.len();
        } else {
            normalized.push(byte);
            index += 1;
        }
    }
    serde_json::from_slice::<Value>(&normalized)
        .ok()
        .filter(Value::is_object)
}
pub(crate) async fn command(name: &str, args: &[&str]) -> Option<i32> {
    tokio::process::Command::new(name)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .ok()
        .map(|s| s.code().unwrap_or(1))
}
pub(crate) async fn notify(message: &str) {
    if command("notify-send", &["Local AI", message])
        .await
        .is_none()
    {
        eprintln!("local-ai: {message}");
    }
}
pub(crate) fn reason(body: &Value, fallback: &str) -> String {
    body.get("error")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("message").and_then(Value::as_str))
        })
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().chars().take(300).collect())
        .unwrap_or_else(|| fallback.to_owned())
}
pub(crate) fn plain_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub(crate) fn storage_id(value: &str) -> bool {
    value.rsplit_once('-').is_some_and(|(kind, hash)| {
        hash.len() == 16
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && kind
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase()))
    })
}
