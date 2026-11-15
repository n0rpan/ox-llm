//! Private, bounded opaque thinking state for exact-prefix tool continuations.
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
    pub(crate) fn new(model: &Value, credential: &str, payload: &Value) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"oxc-anthropic-replay-v1\0");
        for field in ["provider", "id", "upstream_model", "base_url"] {
            add(&mut digest, &model[field]);
        }
        // Keep only a digest of the credential in the scope, never its bytes.
        digest.update(Sha256::digest(credential.as_bytes()));
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
        let size = serde_json::to_vec(blocks).map_or(usize::MAX, |bytes| bytes.len());
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
    const KEY: &str = "synthetic-private-key";
    fn model() -> Value {
        json!({"provider":"anthropic","id":"synthetic-cloud",
            "upstream_model":"synthetic-anthropic","base_url":"https://example.invalid/v1"})
    }
    fn blocks() -> Vec<Value> {
        vec![
            json!({"type":"thinking","thinking":"synthetic plan","signature":"synthetic-signature"}),
            json!({"type":"redacted_thinking","data":"synthetic-opaque-data"}),
        ]
    }
    fn history() -> Value {
        json!({"messages":[{"role":"user","content":"synthetic question"}]})
    }
    #[test]
    fn exact_prefix_replay_is_bound_to_provider_model_endpoint_and_credential() {
        let model = model();
        let mut payload = history();
        let assistant = json!({"role":"assistant","content":"answer"});
        let id = History::new(&model, KEY, &payload).after(&assistant);
        let mut replay = Replay::default();
        replay.remember(id, &blocks());
        payload["messages"].as_array_mut().unwrap().push(assistant);
        let found = History::new(&model, KEY, &payload).at(1).unwrap();
        assert_eq!(replay.restore(found), Some(blocks()));
        for (field, changed) in [
            ("provider", "other-provider"),
            ("id", "other-alias"),
            ("upstream_model", "other-upstream"),
            ("base_url", "https://example.invalid/other"),
        ] {
            let mut different = model.clone();
            different[field] = json!(changed);
            assert!(replay
                .restore(History::new(&different, KEY, &payload).at(1).unwrap())
                .is_none());
        }
        assert!(replay
            .restore(
                History::new(&model, "rotated-synthetic-key", &payload)
                    .at(1)
                    .unwrap()
            )
            .is_none());
        payload["messages"][0]["content"] = json!("different question");
        assert!(replay
            .restore(History::new(&model, KEY, &payload).at(1).unwrap())
            .is_none());
    }
    #[test]
    fn argument_order_and_reasoning_are_normalized_but_tool_result_ids_are_significant() {
        let mut first = history();
        let assistant = json!({"role":"assistant","content":null,"reasoning_content":"synthetic plan",
            "tool_calls":[{"id":"tool-1","type":"function","function":{"name":"read",
            "arguments":"{\"outer\":{\"a\":1,\"b\":2}}"}}]});
        let id = History::new(&model(), KEY, &first).after(&assistant);
        first["messages"].as_array_mut().unwrap().push(assistant);
        first["messages"][1]["reasoning_content"] = Value::Null;
        first["messages"][1]["tool_calls"][0]["function"]["arguments"] =
            json!("{ \"outer\" : {\"b\":2,\"a\":1} }");
        assert_eq!(History::new(&model(), KEY, &first).at(1), Some(id));
        first["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"tool","tool_call_id":"tool-1","content":"result"}));
        let original = History::new(&model(), KEY, &first).at(2);
        first["messages"][2]["tool_call_id"] = json!("tool-2");
        assert_ne!(History::new(&model(), KEY, &first).at(2), original);
    }
    #[test]
    fn lru_and_byte_limits_evict_opaque_state_without_exposing_it() {
        let blocks = blocks();
        let size = serde_json::to_vec(&blocks).unwrap().len();
        let mut replay = Replay {
            max_entries: 2,
            max_bytes: size * 2,
            ..Replay::default()
        };
        replay.remember([1; 32], &blocks);
        replay.remember([2; 32], &blocks);
        assert_eq!(replay.restore([1; 32]), Some(blocks.clone()));
        replay.remember([3; 32], &blocks);
        assert!(replay.restore([2; 32]).is_none());
        assert_eq!(replay.restore([1; 32]), Some(blocks.clone()));
        replay.max_bytes = size;
        replay.remember([4; 32], &blocks);
        assert!(replay.restore([1; 32]).is_none());
        assert!(replay.restore([3; 32]).is_none());
        assert_eq!(replay.bytes, size);
        let mut modified = replay.restore([4; 32]).unwrap();
        modified[0]["signature"] = json!("changed");
        assert_eq!(replay.restore([4; 32]), Some(blocks));
    }
}
