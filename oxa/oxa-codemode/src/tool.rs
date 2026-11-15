//! E6: the `codemode` tool (options line, run, result text).
// ported from pi packages/codemode/src/{index,source,types}.ts
use crate::api::ScriptSink;
use crate::{Shared, CODEMODE_TOOL, STORE_ENTRY_TYPE};
use oxa_proto::{Access, HostAction, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

/// pi's default `max_output_tokens`.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 10_000;
/// A script fails when its output passes this many characters.
pub const MAX_OUTPUT_CHARS: usize = 16_777_216;
/// A script fails when it makes more than this many `text()` / console calls.
pub const MAX_OUTPUT_ITEMS: usize = 100_000;
/// `store()` limits: one value, all values (characters of JSON).
pub const MAX_STORE_VALUE_CHARS: usize = 262_144;
pub const MAX_STORE_TOTAL_CHARS: usize = 1_048_576;

/// Prefix of the options line (first line of a script only).
pub(crate) const OPTIONS_PREFIX: &str = "// @options:";
/// Largest delay `setTimeout` supports, which bounds `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// The options line of a script: `// @options: {"max_output_tokens": 2000, "timeout_ms": 60000}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptOptions {
    pub max_output_tokens: u32,
    /// Hard deadline for the whole script, including the wait for a worker.
    pub timeout_ms: Option<u64>,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        ScriptOptions {
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            timeout_ms: None,
        }
    }
}

/// Parse the options line. It is honoured only as the first line; an `// @options:` line
/// anywhere else is `Err` (the call fails before the script runs), as is malformed JSON or an
/// unknown key. Returns the options and the script without the options line.
///
/// The returned script keeps its leading newline, so line numbers in errors match the input.
pub fn parse_options_line(source: &str) -> Result<(ScriptOptions, &str), String> {
    if source.trim().is_empty() {
        return Err(format!(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `{OPTIONS_PREFIX} {{\"max_output_tokens\": 1000}}`."
        ));
    }
    for (index, line) in source.split('\n').enumerate().skip(1) {
        if line.trim_start().starts_with(OPTIONS_PREFIX) {
            return Err(format!(
                "The {OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
                index + 1
            ));
        }
    }
    let first_end = source.find('\n').unwrap_or(source.len());
    let first = source[..first_end].trim_end_matches('\r').trim_start();
    let Some(directive) = first.strip_prefix(OPTIONS_PREFIX) else {
        return Ok((ScriptOptions::default(), source));
    };
    let code = &source[first_end..];
    if code.trim().is_empty() {
        return Err(format!(
            "The {OPTIONS_PREFIX} line must be followed by JavaScript source on subsequent lines"
        ));
    }
    let options = parse_options(directive.trim())?;
    Ok((options, code))
}

fn parse_options(directive: &str) -> Result<ScriptOptions, String> {
    const FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";
    if directive.is_empty() {
        return Err(format!(
            "@options must be a JSON object with supported fields {FIELDS_TEXT}"
        ));
    }
    let value: Value = serde_json::from_str(directive)
        .map_err(|e| format!("@options must be valid JSON with supported fields {FIELDS_TEXT}: {e}"))?;
    let Some(fields) = value.as_object() else {
        return Err(format!(
            "@options must be a JSON object with supported fields {FIELDS_TEXT}"
        ));
    };
    for key in fields.keys() {
        if key != "max_output_tokens" && key != "timeout_ms" {
            return Err(format!("@options only supports {FIELDS_TEXT}; got `{key}`"));
        }
    }
    let mut options = ScriptOptions::default();
    if let Some(v) = fields.get("max_output_tokens") {
        let n = v.as_u64().filter(|n| *n <= u64::from(u32::MAX)).ok_or_else(|| {
            "@options field `max_output_tokens` must be a non-negative safe integer".to_string()
        })?;
        options.max_output_tokens = n as u32;
    }
    if let Some(v) = fields.get("timeout_ms") {
        let n = v
            .as_u64()
            .filter(|n| *n > 0 && *n <= MAX_TIMEOUT_MS)
            .ok_or_else(|| {
                format!("@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}")
            })?;
        options.timeout_ms = Some(n);
    }
    Ok(options)
}

