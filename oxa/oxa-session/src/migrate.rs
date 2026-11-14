//! B14: import of pi session files (v1, v2, v3). Owner: task B14. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/session-manager.ts (migrateV1ToV2, migrateV2ToV3) and src/migrations.ts
use crate::{decode_entry, decode_header, new_entry_id, SessionHeader, SessionRef, SessionStore};
use oxa_proto::{codes, names, Entry, OxaError};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::Path;

/// Convert the lines of a pi session file of any version into an oxa header + entries.
/// pi assistant messages have no exact wire bytes: `wire` is rebuilt from pi's content blocks
/// (the cache starts cold, as documented for `/import`). Entry types oxa does not know are
/// kept as `Custom { custom_type: "pi:<type>" }`. A file without a `Prefix` entry gets none;
/// the importer's caller opens a new epoch.
pub fn migrate_lines(lines: &[&str]) -> Result<(SessionHeader, Vec<Entry>), OxaError> {
    // Blank and malformed lines are skipped, as pi's parseSessionEntries does. The position in
    // this vector is pi's entry index (`firstKeptEntryIndex` of v1 counts the header too).
    let parsed: Vec<Value> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(Value::is_object)
        .collect();
    let header_val = parsed
        .first()
        .filter(|v| entry_type(v) == "session")
        .ok_or_else(|| invalid("pi session file has no session header line"))?;
    let version = match header_val.get("version") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| invalid("session header version is not a number"))?,
    };
    if version > u64::from(names::SESSION_VERSION) {
        return Err(invalid(format!(
            "session version {version} is newer than this oxa reads (version {})",
            names::SESSION_VERSION
        ))
        .with_hint("open the session with the newer oxa or pi that wrote it"));
    }
    let header_ts = header_val
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("session header has no timestamp"))?
        .to_string();
    let header = decode_header(&header_bytes(header_val, version)?)?;
    let header = SessionHeader {
        version: names::SESSION_VERSION,
        ..header
    };

    // Ids: v1 has none (pi generates them in migrateV1ToV2, in file order); v2 and v3 keep theirs.
    let mut ids: Vec<String> = vec![String::new(); parsed.len()];
    let mut taken: HashSet<String> = HashSet::new();
    if version >= 2 {
        for v in parsed.iter().skip(1) {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                taken.insert(id.to_string());
            }
        }
    }
    for (i, v) in parsed.iter().enumerate().skip(1) {
        if entry_type(v) == "session" {
            continue;
        }
        let existing = if version >= 2 {
            v.get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        } else {
            None
        };
        ids[i] = match existing {
            Some(id) => id,
            None => {
                let id = new_entry_id(&|x| taken.contains(x));
                taken.insert(id.clone());
                id
            }
        };
    }

    let mut out = Vec::new();
    let mut prev: Option<String> = None;
    for (i, v) in parsed.iter().enumerate().skip(1) {
        let ty = entry_type(v);
        if ty == "session" {
            continue;
        }
        let id = ids[i].clone();
        // v1 has a single chain in file order (migrateV1ToV2); v2 and v3 keep `parentId`.
        let parent = if version < 2 {
            prev.clone()
        } else {
            v.get("parentId").and_then(Value::as_str).map(String::from)
        };
        prev = Some(id.clone());
        // A v1 compaction names its first kept entry by file index; the index may point at the header.
        let kept = v
            .get("firstKeptEntryIndex")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| ids.get(n))
            .filter(|s| !s.is_empty())
            .cloned();
        out.push(to_entry(v, ty, id, parent, &header_ts, kept)?);
    }
    Ok((header, out))
}

/// `/import <path>`: read a pi (or oxa) session file and write it as a new session for `cwd`.
pub fn import_pi(path: &Path, store: &dyn SessionStore, cwd: &Path) -> Result<SessionRef, OxaError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e))?;
    let lines: Vec<&str> = text.lines().collect();
    let (_header, entries) = migrate_lines(&lines)?;
    // The new session points at the file it was imported from (pi's parentSession).
    let source = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dst = store.create(cwd, None, Some(source.as_path()))?;
    let written = entries.iter().try_for_each(|e| store.append(&dst, e));
    if let Err(e) = written {
        // Never leave a half-imported session behind.
        let _ = store.delete(&dst);
        return Err(e);
    }
    Ok(dst)
}

