//! Raw and decoded-field redaction. Keep one Stream per independent SSE field;
//! transport framing and JSON parsing happen before decoded-field redaction.
use crate::secrets::Key;
use serde_json::Value;

pub fn value(value: &mut Value, key: &Key) {
    secret(value, key.expose());
}
pub(crate) fn contains_secret(value: &Value, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(items) => items.iter().any(|item| contains_secret(item, secret)),
        Value::Object(map) => map
            .iter()
            .any(|(key, item)| key.contains(secret) || contains_secret(item, secret)),
        _ => false,
    }
}
pub(crate) fn secret(value: &mut Value, secret: &str) {
    if secret.is_empty() {
        return;
    }
    match value {
        Value::String(text) => *text = text.replace(secret, "[redacted]"),
        Value::Array(items) => items.iter_mut().for_each(|item| self::secret(item, secret)),
        Value::Object(map) => {
            let old = std::mem::take(map);
            for (name, mut item) in old {
                self::secret(&mut item, secret);
                map.insert(name.replace(secret, "[redacted]"), item);
            }
        }
        _ => {}
    }
}

pub struct Stream {
    key: Vec<u8>,
    pending: Vec<u8>,
}
impl Stream {
    pub fn new(key: &Key) -> Self {
        Self {
            key: key.expose().as_bytes().to_vec(),
            pending: Vec::new(),
        }
    }
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let mut safe = Vec::with_capacity(data.len());
        let mut index = 0;
        while index < data.len() {
            if data[index..].starts_with(&self.key) {
                safe.extend_from_slice(b"[redacted]");
                index += self.key.len();
            } else {
                safe.push(data[index]);
                index += 1;
            }
        }
        let hold = (1..self.key.len().min(safe.len() + 1))
            .rev()
            .find(|size| safe.ends_with(&self.key[..*size]))
            .unwrap_or(0);
        self.pending = safe.split_off(safe.len() - hold);
        safe
    }
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}