/// What a script produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScriptOutcome {
    /// `false` when the script threw, was interrupted, ran out of memory or passed a limit.
    pub ok: bool,
    /// Text items in order (`text()` calls, then a top-level `return` value).
    pub texts: Vec<String>,
    /// `console.*` lines in order.
    pub console: Vec<String>,
    /// The error text when `!ok`.
    pub error: Option<String>,
    /// `store()` writes of this script, key -> value (`Null` = delete); applied only when `ok`.
    pub store_writes: Vec<(String, Value)>,
}

/// Token estimate used for the output cap: about four characters per token, rounded up.
/// Deterministic; the engine's tokenizer is not reachable from a script worker.
pub fn estimate_tokens(text: &str) -> u32 {
    text.chars().count().div_ceil(4) as u32
}

/// The result text for the model (W4: deterministic, no wall time).
///
/// First line `Script completed` or `Script failed`; then the text items, each on its own
/// line, each preceded by `==> text N/M <==` when there is more than one; then the error text
/// when failed; then, if any, the console lines inside `<console_output>` .. `</console_output>`.
/// Output longer than `max_output_tokens` (counted by `count`) keeps its start and end around
/// one fixed marker line naming the spill file, which holds the full text.
pub fn format_outcome(
    o: &ScriptOutcome,
    opts: &ScriptOptions,
    spill_dir: &std::path::Path,
    count: &dyn Fn(&str) -> u32,
) -> String {
    let mut body = String::new();
    let total = o.texts.len();
    for (i, text) in o.texts.iter().enumerate() {
        if total > 1 {
            let _ = writeln!(body, "==> text {}/{} <==", i + 1, total);
        }
        body.push_str(text);
        body.push('\n');
    }
    if body.ends_with('\n') {
        body.pop();
    }
    let body = cap_output(&body, opts.max_output_tokens, spill_dir, count);

    let mut parts: Vec<String> = Vec::new();
    parts.push(if o.ok { "Script completed" } else { "Script failed" }.to_string());
    if !body.is_empty() {
        parts.push(body);
    }
    if !o.ok {
        let error = o.error.as_deref().unwrap_or("unknown error");
        parts.push(format!("Script error: {error}"));
    }
    if !o.console.is_empty() {
        parts.push(format!(
            "<console_output>\n{}\n</console_output>",
            o.console.join("\n")
        ));
    }
    parts.join("\n")
}

