//! A11: the settings schema table, validation and merge rules.
// ported from pi packages/coding-agent/docs/settings.md and src/core/settings-manager.ts
use crate::Diagnostic;
use oxa_proto::{Level, Value};
use serde_json::Map;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Value kind of a settings key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Number,
    String,
    /// One of the listed strings.
    Enum(&'static [&'static str]),
    StringList,
    Object,
    Any,
}

/// One documented settings key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDef {
    /// Dotted key as in settings.md.
    pub key: &'static str,
    pub kind: Kind,
    /// Default as JSON text.
    pub default: &'static str,
    /// `false` = accepted and ignored in oxa with a warning (verdict O keys).
    pub supported: bool,
}

/// The built-in tools enabled when no `defaultTools` is set (oxa has seven; pi's `powershell`
/// is not among them).
// Lead-only tools are included in settings patches, but are registered by the CLI only for leads.
pub(crate) const DEFAULT_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls", "ask"];

/// Every key of pi's settings.md plus oxa's `oxa.*` keys (`oxa.toolOutputCap`,
/// `oxa.compaction.softTokens`, `oxa.mode`, `oxa.subagent.*`, O-152's `oxa.bootPreload`,
/// `oxa.builtinSkills`, `oxa.reportIssue.repo`, `oxa.thinking.policy`; `ui.skin`), sorted by key.
///
/// Derived from the descriptor rows that declare a key (`descriptor.rs`, `descriptor/*.rs`).
pub fn schema() -> &'static [KeyDef] {
    static SCHEMA: OnceLock<Vec<KeyDef>> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            let mut keys: Vec<KeyDef> = crate::descriptor::all_rows()
                .iter()
                .filter_map(|row| {
                    row.schema.map(|info| KeyDef {
                        key: row.key,
                        kind: info.kind,
                        default: info.default,
                        supported: info.supported,
                    })
                })
                .collect();
            keys.sort_by(|a, b| a.key.cmp(b.key));
            keys
        })
        .as_slice()
}

/// Check `v` (one settings file) against [`schema`]. Unknown key, wrong type, unsupported key =
/// one diagnostic each; never an error. `source` is the file path for the diagnostics.
pub fn validate(v: &Value, source: &str) -> Vec<Diagnostic> {
    check_root(v, source).1
}

/// pi's merge: objects merge key by key, scalars and plain lists from `project` replace
/// `user`, lists made of `+x` / `-x` entries patch the lower layer ([`apply_list_patch`]).
/// The native `oxa.local` launch subtree replaces literally, preserving argv bytes.
pub fn merge_settings(user: &Value, project: &Value) -> Value {
    match project {
        Value::Object(_) => {
            let mut merged = merge_value(Some(user), project);
            // Native launch data is literal: argv flags are not list-patch edits.
            if let Some(local) = project.pointer("/oxa/local") {
                merged["oxa"]["local"] = local.clone();
            }
            merged
        }
        _ => user.clone(),
    }
}

/// Apply `+x` (add if absent, at the end) and `-x` (remove) entries to `base`. A patch without
/// any `+`/`-` entry replaces `base`. Exclusion wins when both are given for the same name.
pub fn apply_list_patch(base: &[String], patch: &[String]) -> Vec<String> {
    if !patch.iter().any(|p| is_patch_entry(p)) {
        return dedup(patch.iter().cloned());
    }
    let excluded: HashSet<&str> = patch.iter().filter_map(|p| p.strip_prefix('-')).collect();
    let plain: Vec<String> = patch.iter().filter(|p| !is_patch_entry(p)).cloned().collect();
    let mut out = if plain.is_empty() {
        dedup(base.iter().cloned())
    } else {
        dedup(plain.into_iter())
    };
    for entry in patch {
        if let Some(name) = entry.strip_prefix('+') {
            if !name.is_empty() && !out.iter().any(|x| x == name) {
                out.push(name.to_string());
            }
        } else if let Some(name) = entry.strip_prefix('-') {
            out.retain(|x| x != name);
        }
    }
    out.retain(|x| !excluded.contains(x.as_str()));
    out
}

// ---- crate-internal helpers (not part of the frozen surface) ----

/// The schema entry for an exact dotted key.
pub(crate) fn find_key(key: &str) -> Option<&'static KeyDef> {
    schema().iter().find(|d| d.key == key)
}

