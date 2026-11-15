//! Advisory prefix registry. Unknown-client text is never persisted without opt-in.
use crate::paths;
use nix::libc;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
const PRIMER_USER: &str = "Initialize this coding prefix. Reply READY. Do not call tools.";
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Unix clock")
        .as_secs_f64()
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn hex(text: &str, n: usize) -> bool {
    text.len() == n
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn learned(text: &str) -> bool {
    text.strip_prefix("learned-").is_some_and(|s| hex(s, 8))
}
fn clean_label(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " ._+-".contains(*c))
        .collect::<String>()
        .trim()
        .chars()
        .take(limit)
        .collect()
}
fn agent_label(agent: Option<&str>) -> String {
    clean_label(
        agent
            .unwrap_or("")
            .trim()
            .split('/')
            .next()
            .unwrap_or("")
            .split(' ')
            .next()
            .unwrap_or(""),
        24,
    )
}
pub fn client_label(payload: &Value, agent: Option<&str>) -> String {
    let product = agent_label(agent)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(32)
        .collect::<String>();
    if !product.is_empty() {
        product
    } else {
        parts(payload)
            .map(|(systems, tools, _)| learned_id(&systems, &tools))
            .unwrap_or_else(|| "other".into())
    }
}
type PrefixParts = (Vec<Value>, Vec<Value>, Map<String, Value>);
fn parts(payload: &Value) -> Option<PrefixParts> {
    let systems = payload["messages"]
        .as_array()?
        .iter()
        .take_while(|m| m.is_object() && [json!("system"), json!("developer")].contains(&m["role"]))
        .cloned()
        .collect::<Vec<_>>();
    if systems.is_empty() {
        return None;
    }
    let tools = payload["tools"].as_array().cloned().unwrap_or_default();
    let mut template = Map::new();
    for key in ["chat_template_kwargs", "parallel_tool_calls"] {
        if let Some(value) = payload.get(key) {
            template.insert(key.into(), value.clone());
        }
    }
    Some((systems, tools, template))
}
fn prefix_key(payload: &Value) -> Option<String> {
    let (systems, tools, template) = parts(payload)?;
    Some(hash(&json!([systems, tools, template]).to_string()))
}
fn system_text(systems: &[Value]) -> String {
    systems
        .iter()
        .map(|m| match &m["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
            _ => String::new(),
        })
        .collect::<Vec<String>>()
        .join("\n")
}
fn learned_id(systems: &[Value], tools: &[Value]) -> String {
    let head = system_text(systems)
        .trim_start()
        .chars()
        .take(256)
        .collect::<String>();
    let mut names = tools
        .iter()
        .filter(|t| t.is_object() && t["function"].is_object())
        .map(|t| {
            t["function"]["name"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if t["function"]["name"].is_null() {
                        "None".into()
                    } else {
                        t["function"]["name"].to_string()
                    }
                })
        })
        .collect::<Vec<_>>();
    names.sort();
    format!(
        "learned-{}",
        &hash(&paths::unicode_json(&json!([head, names])))[..8]
    )
}
pub fn settings(config: &Value) -> Value {
    let raw = &config["harness_learning"];
    let mut out = json!({"enabled":raw["enabled"]==true,"min_requests":3,"min_prefix_chars":3000});
    for (key, max) in [("min_requests", 100), ("min_prefix_chars", 1_000_000)] {
        let min = if key == "min_requests" { 1 } else { 0 };
        if let Some(n) = raw[key].as_u64().filter(|n| (min..=max).contains(n)) {
            out[key] = json!(n);
        }
    }
    out
}
pub fn identity(config: &Value) -> String {
    let field = |key: &str| {
        config
            .get(key)
            .map(|v| {
                v.as_str().map(str::to_owned).unwrap_or_else(|| {
                    if v.is_null() {
                        "None".into()
                    } else {
                        v.to_string()
                    }
                })
            })
            .unwrap_or_default()
    };
    if !config.is_object() {
        String::new()
    } else {
        format!(
            "{}:{}:{}",
            field("sha256"),
            field("server_sha256"),
            field("context_size")
        )
    }
}
pub struct Registry {
    directory: Option<PathBuf>,
    entries: Map<String, Value>,
    approved: BTreeSet<String>,
    ignored: BTreeSet<String>,
    labels: Map<String, Value>,
    candidates: Map<String, Value>,
    compat: Map<String, Value>,
}
impl Registry {
    pub fn new() -> Self {
        let directory = match std::env::var_os("OXC_HARNESS_STATE") {
            Some(value) if value.is_empty() => None,
            Some(value) => Some(PathBuf::from(value)),
            None => Some(paths::xdg("XDG_STATE_HOME", ".local/state").join("oxc/harness-prefixes")),
        };
        let mut registry = Self {
            directory,
            entries: Map::new(),
            approved: BTreeSet::new(),
            ignored: BTreeSet::new(),
            labels: Map::new(),
            candidates: Map::new(),
            compat: Map::new(),
        };
        registry.load();
        registry
    }
    fn load(&mut self) {
        let Some(directory) = &self.directory else {
            return;
        };
        let Some(value) = fs::read(directory.join("index.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        else {
            return;
        };
        if value["schema"] != 1 && value["schema"] != true {
            return;
        }
        let Some(entries) = value["entries"].as_object() else {
            return;
        };
        self.entries = entries
            .iter()
            .filter(|(k, v)| hex(k, 64) && v.is_object())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let discard = self
            .entries
            .iter()
            .filter(|(_, v)| v["harness"] == "other")
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in discard {
            self.drop_entry(&key);
        }
        self.approved = value["approved"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| learned(s))
            .map(str::to_owned)
            .collect();
        self.ignored = value["ignored"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| learned(s))
            .map(str::to_owned)
            .collect();
        if let Some(labels) = value["labels"].as_object() {
            for (key, value) in labels {
                let label = clean_label(value.as_str().unwrap_or(""), 40);
                if learned(key) && !label.is_empty() {
                    self.labels.insert(key.clone(), json!(label));
                }
            }
        }
    }
    fn write(path: &Path, value: &Value) -> io::Result<()> {
        let temporary = path.with_file_name(format!(
            "{}.pending",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(paths::unicode_json(value).as_bytes())?;
        drop(file);
        fs::rename(&temporary, path)
    }
    fn save(&self) {
        let Some(directory) = &self.directory else {
            return;
        };
        let result = (|| -> io::Result<()> {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory.join("primers"))?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            Self::write(
                &directory.join("index.json"),
                &json!({"schema":1,"entries":self.entries,"approved":self.approved,"ignored":self.ignored,"labels":self.labels}),
            )
        })();
        let _ = result;
    }
    fn drop_entry(&mut self, key: &str) {
        self.entries.shift_remove(key);
        if let Some(directory) = &self.directory {
            let _ = fs::remove_file(directory.join("primers").join(format!("{key}.json")));
        }
    }
    fn candidate(
        &mut self,
        name: &str,
        key: &str,
        sizes: &Value,
        agent: Option<&str>,
        time: f64,
    ) -> Value {
        let mut row=self.candidates.shift_remove(name).unwrap_or_else(||json!({"first_seen":time,"requests":0,"keys":{},"last_cache_n":null,"last_prompt_n":null}));
        let keys = row["keys"].as_object_mut().expect("candidate keys");
        let count = keys.shift_remove(key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        keys.insert(key.into(), json!(count));
        while keys.len() > 8 {
            let key = keys.keys().next().expect("candidate key").clone();
            keys.shift_remove(&key);
        }
        for (key, value) in sizes.as_object().expect("sizes") {
            row[key] = value.clone();
        }
        row["last_used"] = json!(time);
        row["requests"] = json!(row["requests"].as_u64().unwrap_or(0) + 1);
        row["last_key"] = json!(key);
        let agent = agent_label(agent);
        if !agent.is_empty() {
            row["agent"] = json!(agent);
        }
        self.candidates.insert(name.into(), row.clone());
        while self.candidates.len() > 16 {
            let key = self.candidates.keys().next().expect("candidate").clone();
            self.candidates.shift_remove(&key);
        }
        row
    }
    pub fn observe(
        &mut self,
        payload: &Value,
        config: &Value,
        learning: &Value,
        agent: Option<&str>,
    ) -> Option<String> {
        let key = prefix_key(payload)?;
        let (systems, tools, template) = parts(payload)?;
        let time = now();
        let system_chars = systems
            .iter()
            .map(|m| {
                m["content"]
                    .as_str()
                    .map(|s| s.chars().count())
                    .unwrap_or_else(|| paths::python_json(&m["content"]).chars().count())
            })
            .sum::<usize>();
        let sizes = json!({"system_chars":system_chars,"tools":tools.len(),"tools_chars":if tools.is_empty() {0} else {paths::unicode_json(&json!(tools)).chars().count()}});
        let mut entry = self.entries.get(&key).cloned();
        let fresh = entry.is_none();
        let harness = if let Some(entry) = &entry {
            entry.get("harness").cloned().unwrap_or(json!("other"))
        } else {
            let name = learned_id(&systems, &tools);
            let row = self.candidate(&name, &key, &sizes, agent, time);
            let known = self.approved.contains(&name)
                || self.entries.values().any(|e| e["harness"] == name);
            let needed = if known {
                1
            } else {
                learning["min_requests"].as_u64().unwrap_or(3)
            };
            let large = system_chars as u64 + sizes["tools_chars"].as_u64().unwrap_or(0)
                >= learning["min_prefix_chars"].as_u64().unwrap_or(0);
            if self.ignored.contains(&name)
                || !(known || learning["enabled"] == true)
                || !large
                || row["keys"][&key].as_u64().unwrap_or(0) < needed
            {
                return Some(key);
            }
            self.candidates.shift_remove(&name);
            self.approved.remove(&name);
            if let Some(agent) = row["agent"].as_str().filter(|s| !s.is_empty()) {
                self.labels.entry(name.clone()).or_insert(json!(agent));
            }
            entry = Some(
                json!({"first_seen":row["first_seen"],"requests":row["keys"][&key].as_u64().unwrap_or(1)-1,"warm_identity":null,"learned":true}),
            );
            json!(name)
        };
        let mut entry =
            entry.unwrap_or_else(|| json!({"first_seen":time,"requests":0,"warm_identity":null}));
        entry["harness"] = harness.clone();
        entry["model"] = config["model_alias"].clone();
        for (key, value) in sizes.as_object().expect("sizes") {
            entry[key] = value.clone();
        }
        entry["last_used"] = json!(time);
        entry["requests"] = json!(entry["requests"].as_u64().unwrap_or(0) + 1);
        self.entries.insert(key.clone(), entry);
        if fresh {
            if let Some(directory) = &self.directory {
                let mut messages = systems;
                messages.push(json!({"role":"user","content":PRIMER_USER}));
                let mut primer = json!({"messages":messages,"tools":tools});
                for (key, value) in template {
                    primer[&key] = value;
                }
                if paths::unicode_json(&primer).len() <= 2 * 1024 * 1024 {
                    let result = (|| -> io::Result<()> {
                        fs::DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(directory.join("primers"))?;
                        Self::write(
                            &directory.join("primers").join(format!("{key}.json")),
                            &primer,
                        )
                    })();
                    let _ = result;
                }
            }
        }
        if harness.as_str().is_some_and(|s| s.starts_with("learned-")) {
            let mut own = self
                .entries
                .iter()
                .filter(|(_, e)| e["harness"] == harness)
                .map(|(k, e)| (k.clone(), e["last_used"].as_f64().unwrap_or(0.0)))
                .collect::<Vec<_>>();
            own.sort_by(|a, b| a.1.total_cmp(&b.1));
            let count = own.len().saturating_sub(4);
            for (key, _) in own.into_iter().take(count) {
                self.drop_entry(&key);
            }
        }
        while self.entries.len() > 24 {
            let key = self
                .entries
                .iter()
                .min_by(|(_, a), (_, b)| {
                    a["last_used"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .total_cmp(&b["last_used"].as_f64().unwrap_or(0.0))
                })
                .map(|(k, _)| k.clone())
                .expect("registry entry");
            self.drop_entry(&key);
        }
        self.save();
        Some(key)
    }
    pub fn complete(&mut self, key: &str, config: &Value, cache: Value, prompt: Value) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry["warm_identity"] = json!(identity(config));
            entry["last_cache_n"] = cache;
            entry["last_prompt_n"] = prompt;
            entry["last_ok"] = json!(now());
            self.save();
        } else {
            for row in self.candidates.values_mut() {
                if row["last_key"] == key {
                    row["last_cache_n"] = cache.clone();
                    row["last_prompt_n"] = prompt.clone();
                }
            }
        }
    }
    pub fn cleared(&mut self) {
        for entry in self.entries.values_mut() {
            entry["warm_identity"] = Value::Null;
        }
        self.save();
    }
    pub fn record_compat(&mut self, notes: &[&str]) {
        for note in notes {
            let count = self.compat.get(*note).and_then(Value::as_u64).unwrap_or(0) + 1;
            self.compat.insert((*note).into(), json!(count));
        }
    }
    pub fn forget(&mut self, harness: Option<&str>, ignore: bool) {
        let keys = self
            .entries
            .iter()
            .filter(|(_, e)| harness.is_none_or(|h| e["harness"] == h))
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.drop_entry(&key);
        }
        if let Some(harness) = harness {
            self.candidates.shift_remove(harness);
            self.approved.remove(harness);
            self.labels.shift_remove(harness);
            if ignore && learned(harness) {
                self.ignored.insert(harness.into());
            }
        } else {
            self.candidates.clear();
            self.approved.clear();
            self.labels.clear();
            if !ignore {
                self.ignored.clear();
            }
        }
        self.save();
    }
    pub fn approve(&mut self, harness: &str) -> bool {
        if !learned(harness) {
            return false;
        }
        if !self.candidates.contains_key(harness) && !self.ignored.contains(harness) {
            return self.entries.values().any(|e| e["harness"] == harness);
        }
        self.ignored.remove(harness);
        self.approved.insert(harness.into());
        self.save();
        true
    }
    pub fn label(&mut self, harness: &str, label: &str) -> bool {
        let label = clean_label(label, 40);
        if label.is_empty()
            || !harness.starts_with("learned-")
            || !self.entries.values().any(|e| e["harness"] == harness)
        {
            return false;
        }
        self.labels.insert(harness.into(), json!(label));
        self.save();
        true
    }
    pub fn names(&self, config: &Value) -> Vec<String> {
        let mut newest = Map::<String, Value>::new();
        for entry in self.entries.values() {
            let name = entry["harness"].as_str().unwrap_or("other");
            if newest.get(name).is_none_or(|e| {
                entry["last_used"].as_f64().unwrap_or(0.0) > e["last_used"].as_f64().unwrap_or(0.0)
            }) {
                newest.insert(name.into(), entry.clone());
            }
        }
        let mut newest = newest.into_iter().collect::<Vec<_>>();
        newest.sort_by(|(_, a), (_, b)| {
            b["last_used"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["last_used"].as_f64().unwrap_or(0.0))
        });
        let identity = identity(config);
        newest
            .into_iter()
            .filter(|(_, e)| e["warm_identity"] != identity || identity.is_empty())
            .map(|(name, _)| name)
            .collect()
    }
    pub fn primers(&self, harness: &str, alias: &str) -> Vec<(String, Value)> {
        let Some(directory) = &self.directory else {
            return Vec::new();
        };
        let mut entries = self
            .entries
            .iter()
            .filter(|(_, e)| e["harness"] == harness)
            .collect::<Vec<_>>();
        entries.sort_by(|(_, a), (_, b)| {
            b["last_used"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["last_used"].as_f64().unwrap_or(0.0))
        });
        entries
            .into_iter()
            .take(3)
            .filter_map(|(key, _)| {
                let mut body = fs::read(directory.join("primers").join(format!("{key}.json")))
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())?;
                if prefix_key(&body).as_ref() != Some(key) {
                    return None;
                }
                for (key, value) in [
                    ("model", json!(alias)),
                    ("max_tokens", json!(1)),
                    ("temperature", json!(0)),
                    ("stream", json!(false)),
                    ("cache_prompt", json!(true)),
                    ("tool_choice", json!("none")),
                ] {
                    body[key] = value;
                }
                Some((key.clone(), body))
            })
            .collect()
    }
    pub fn status(&self, config: &Value, runtime: &Value) -> Value {
        let identity = if config.as_object().is_some_and(|m| !m.is_empty()) {
            Some(identity(config))
        } else {
            None
        };
        let mut harnesses = Map::<String, Value>::new();
        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_by(|(_, a), (_, b)| {
            b["last_used"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["last_used"].as_f64().unwrap_or(0.0))
        });
        for (key, entry) in entries {
            let name = entry["harness"].as_str().unwrap_or("other");
            let row = harnesses
                .entry(name)
                .or_insert_with(|| json!({"prefixes":[],"warm":0,"last_used":0}));
            if name.starts_with("learned-") {
                row["learned"] = json!(true);
                row["label"] = self.labels.get(name).cloned().unwrap_or(json!(""));
            }
            let warm = identity
                .as_ref()
                .is_some_and(|s| entry["warm_identity"] == *s);
            row["prefixes"].as_array_mut().expect("prefix list").push(json!({"key":&key[..12],"model":entry["model"],"warm":warm,"system_chars":entry["system_chars"],"tools":entry["tools"],"requests":entry["requests"],"last_used":entry["last_used"],"last_cache_n":entry["last_cache_n"],"last_prompt_n":entry["last_prompt_n"]}));
            row["warm"] = json!(row["warm"].as_u64().unwrap_or(0) + u64::from(warm));
            if entry["last_used"].as_f64().unwrap_or(0.0) > row["last_used"].as_f64().unwrap_or(0.0)
            {
                row["last_used"] = entry["last_used"].clone();
            }
        }
        let mut learning = settings(runtime);
        let mut candidates = Vec::new();
        for (name, row) in self.candidates.iter().rev() {
            let best = row["keys"]
                .as_object()
                .into_iter()
                .flat_map(|keys| keys.values())
                .filter_map(Value::as_u64)
                .max()
                .unwrap_or(0);
            let size = row["system_chars"].as_u64().unwrap_or(0)
                + row["tools_chars"].as_u64().unwrap_or(0);
            candidates.push(json!({"harness":name,"agent":row.get("agent").cloned().unwrap_or(json!("")),"requests":row["requests"],"distinct_prefixes":row["keys"].as_object().map(Map::len).unwrap_or(0),"consistent_requests":best,"stable":best>=2 || row["requests"]==1,"system_chars":row["system_chars"],"tools":row["tools"],"prefix_chars":size,"large_enough":size>=learning["min_prefix_chars"].as_u64().unwrap_or(3000),"approved":self.approved.contains(name),"ignored":self.ignored.contains(name),"first_seen":row["first_seen"],"last_used":row["last_used"],"last_cache_n":row["last_cache_n"],"last_prompt_n":row["last_prompt_n"]}));
        }
        learning["persistent"] = json!(self.directory.is_some());
        learning["candidates"] = json!(candidates);
        learning["ignored"] = json!(self.ignored);
        json!({"harnesses":harnesses,"request_compat":self.compat,"harness_learning":learning})
    }
}
