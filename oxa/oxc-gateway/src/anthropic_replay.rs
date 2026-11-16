//! Private, bounded opaque thinking state for exact-prefix tool continuations.
use crate::catalog::Model;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};

type Id = [u8; 32];

fn canonical(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        value => value,
    }
}
fn message(value: &Value) -> Value {
    if !value.is_object() {
        return Value::Null;
    }
    let content = match value["content"].as_array() {
        Some(parts) => json!(parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<String>()),
        None => value["content"].clone(),
    };
    let calls: Vec<_> = value["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|call| call.is_object())
        .map(|call| {
            let arguments = &call["function"]["arguments"];
            let arguments = arguments
                .as_str()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .unwrap_or_else(|| arguments.clone());
            json!({"id":call["id"],"type":call["type"],
                "name":call["function"]["name"],"arguments":arguments})
        })
        .collect();
    // The OpenAI client may omit/reformat reasoning or tool argument JSON.
    // Tool result IDs remain significant, including when results are swapped.
    canonical(json!({"role":value["role"],"content":content,"tool_calls":calls,
        "tool_call_id":value["tool_call_id"]}))
}
fn add(digest: &mut Sha256, value: &Value) {
    digest.update(value.to_string().as_bytes());
    digest.update([0]);
}

pub(crate) struct History {
    prefixes: Vec<Id>,
    end: Sha256,
}
impl History {
    pub(crate) fn new(model: &Model, credential: &str, payload: &Value) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"oxc-anthropic-replay-v1\0");
        for field in [
            model.info().provider.as_str(),
            model.info().id,
            model.upstream(),
            model.endpoint().as_str(),
        ] {
            digest.update(field.len().to_le_bytes());
            digest.update(field.as_bytes());
        }
        // Keep only a digest of the credential in the scope, never its bytes.
        digest.update(Sha256::digest(credential.as_bytes()));
        add(&mut digest, &canonical(payload["tools"].clone()));
        let prefixes = payload["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|value| {
                add(&mut digest, &message(value));
                digest.clone().finalize().into()
            })
            .collect();
        Self {
            prefixes,
            end: digest,
        }
    }
    pub(crate) fn at(&self, index: usize) -> Option<Id> {
        self.prefixes.get(index).copied()
    }
    pub(crate) fn after(&self, assistant: &Value) -> Id {
        let mut digest = self.end.clone();
        add(&mut digest, &message(assistant));
        digest.finalize().into()
    }
}

struct Entry {
    id: Id,
    blocks: Vec<Value>,
    size: usize,
}
struct Count(usize);
impl std::io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) struct Replay {
    entries: VecDeque<Entry>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}
impl Default for Replay {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries: 4096,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}
impl Replay {
    pub(crate) fn restore(&mut self, id: Id) -> Option<Vec<Value>> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        let entry = self.entries.remove(index)?;
        let blocks = entry.blocks.clone();
        self.entries.push_back(entry);
        Some(blocks)
    }
    pub(crate) fn remember(&mut self, id: Id, blocks: &[Value]) {
        if !blocks
            .iter()
            .any(|block| matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking")))
        {
            return;
        }
        let mut count = Count(0);
        if serde_json::to_writer(&mut count, blocks).is_err() {
            return;
        }
        let size = count.0;
        if let Some(index) = self.entries.iter().position(|entry| entry.id == id) {
            if let Some(old) = self.entries.remove(index) {
                self.bytes -= old.size;
            }
        }
        if size > self.max_bytes || self.max_entries == 0 {
            return;
        }
        self.entries.push_back(Entry {
            id,
            blocks: blocks.to_vec(),
            size,
        });
        self.bytes += size;
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= old.size;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Provider;
    fn model(alias: &str, upstream: &str, endpoint: &str) -> Model {
        Model::new(alias, Provider::Anthropic, upstream, endpoint, 1024, 16).unwrap()
    }
    fn blocks() -> Vec<Value> {
        vec![json!({"type":"thinking","thinking":"Synthetic plan","signature":"opaque-signature"})]
    }
    #[test]
    fn replay_never_crosses_history_model_endpoint_or_credential_boundaries() {
        let route = model("alias", "synthetic", "https://example.invalid");
        let first = json!({"messages":[{"role":"user","content":"Synthetic"}]});
        let assistant = json!({"role":"assistant","content":null,"tool_calls":[
            {"id":"tool-1","type":"function","function":{"name":"read","arguments":"{\"a\":1,\"b\":2}"}}]});
        let id = History::new(&route, "synthetic-key", &first).after(&assistant);
        let mut replay = Replay::default();
        replay.remember(id, &blocks());
        let mut continued = first.clone();
        continued["messages"].as_array_mut().unwrap().push(assistant);
        continued["messages"][1]["tool_calls"][0]["function"]["arguments"] = json!("{ \"b\":2,\"a\":1 }");
        assert_eq!(
            replay.restore(History::new(&route, "synthetic-key", &continued).at(1).unwrap()),
            Some(blocks())
        );
        assert!(replay
            .restore(History::new(&route, "rotated-key", &continued).at(1).unwrap())
            .is_none());
        for changed in [
            model("other", "synthetic", "https://example.invalid"),
            model("alias", "other", "https://example.invalid"),
            model("alias", "synthetic", "https://example.invalid/other"),
        ] {
            assert!(replay
                .restore(History::new(&changed, "synthetic-key", &continued).at(1).unwrap())
                .is_none());
        }
        continued["messages"][0]["content"] = json!("Different");
        assert!(replay
            .restore(History::new(&route, "synthetic-key", &continued).at(1).unwrap())
            .is_none());
    }
    #[test]
    fn opaque_replay_has_hard_entry_and_byte_bounds() {
        let blocks = blocks();
        let size = serde_json::to_vec(&blocks).unwrap().len();
        let mut replay = Replay {
            max_entries: 2,
            max_bytes: 2 * size,
            ..Replay::default()
        };
        replay.remember([1; 32], &blocks);
        replay.remember([2; 32], &blocks);
        assert_eq!(replay.restore([1; 32]), Some(blocks.clone()));
        replay.remember([3; 32], &blocks);
        assert!(replay.restore([2; 32]).is_none());
        replay.max_bytes = size - 1;
        replay.remember([4; 32], &blocks);
        assert!(replay.restore([4; 32]).is_none());
    }
}
