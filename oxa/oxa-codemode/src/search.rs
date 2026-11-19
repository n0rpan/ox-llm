//! F4: `tool_search` and the frozen `call_tool` dispatcher.
// ported from pi packages/coding-agent/src/core/tools/tool-search.ts (ranking)
use crate::{Namespace, Shared};
use oxa_proto::{
    Access, Exposure, HostAction, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value,
};
use serde_json::Map;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

/// pi's default result count.
pub const DEFAULT_SEARCH_LIMIT: usize = 8;

/// BM25 constants (pi's `search.ts`).
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// A query term in the name counts this many times its IDF.
const NAME_WEIGHT: f64 = 3.0;
/// A query term in the namespace text counts this many times its IDF.
const NAMESPACE_WEIGHT: f64 = 0.5;

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on", "or",
    "that", "the", "this", "to", "with",
];

/// Fixed text of `tool_search`. It never lists tools (W3).
const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata (names, descriptions and parameters) with BM25 and returns the matching tools with their JSON Schemas as text. Some tools, such as tools of MCP servers, are not offered upfront: use `tool_search` to find them, then call a match with `call_tool`.";
const TOOL_SEARCH_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query for deferred tools."},"limit":{"type":"integer","description":"Maximum number of tools to return. Defaults to 8."}},"required":["query"]}"#;

/// Fixed text of `call_tool`. It never lists tools (W3).
const CALL_TOOL_DESCRIPTION: &str = "Calls a tool that tool_search returned in this session, by its exact name, with its arguments. Any other name is refused. The call runs with the same permission checks and output limits as a direct tool call.";
const CALL_TOOL_SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"Exact name of a tool returned by tool_search."},"args":{"type":"object","description":"Arguments of that tool, as its schema describes."}},"required":["name","args"]}"#;

/// One ranked match.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub score: f64,
}

/// Rank `tools` for `query` with BM25 over name (split on `_`, `-`, camelCase), description,
/// the namespace's description, and the schema's property names and descriptions including
/// those reached through `$ref`. Ties break by name. `namespace` restricts to
/// `mcp__<namespace>__*`. Pure and deterministic. `limit` 0 = [`DEFAULT_SEARCH_LIMIT`].
pub fn rank(
    query: &str,
    tools: &[ToolSpec],
    namespaces: &[crate::Namespace],
    limit: usize,
    namespace: Option<&str>,
) -> Vec<SearchHit> {
    let limit = if limit == 0 { DEFAULT_SEARCH_LIMIT } else { limit };
    let query_terms: BTreeSet<String> = tokenize(query).into_iter().collect();
    if query_terms.is_empty() {
        return Vec::new();
    }
    let prefix = namespace.map(|ns| format!("mcp__{ns}__"));
    let docs: Vec<Indexed> = tools
        .iter()
        .filter(|t| prefix.as_ref().is_none_or(|p| t.name.starts_with(p.as_str())))
        .map(|t| index(t, namespaces))
        .collect();
    if docs.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as f64;
    let total_len: usize = docs.iter().map(|d| d.text_len).sum();
    let average = match total_len as f64 / n {
        a if a > 0.0 => a,
        _ => 1.0,
    };
    let idf: HashMap<&str, f64> = query_terms
        .iter()
        .map(|term| {
            let df = docs
                .iter()
                .filter(|d| {
                    d.name_terms.contains(term) || d.counts.contains_key(term) || d.ns_terms.contains(term)
                })
                .count() as f64;
            (term.as_str(), (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        })
        .collect();
    // Summed in the query terms' sorted order: a float sum in hash order could differ in the
    // last bit between runs, and the ranking must be the same every time (W4).
    let full_name_bonus: f64 = query_terms
        .iter()
        .map(|term| idf.get(term.as_str()).copied().unwrap_or(0.0))
        .sum();
    let exact_query = normalize_name(query);

    let mut matches: Vec<(SearchHit, usize, bool)> = Vec::new();
    for doc in &docs {
        let norm = K1 * (1.0 - B + B * doc.text_len as f64 / average);
        let mut score = 0.0;
        let mut covered = 0usize;
        for term in &query_terms {
            let count = doc.counts.get(term).copied().unwrap_or(0) as f64;
            let weight = (if doc.name_terms.contains(term) {
                NAME_WEIGHT
            } else {
                0.0
            }) + (if doc.ns_terms.contains(term) {
                NAMESPACE_WEIGHT
            } else {
                0.0
            }) + count * (K1 + 1.0) / (count + norm);
            if weight > 0.0 {
                covered += 1;
            }
            score += idf.get(term.as_str()).copied().unwrap_or(0.0) * weight;
        }
        if score == 0.0 {
            continue;
        }
        if !doc.local_terms.is_empty() && doc.local_terms.iter().all(|t| query_terms.contains(t)) {
            score += full_name_bonus;
        }
        let norm_name = normalize_name(&doc.spec.name);
        let exact = norm_name == exact_query || norm_name.ends_with(&format!("__{exact_query}"));
        matches.push((
            SearchHit {
                name: doc.spec.name.clone(),
                description: doc.spec.description.clone(),
                score,
            },
            covered,
            exact,
        ));
    }
    matches.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(b.1.cmp(&a.1))
            .then(
                b.0.score
                    .partial_cmp(&a.0.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.0.name.cmp(&b.0.name))
    });
    matches.into_iter().take(limit).map(|(hit, _, _)| hit).collect()
}

/// The text `tool_search` returns: per hit its name, description and JSON Schema, then the one
/// fixed line that says to call matches with `call_tool {"name": .., "args": {..}}`.
/// `No matching tools.` when empty. Deterministic (W4).
pub fn format_hits(hits: &[SearchHit], tools: &[ToolSpec]) -> String {
    if hits.is_empty() {
        return "No matching tools.".to_string();
    }
    let mut out = String::new();
    for hit in hits {
        let schema = tools
            .iter()
            .find(|t| t.name == hit.name)
            .map(|t| t.schema_json.as_str())
            .unwrap_or("{}");
        let _ = writeln!(out, "name: {}", hit.name);
        let _ = writeln!(out, "description: {}", hit.description.trim());
        let _ = writeln!(out, "schema: {schema}");
        out.push('\n');
    }
    out.push_str("Call a matching tool with call_tool {\"name\": \"<name>\", \"args\": {...}}.");
    out
}

/// Parsed `tool_search` arguments: the query and the result limit.
fn parse_search_args(args: &Value) -> Result<(String, usize), String> {
    let query = match args.get("query").and_then(Value::as_str) {
        Some(q) if !q.trim().is_empty() => q.to_string(),
        Some(_) => return Err("query must not be empty".to_string()),
        None => return Err("tool_search needs a `query` string argument".to_string()),
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
        Some(v) => v
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| n as usize)
            .ok_or_else(|| "limit must be a positive integer".to_string())?,
    };
    Ok((query, limit))
}