/// pi `/import <path>` (slash-commands.md: "Import a JSONL session, preserving its ID and
/// working directory"): the file is migrated (pi v1-v3, or an oxa file as is) and stored with
/// the header's id and cwd (`fallback_cwd` when the header names none). A file that is already
/// in the store under its id is opened as it is (pi: the source is already stored). When
/// another stored session holds that id, or the id is not one oxa accepts, the copy gets a new
/// id (the store keeps ids unique; pi would keep two files with one id).
pub fn import_session(
    path: &Path,
    store: &dyn SessionStore,
    fallback_cwd: &Path,
) -> Result<SessionRef, OxaError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e))?;
    let lines: Vec<&str> = text.lines().collect();
    let (header, entries) = migrate_lines(&lines)?;
    let source = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let stored = store.list(None).into_iter().find(|m| m.sref.id == header.id);
    if let Some(m) = &stored {
        let same = std::fs::canonicalize(&m.sref.path)
            .map(|p| p == source)
            .unwrap_or(false);
        if same {
            return Ok(m.sref.clone());
        }
    }
    let cwd = if header.cwd.as_os_str().is_empty() {
        fallback_cwd.to_path_buf()
    } else {
        header.cwd.clone()
    };
    let keep_id = (stored.is_none() && !header.id.is_empty()).then_some(header.id.as_str());
    let dst = match store.create(&cwd, keep_id, Some(source.as_path())) {
        Ok(dst) => dst,
        Err(_) if keep_id.is_some() => store.create(&cwd, None, Some(source.as_path()))?,
        Err(e) => return Err(e),
    };
    let written = entries.iter().try_for_each(|e| store.append(&dst, e));
    if let Err(e) = written {
        let _ = store.delete(&dst);
        return Err(e);
    }
    Ok(dst)
}

fn invalid(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, message)
}

fn entry_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The header as `decode_header` reads it: v1 headers have no `version`, and pi's older
/// `branchedFrom` is the parent of the same meaning as `parentSession`.
fn header_bytes(v: &Value, version: u64) -> Result<Vec<u8>, OxaError> {
    let mut m = Map::new();
    m.insert("type".into(), json!("session"));
    m.insert("version".into(), json!(version));
    for key in ["id", "timestamp", "cwd"] {
        m.insert(key.into(), v.get(key).cloned().unwrap_or(Value::Null));
    }
    let parent = v
        .get("parentSession")
        .or_else(|| v.get("branchedFrom"))
        .filter(|p| p.is_string())
        .cloned();
    if let Some(p) = parent {
        m.insert("parentSession".into(), p);
    }
    serde_json::to_vec(&Value::Object(m)).map_err(|e| invalid(format!("header: {e}")))
}

/// One pi entry as an oxa entry. Known shapes are normalised and decoded by `decode_entry`; a
/// shape that does not decode is kept as `custom` with `customType` `pi:<type>` (never dropped).
fn to_entry(
    v: &Value,
    ty: &str,
    id: String,
    parent: Option<String>,
    header_ts: &str,
    kept: Option<String>,
) -> Result<Entry, OxaError> {
    let obj = v.as_object().ok_or_else(|| invalid("entry is not an object"))?;
    let stamp = obj.get("timestamp").and_then(Value::as_str).unwrap_or(header_ts);
    let (new_ty, fields) = normalise(ty, obj, kept.as_deref());
    for s in [stamp, header_ts] {
        if let Ok(e) = decode_entry(&envelope(&new_ty, s, &id, parent.as_deref(), fields.clone())) {
            return Ok(e);
        }
    }
    let mut data = Map::new();
    data.insert("customType".into(), json!(format!("pi:{ty}")));
    data.insert("data".into(), Value::Object(obj.clone()));
    decode_entry(&envelope("custom", header_ts, &id, parent.as_deref(), data))
}

/// The envelope (`type`, `id`, `parentId`, `timestamp`) plus the body fields, as one line.
fn envelope(ty: &str, ts: &str, id: &str, parent: Option<&str>, fields: Map<String, Value>) -> Vec<u8> {
    let mut m = Map::new();
    m.insert("type".into(), json!(ty));
    m.insert("id".into(), json!(id));
    m.insert("parentId".into(), parent.map_or(Value::Null, |p| json!(p)));
    m.insert("timestamp".into(), json!(ts));
    m.extend(fields);
    serde_json::to_vec(&Value::Object(m)).unwrap_or_default()
}

