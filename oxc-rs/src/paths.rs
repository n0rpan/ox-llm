//! CLI home resolution and Python-compatible public JSON encoding.
use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::Uid::current())
                .ok()
                .flatten()
                .map(|user| user.dir)
        })
        .expect("Could not determine home directory.")
}

pub fn python_json(value: &serde_json::Value) -> String {
    format_json(value, true, true)
}
fn ascii_json(raw: &str) -> String {
    let mut result = String::new();
    for ch in raw.chars() {
        if (ch as u32) < 0x7f {
            result.push(ch);
        } else {
            let mut units = [0_u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                result.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    result
}
fn format_json(value: &serde_json::Value, ascii: bool, spaced: bool) -> String {
    use serde_json::Value;
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| format_json(value, ascii, spaced))
                .collect::<Vec<_>>()
                .join(if spaced { ", " } else { "," })
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}{}",
                    format_json(&Value::String(key.clone()), ascii, spaced),
                    if spaced { " " } else { "" },
                    format_json(value, ascii, spaced)
                ))
                .collect::<Vec<_>>()
                .join(if spaced { ", " } else { "," })
        ),
        Value::String(_) => {
            let raw = value.to_string();
            if ascii {
                ascii_json(&raw)
            } else {
                raw
            }
        }
        _ => value.to_string(),
    }
}