/// The catalog entries `tool_search` may find: deferred and codemode-only tools.
fn searchable(shared: &Arc<Shared>) -> Vec<ToolSpec> {
    (shared.catalog)()
        .into_iter()
        .filter(|t| matches!(t.exposure, Exposure::Deferred | Exposure::Codemode))
        .collect()
}

/// `tool_search {query, limit?}`: ranks the catalog's `Deferred` and `Codemode` tools, records
/// the matched names in `Shared::matched`, returns [`format_hits`]. Class `Read`. Its spec is
/// constant (W3).
pub struct ToolSearchTool {
    pub(crate) shared: Arc<Shared>,
    pub(crate) spec: ToolSpec,
}

impl ToolSearchTool {
    pub fn new(shared: Arc<Shared>) -> ToolSearchTool {
        ToolSearchTool {
            shared,
            spec: ToolSpec {
                name: crate::TOOL_SEARCH_TOOL.to_string(),
                description: TOOL_SEARCH_DESCRIPTION.to_string(),
                schema_json: TOOL_SEARCH_SCHEMA.to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
        }
    }
}

impl Tool for ToolSearchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        parse_search_args(args)?;
        Ok(Vec::new())
    }
    fn run(&self, args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let (query, limit) = match parse_search_args(args) {
            Ok(parsed) => parsed,
            Err(message) => return ToolOutput::error(message),
        };
        let tools = searchable(&self.shared);
        let namespaces: Vec<Namespace> = (self.shared.namespaces)();
        let hits = rank(&query, &tools, &namespaces, limit, None);
        {
            let mut matched = self.shared.matched.lock().unwrap_or_else(|p| p.into_inner());
            for hit in &hits {
                matched.insert(hit.name.clone());
            }
        }
        ToolOutput::ok(format_hits(&hits, &tools))
    }
}

/// `call_tool {name, args}`: the one frozen dispatcher for deferred tools. Refuses (fixed
/// error text `Tool <name> has not been loaded by tool_search`) unless `name` is in
/// `Shared::matched`; otherwise runs it through `Host::call(HostAction::CallTool)`, so hooks,
/// `Policy` and `shape` apply to the target tool. Its spec is constant (W3). Its own class is
/// `Read`; `access` returns nothing (the target's access is checked by the host).
pub struct CallToolTool {
    pub(crate) shared: Arc<Shared>,
    pub(crate) spec: ToolSpec,
}

impl CallToolTool {
    pub fn new(shared: Arc<Shared>) -> CallToolTool {
        CallToolTool {
            shared,
            spec: ToolSpec {
                name: crate::CALL_TOOL_TOOL.to_string(),
                description: CALL_TOOL_DESCRIPTION.to_string(),
                schema_json: CALL_TOOL_SCHEMA.to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
        }
    }
}

/// The `name` and `args` of a `call_tool` call; `args` defaults to an empty object.
fn parse_call_args(args: &Value) -> Result<(String, Value), String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "call_tool needs a `name` string argument".to_string())?;
    let call_args = match args.get("args") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err("call_tool needs an `args` object".to_string()),
    };
    Ok((name.to_string(), call_args))
}