/// The oxa type and body fields of a pi entry. Missing optional fields get pi's defaults. A type
/// without a normalisation keeps its own fields; the decoder accepts or rejects them.
fn normalise(ty: &str, obj: &Map<String, Value>, kept: Option<&str>) -> (String, Map<String, Value>) {
    let mut f = Map::new();
    match ty {
        "message" => {
            let role = obj
                .get("message")
                .and_then(Value::as_object)
                .and_then(|m| m.get("role").and_then(Value::as_str).map(|r| (r, m)));
            match role {
                // An oxa entry already carries its exact `wire` (W2): keep it as it is.
                Some((r @ ("user" | "assistant" | "toolResult"), msg)) if !obj.contains_key("wire") => {
                    f.insert("message".into(), Value::Object(msg.clone()));
                    f.insert("wire".into(), synth_wire(r, msg));
                    return ("message".into(), f);
                }
                // v2 wrote `hookMessage`; v2 to v3 renamed it to `custom` (migrateV2ToV3).
                Some(("custom" | "hookMessage", msg)) => {
                    f.insert("customType".into(), value_or(msg.get("customType"), json!("")));
                    f.insert("content".into(), json!(content_of(msg.get("content")).0));
                    f.insert("display".into(), json!(flag(msg.get("display"))));
                    f.insert("details".into(), value_or(msg.get("details"), Value::Null));
                    return ("custom_message".into(), f);
                }
                _ => passthrough(obj, &mut f),
            }
        }
        "compaction" => {
            let first = obj
                .get("firstKeptEntryId")
                .and_then(Value::as_str)
                .or(kept)
                .unwrap_or("");
            f.insert("summary".into(), value_or(obj.get("summary"), json!("")));
            f.insert("firstKeptEntryId".into(), json!(first));
            f.insert(
                "tokensBefore".into(),
                json!(obj.get("tokensBefore").and_then(Value::as_u64).unwrap_or(0)),
            );
            f.insert("details".into(), value_or(obj.get("details"), Value::Null));
            f.insert("fromHook".into(), json!(flag(obj.get("fromHook"))));
        }
        "branch_summary" => {
            f.insert("fromId".into(), value_or(obj.get("fromId"), json!("")));
            f.insert("summary".into(), value_or(obj.get("summary"), json!("")));
            f.insert("details".into(), value_or(obj.get("details"), Value::Null));
            f.insert("fromHook".into(), json!(flag(obj.get("fromHook"))));
        }
        "custom_message" => {
            f.insert("customType".into(), value_or(obj.get("customType"), json!("")));
            f.insert("content".into(), json!(content_of(obj.get("content")).0));
            f.insert("display".into(), json!(flag(obj.get("display"))));
            f.insert("details".into(), value_or(obj.get("details"), Value::Null));
        }
        _ => passthrough(obj, &mut f),
    }
    (ty.to_string(), f)
}

/// Every field of the entry except the envelope.
fn passthrough(obj: &Map<String, Value>, f: &mut Map<String, Value>) {
    for (k, val) in obj {
        if !matches!(k.as_str(), "type" | "id" | "parentId" | "timestamp") {
            f.insert(k.clone(), val.clone());
        }
    }
}

fn value_or(v: Option<&Value>, default: Value) -> Value {
    match v {
        None | Some(Value::Null) => default,
        Some(other) => other.clone(),
    }
}

fn flag(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool).unwrap_or(false)
}

/// The text of a pi content value (a string or content blocks), its reasoning, and its tool calls
/// in the wire shape (`id`, `name`, `arguments` as a JSON string).
fn content_of(v: Option<&Value>) -> (String, Option<String>, Vec<Value>) {
    match v {
        Some(Value::String(s)) => (s.clone(), None, Vec::new()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut reasoning: Option<String> = None;
            let mut calls = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("thinking") => reasoning
                        .get_or_insert_with(String::new)
                        .push_str(b.get("thinking").and_then(Value::as_str).unwrap_or("")),
                    Some("toolCall") => {
                        let args = match b.get("arguments") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => "{}".to_string(),
                        };
                        calls.push(json!({
                            "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                            "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                            "arguments": args,
                        }));
                    }
                    _ => {}
                }
            }
            (text, reasoning, calls)
        }
        _ => (String::new(), None, Vec::new()),
    }
}

/// The exact wire object for a pi user, assistant or tool result message (see `decode_wire`).
fn synth_wire(role: &str, msg: &Map<String, Value>) -> Value {
    let (text, reasoning, calls) = content_of(msg.get("content"));
    let (wire_role, calls, tool_call_id) = match role {
        "assistant" => ("assistant", calls, Value::Null),
        "toolResult" => ("tool", Vec::new(), value_or(msg.get("toolCallId"), Value::Null)),
        _ => ("user", Vec::new(), Value::Null),
    };
    json!({
        "role": wire_role,
        "content": text,
        "reasoning_content": reasoning.map_or(Value::Null, Value::from),
        "tool_calls": calls,
        "tool_call_id": tool_call_id,
    })
}