/// Keep the start and end of `body` when it is over `max_tokens`; the full text goes to a spill
/// file named by its content hash (deterministic), and the marker line names that file.
fn cap_output(body: &str, max_tokens: u32, spill_dir: &Path, count: &dyn Fn(&str) -> u32) -> String {
    let total = count(body);
    if total <= max_tokens {
        return body.to_string();
    }
    let path = spill_dir.join(format!("codemode-{:016x}.txt", fnv1a(body.as_bytes())));
    let _ = std::fs::create_dir_all(spill_dir);
    let _ = std::fs::write(&path, body);

    // Character boundaries, including the end of the text.
    let mut bounds: Vec<usize> = body.char_indices().map(|(i, _)| i).collect();
    bounds.push(body.len());
    let half = max_tokens / 2;
    let rest = max_tokens - half;

    // Largest head whose count fits `half`.
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if count(&body[..bounds[mid]]) <= half {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let head_end = bounds[lo];

    // Smallest tail start whose count fits `rest`.
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if count(&body[bounds[mid]..]) <= rest {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let tail_start = bounds[lo].max(head_end);

    format!(
        "{}\n... output truncated: {} tokens in total, showing the start and end; the full output is in {}\n{}",
        &body[..head_end],
        total,
        path.display(),
        &body[tail_start..]
    )
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The fixed description of the `codemode` tool. It never lists tools (W3).
const CODEMODE_DESCRIPTION: &str = "Run a JavaScript script that calls other tools with `tools.<name>({...})`, \
in parallel with `Promise.all` or `Promise.allSettled`. Only the script's output reaches you: use `text(value)` \
to add output, `return` for a final value, and `console.log` for diagnostics. `searchTools(query)` and \
`describeTool(name)` find tools. Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`. \
The script has no network, file system or timers; reach the outside world through tools only.";

const CODEMODE_SCHEMA: &str = r#"{"type":"object","properties":{"code":{"type":"string","description":"JavaScript source of the script. Optionally first line // @options: {...}."}},"required":["code"]}"#;

/// The `codemode` tool. Arguments: `{"code": "<JavaScript>"}`. Class `Exec` (what the script
/// then calls is checked per tool call by the host).
pub struct CodemodeTool {
    pub(crate) shared: Arc<Shared>,
    pub(crate) spec: ToolSpec,
}

impl CodemodeTool {
    /// The spec's description is a constant text (W3): it never lists tools.
    pub fn new(shared: Arc<Shared>) -> CodemodeTool {
        CodemodeTool {
            shared,
            spec: ToolSpec {
                name: CODEMODE_TOOL.to_string(),
                description: CODEMODE_DESCRIPTION.to_string(),
                schema_json: CODEMODE_SCHEMA.to_string(),
                class: ToolClass::Exec,
                exposure: oxa_proto::Exposure::Direct,
                source: oxa_proto::Source::Builtin,
            },
        }
    }

    /// Apply a successful script's store writes to the session store and append the new
    /// snapshot as a `codemode-store` entry.
    fn persist_store(&self, writes: &[(String, Value)]) {
        let snapshot = {
            let mut store = self.shared.store.lock().unwrap_or_else(|p| p.into_inner());
            for (key, value) in writes {
                if value.is_null() {
                    store.remove(key);
                } else {
                    store.insert(key.clone(), value.clone());
                }
            }
            store.clone()
        };
        if let Ok(host) = self.shared.host() {
            let data = Value::Object(snapshot.into_iter().collect());
            let _ = host.call(HostAction::AppendEntry {
                custom_type: STORE_ENTRY_TYPE.to_string(),
                data,
            });
        }
    }
}

impl Tool for CodemodeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Exec { command: "codemode script", net: false }]`; `Err` when `code` is
    /// missing or not a string.
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        match args.get("code").and_then(Value::as_str) {
            Some(_) => Ok(vec![Access::Exec {
                command: "codemode script".to_string(),
                net: false,
            }]),
            None => Err("codemode needs a `code` string argument".to_string()),
        }
    }
    /// Parse the options line, run the script on the pool ([`crate::WorkerPool::run`]) with
    /// the globals of [`crate::api::install_globals`], apply the store writes on success
    /// (append a `codemode-store` entry through the host), return [`format_outcome`].
    /// `cx.cancel` interrupts the script. Never panics.
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let Some(code) = args.get("code").and_then(Value::as_str) else {
            return ToolOutput::error("codemode needs a `code` string argument");
        };
        let (opts, script) = match parse_options_line(code) {
            Ok(parsed) => parsed,
            Err(message) => return ToolOutput::error(message),
        };
        let outcome = self.shared.pool.run(&self.shared, script, &opts, cx.cancel);
        if outcome.ok && !outcome.store_writes.is_empty() {
            self.persist_store(&outcome.store_writes);
        }
        let text = format_outcome(&outcome, &opts, &self.shared.spill_dir, &estimate_tokens);
        ToolOutput {
            text,
            is_error: !outcome.ok,
            ui: Value::Null,
        }
    }
}

/// Error of a script that stopped at an output or store limit. `None` while within limits.
pub(crate) fn output_limit_error(sink: &ScriptSink) -> Option<String> {
    if sink.chars > MAX_OUTPUT_CHARS {
        return Some(format!(
            "script output exceeds {MAX_OUTPUT_CHARS} characters of text and image data"
        ));
    }
    if sink.texts.len() + sink.console.len() > MAX_OUTPUT_ITEMS {
        return Some(format!(
            "script makes more than {MAX_OUTPUT_ITEMS} text() or console calls"
        ));
    }
    None
}
