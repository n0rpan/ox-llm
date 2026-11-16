//! Raw and decoded-field redaction. Keep one Stream per independent SSE field;
//! transport framing and JSON parsing happen before decoded-field redaction.
use crate::secrets::Key;
use serde_json::Value;

/// Redact the exact held key and report whether it occurred, including when
/// the key itself equals the replacement marker. Visits every string and key.
pub fn value(value: &mut Value, key: &Key) -> bool {
    match value {
        Value::String(text) => {
            let found = text.contains(key.expose());
            *text = text.replace(key.expose(), "[redacted]");
            found
        }
        Value::Array(items) => {
            let mut found = false;
            for item in items {
                found |= self::value(item, key);
            }
            found
        }
        Value::Object(map) => {
            let old = std::mem::take(map);
            let mut found = false;
            for (name, mut item) in old {
                found |= name.contains(key.expose());
                found |= self::value(&mut item, key);
                map.insert(name.replace(key.expose(), "[redacted]"), item);
            }
            found
        }
        _ => false,
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