impl Tool for CallToolTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        parse_call_args(args)?;
        Ok(Vec::new())
    }
    fn run(&self, args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let (name, call_args) = match parse_call_args(args) {
            Ok(parsed) => parsed,
            Err(message) => return ToolOutput::error(message),
        };
        let matched = self
            .shared
            .matched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&name);
        if !matched {
            return ToolOutput::error(format!("Tool {name} has not been loaded by tool_search"));
        }
        let host = match self.shared.host() {
            Ok(host) => host,
            Err(e) => return ToolOutput::error(e.message),
        };
        match host.call(HostAction::CallTool {
            name: name.clone(),
            args: call_args,
        }) {
            Ok(reply) => ToolOutput {
                images: Vec::new(),
                text: reply
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: reply.get("isError").and_then(Value::as_bool).unwrap_or(false),
                ui: Value::Null,
            },
            Err(e) => ToolOutput::error(e.message),
        }
    }
}

/// One tool as the ranker reads it.
struct Indexed<'a> {
    spec: &'a ToolSpec,
    /// Terms of the name: `mcp`, `app`, `thread`, `read` for `mcp__app__thread_read`.
    name_terms: HashSet<String>,
    /// Terms of the name after its last `__`.
    local_terms: Vec<String>,
    counts: HashMap<String, usize>,
    text_len: usize,
    ns_terms: HashSet<String>,
}

fn index<'a>(tool: &'a ToolSpec, namespaces: &[Namespace]) -> Indexed<'a> {
    let schema: Value = serde_json::from_str(&tool.schema_json).unwrap_or(Value::Null);
    let mut parts = vec![tool.description.clone()];
    let mut seen = HashSet::new();
    seen.insert("#".to_string());
    schema_text(&schema, &schema, &mut parts, &mut seen);
    let text = parts
        .iter()
        .filter(|p| !p.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut text_len = 0usize;
    for term in tokenize(&text) {
        *counts.entry(term).or_insert(0) += 1;
        text_len += 1;
    }
    let local = tool.name.rsplit("__").next().unwrap_or_default();
    let mut local_terms: Vec<String> = tokenize(local);
    local_terms.sort();
    local_terms.dedup();
    let ns_text = namespace_of(&tool.name)
        .and_then(|ns| namespaces.iter().find(|n| n.name == ns))
        .map(|n| {
            [
                Some(n.name.as_str()),
                n.description.as_deref(),
                n.instructions.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
        })
        .unwrap_or_default();
    Indexed {
        spec: tool,
        name_terms: tokenize(&tool.name).into_iter().collect(),
        local_terms,
        counts,
        text_len,
        ns_terms: tokenize(&ns_text).into_iter().collect(),
    }
}

/// The MCP namespace of `mcp__<ns>__<tool>`, if the name has that shape.
fn namespace_of(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("mcp__")?;
    let (ns, _) = rest.split_once("__")?;
    Some(ns)
}

/// Schema descriptions and property names, recursively, following local `$ref`s into `root`.
/// `seen` holds the references already read, so recursive definitions are read once.
fn schema_text(schema: &Value, root: &Value, parts: &mut Vec<String>, seen: &mut HashSet<String>) {
    let Some(obj) = schema.as_object() else {
        return;
    };
    if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
        if let Some(path) = reference.strip_prefix('#') {
            if seen.insert(reference.to_string()) {
                if let Some(target) = root.pointer(path) {
                    schema_text(target, root, parts, seen);
                }
            }
        }
    }
    if let Some(description) = obj.get("description").and_then(Value::as_str) {
        parts.push(description.to_string());
    }
    if let Some(properties) = obj.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, root, parts, seen);
        }
    }
    if let Some(items) = obj.get("items") {
        schema_text(items, root, parts, seen);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = obj.get(key).and_then(Value::as_array) {
            for variant in variants {
                schema_text(variant, root, parts, seen);
            }
        }
    }
}

/// Lowercase, with characters that are not identifier characters as `_`.
fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Splits camelCase, lowercases, drops non-alphanumerics and stop words, and singularises.
fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut spaced = String::with_capacity(text.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                spaced.push(' ');
            }
        }
        spaced.push(c);
    }
    spaced
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| !STOP_WORDS.contains(w))
        .map(|w| {
            if w.chars().all(|c| c.is_ascii_alphanumeric()) {
                stem(w)
            } else {
                w.to_string()
            }
        })
        .collect()
}

/// Naive singular form: `issues` -> `issue`, `searches` -> `search`.
fn stem(term: &str) -> String {
    let n = term.len();
    if n > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..n - 3]);
    }
    if n > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| term.ends_with(s))
    {
        return term[..n - 2].to_string();
    }
    if n > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..n - 1].to_string();
    }
    term.to_string()
}