/// Whether `v` has the value kind `kind`.
pub(crate) fn kind_matches(kind: Kind, v: &Value) -> bool {
    match kind {
        Kind::Bool => v.is_boolean(),
        Kind::Number => v.as_u64().is_some(),
        Kind::String => v.is_string(),
        Kind::Enum(options) => v.as_str().is_some_and(|s| options.contains(&s)),
        Kind::StringList => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
        Kind::Object => v.is_object(),
        Kind::Any => true,
    }
}

/// Human description of a kind, for diagnostics.
pub(crate) fn kind_name(kind: Kind) -> String {
    match kind {
        Kind::Bool => "true or false".to_string(),
        Kind::Number => "a non-negative whole number".to_string(),
        Kind::String => "a string".to_string(),
        Kind::Enum(options) => {
            let quoted: Vec<String> = options.iter().map(|o| format!("\"{o}\"")).collect();
            format!("one of {}", quoted.join(", "))
        }
        Kind::StringList => "a list of strings".to_string(),
        Kind::Object => "an object".to_string(),
        Kind::Any => "any value".to_string(),
    }
}

/// A warning diagnostic for one settings source.
pub(crate) fn warn(source: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        level: Level::Warning,
        source: source.to_string(),
        message: message.into(),
    }
}

/// Validate one settings object. Returns the value with every wrongly typed known key removed
/// (so the lower layer or the default shows through) and the diagnostics. Unknown and
/// unsupported keys are kept; unsupported keys warn.
pub(crate) fn check_root(v: &Value, source: &str) -> (Value, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let out = match v {
        Value::Object(map) => Value::Object(check_map(map, "", source, &mut diags)),
        Value::Null => Value::Object(Map::new()),
        _ => {
            diags.push(warn(
                source,
                "settings must be a JSON object; the file is ignored",
            ));
            Value::Object(Map::new())
        }
    };
    (out, diags)
}

fn check_map(
    map: &Map<String, Value>,
    path: &str,
    source: &str,
    diags: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, child) in map {
        let dotted = if path.is_empty() {
            k.clone()
        } else {
            format!("{path}.{k}")
        };
        if let Some(kept) = check_value(child, &dotted, source, diags) {
            out.insert(k.clone(), kept);
        }
    }
    out
}

fn check_value(v: &Value, path: &str, source: &str, diags: &mut Vec<Diagnostic>) -> Option<Value> {
    if let Some(def) = find_key(path) {
        if !def.supported {
            diags.push(warn(
                source,
                format!("setting `{path}` is accepted but ignored in oxa"),
            ));
            return Some(v.clone());
        }
        if kind_matches(def.kind, v) || (path == "oxa.thinking.budget" && v.is_null()) {
            return Some(v.clone());
        }
        diags.push(warn(
            source,
            format!(
                "setting `{path}` has the wrong type (expected {}); the default is used",
                kind_name(def.kind)
            ),
        ));
        return None;
    }
    let prefix = format!("{path}.");
    if schema().iter().any(|d| d.key.starts_with(&prefix)) {
        return match v {
            Value::Object(map) => Some(Value::Object(check_map(map, path, source, diags))),
            _ => {
                diags.push(warn(
                    source,
                    format!("setting `{path}` must be an object; the default is used"),
                ));
                None
            }
        };
    }
    diags.push(warn(source, format!("unknown setting `{path}` (kept, ignored)")));
    Some(v.clone())
}

/// `user` merged with `project` under pi's rules (see [`merge_settings`]).
fn merge_value(base: Option<&Value>, over: &Value) -> Value {
    match over {
        Value::Object(o) => {
            let mut out = match base {
                Some(Value::Object(b)) => b.clone(),
                _ => Map::new(),
            };
            for (k, v) in o {
                let merged = merge_value(out.get(k), v);
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        Value::Array(items) if is_patch_list(items) => {
            let base_list: Vec<String> = match base {
                Some(Value::Array(b)) => b.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            let patch: Vec<String> = items
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            Value::Array(
                apply_list_patch(&base_list, &patch)
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            )
        }
        _ => over.clone(),
    }
}

/// A `+x` / `-x` entry.
fn is_patch_entry(s: &str) -> bool {
    s.starts_with('+') || s.starts_with('-')
}

/// A list of strings with at least one `+x` / `-x` entry.
fn is_patch_list(items: &[Value]) -> bool {
    items.iter().all(Value::is_string) && items.iter().any(|x| x.as_str().is_some_and(is_patch_entry))
}

/// Keep the first occurrence of each name, in order.
fn dedup(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items.filter(|x| seen.insert(x.clone())).collect()
}
