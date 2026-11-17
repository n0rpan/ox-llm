//! A11: the settings schema table, validation and merge rules.
// ported from pi packages/coding-agent/docs/settings.md and src/core/settings-manager.ts
use crate::Diagnostic;
use oxa_proto::{Level, Value};
use serde_json::Map;
use std::collections::HashSet;

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

const fn key(key: &'static str, kind: Kind, default: &'static str) -> KeyDef {
    KeyDef {
        key,
        kind,
        default,
        supported: true,
    }
}

const fn verdict_o(key: &'static str, kind: Kind, default: &'static str) -> KeyDef {
    KeyDef {
        key,
        kind,
        default,
        supported: false,
    }
}

/// Sorted by key (byte order); `schema_is_sorted_and_unique` in the tests checks it.
const SCHEMA: &[KeyDef] = &[
    key("$schema", Kind::String, "null"),
    key("autocompleteMaxVisible", Kind::Number, "5"),
    key("branchSummary.reserveTokens", Kind::Number, "16384"),
    key("branchSummary.skipPrompt", Kind::Bool, "false"),
    key(
        "cacheWarming",
        Kind::Enum(&["off", "streaming", "idle"]),
        "\"idle\"",
    ),
    // oxa: `codemode.enabled`, `codemode.toolSearch` and `codemode.maxWorkers` turn on the
    // built-in codemode / tool-search hook sets and size the script pool (oxa-cli hooks.rs);
    // pi activates them through `defaultTools`, which oxa honours as well.
    key("codemode.enabled", Kind::Bool, "false"),
    key("codemode.maxWorkers", Kind::Number, "0"),
    key("codemode.mode", Kind::Enum(&["on", "only"]), "\"on\""),
    key("codemode.toolSearch", Kind::Bool, "false"),
    key("compaction.enabled", Kind::Bool, "true"),
    key("compaction.keepRecentTokens", Kind::Number, "4000"),
    key("compaction.modelOverrides", Kind::Object, "null"),
    key("compaction.reserveTokens", Kind::Number, "16384"),
    key("defaultModel", Kind::String, "null"),
    key(
        "defaultProjectTrust",
        Kind::Enum(&["ask", "always", "never"]),
        "\"ask\"",
    ),
    verdict_o("defaultProvider", Kind::String, "null"),
    key(
        "defaultThinkingLevel",
        Kind::Enum(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
        "\"medium\"",
    ),
    key(
        "defaultTools",
        Kind::StringList,
        "[\"read\",\"bash\",\"edit\",\"write\",\"grep\",\"find\",\"ls\"]",
    ),
    key("done", Kind::StringList, "[]"),
    key(
        "doubleEscapeAction",
        Kind::Enum(&["tree", "fork", "none"]),
        "\"tree\"",
    ),
    key("editorPaddingX", Kind::Number, "0"),
    verdict_o("enableAnalytics", Kind::Bool, "false"),
    verdict_o("enableInstallTelemetry", Kind::Bool, "true"),
    key("enableSkillCommands", Kind::Bool, "true"),
    key("enabledModels", Kind::StringList, "null"),
    key("extensions", Kind::StringList, "[]"),
    key("externalEditor", Kind::String, "null"),
    key(
        "followUpMode",
        Kind::Enum(&["all", "one-at-a-time"]),
        "\"one-at-a-time\"",
    ),
    key("fullscreenCopyOnSelect", Kind::Bool, "true"),
    key(
        "fullscreenExitOutput",
        Kind::Enum(&["transcript", "resume-hint"]),
        "\"transcript\"",
    ),
    key(
        "fullscreenScrollbar",
        Kind::Enum(&["auto", "always", "hidden"]),
        "\"auto\"",
    ),
    key("fullscreenWheelScrollLines", Kind::Any, "\"auto\""),
    key("hideThinkingBlock", Kind::Bool, "false"),
    key("hooks", Kind::Object, "{}"),
    key("httpIdleTimeoutMs", Kind::Any, "300000"),
    key("httpProxy", Kind::String, "null"),
    verdict_o("images.autoResize", Kind::Bool, "true"),
    key("images.blockImages", Kind::Bool, "false"),
    key("markdown.codeBlockIndent", Kind::String, "\"  \""),
    key(
        "markdown.mermaid",
        Kind::Enum(&["off", "final", "streaming"]),
        "\"streaming\"",
    ),
    key("modelThinkingLevels", Kind::Object, "{}"),
    key("outputPad", Kind::Number, "1"),
    key("oxa.allowHomeCwd", Kind::Bool, "false"),
    // F5: seconds; unset = 120; 0 = none.
    key("oxa.bash.defaultTimeout", Kind::Number, "null"),
    key("oxa.bootPreload", Kind::Bool, "true"),
    key("oxa.builtinSkills", Kind::Bool, "true"),
    key("oxa.cloud", Kind::Object, "{}"),
    // V020-OPTIMIZE #7: unset = on.
    key("oxa.compaction.idleSummary", Kind::Bool, "null"),
    // L5 mid-run: unset = on.
    key("oxa.compaction.midRun", Kind::Bool, "null"),
    key("oxa.compaction.softTokens", Kind::Number, "null"),
    // V020-OPTIMIZE #7: soft length target of a summary in tokens; unset = 700; 0 = none.
    key("oxa.compaction.summaryTarget", Kind::Number, "null"),
    // L5: unset = on.
    key("oxa.compaction.turnStart", Kind::Bool, "null"),
    // V020-OPTIMIZE #1 / #2: unset = on.
    key("oxa.diet.output", Kind::Bool, "null"),
    key("oxa.diet.tools", Kind::Bool, "null"),
    key("oxa.engine.allowRemote", Kind::Bool, "false"),
    key("oxa.engine.inProcessCloud", Kind::Bool, "true"),
    key("oxa.engine.inProcessLocal", Kind::Bool, "true"),
    // The per-prompt environment note (date, time, runtime facts): unset = on.
    key("oxa.envNote", Kind::Bool, "null"),
    // Model replies per run before it ends with `turn_limit`; unset or 0 = no limit (#507).
    key("oxa.local", Kind::Object, "{}"),
    key("oxa.maxTurns", Kind::Number, "null"),
    key(
        "oxa.mode",
        Kind::Enum(&["plan", "workspace", "ask"]),
        "\"workspace\"",
    ),
    key("oxa.offline", Kind::Bool, "false"),
    // V020-OPTIMIZE #13 / OXA-LOCAL-FIRST F1: timing only (bytes unchanged); unset = on.
    key("oxa.prefill.paste", Kind::Bool, "null"),
    key("oxa.prefill.tools", Kind::Bool, "null"),
    key("oxa.reportIssue.repo", Kind::String, "null"),
    // B3 (lane harden): user settings only (read from the user file whatever the project says).
    // `greedy` = W6 `temperature: 0` (MTP on); `model` = the model card's sampling (oxa-core
    // `sampling.rs`). The seed is sent only with `model`.
    key(
        "oxa.sampling.mode",
        Kind::Enum(&["greedy", "model"]),
        "\"greedy\"",
    ),
    key("oxa.sampling.seed", Kind::Number, "null"),
    key("oxa.sandbox.allowHomeWrite", Kind::Bool, "false"),
    key("oxa.sandbox.net", Kind::Enum(&["on", "off"]), "\"on\""),
    // B2 (lane harden): extra binds of the bash sandbox; user settings only.
    key("oxa.sandbox.readPaths", Kind::StringList, "[]"),
    key("oxa.sandbox.writePaths", Kind::StringList, "[]"),
    // Skills another product synced into `~/.agents/skills` (`synced/`); off = skipped.
    key("oxa.skills.includeSynced", Kind::Bool, "false"),
    // O-146: unset = on except on Flash and below 32K (`SubagentHooks::declared_for`).
    key("oxa.subagent.enabled", Kind::Bool, "null"),
    key("oxa.subagent.remoteConcurrency", Kind::Number, "4"),
    key("oxa.subagent.resultCap", Kind::Number, "6144"),
    key("oxa.subagent.smallContext", Kind::Bool, "false"),
    key("oxa.subagent.softTokens", Kind::Number, "24000"),
    // Optional local engine cap, capability-gated; null leaves the current policy unchanged.
    key("oxa.thinking.budget", Kind::Number, "null"),
    // Adaptive escalation of tool follow-ups (oxa-core `think.rs`): calls without progress
    // before thinking turns on; 0 = never on that ground.
    key("oxa.thinking.escalation.fruitlessCalls", Kind::Number, "6"),
    // T5: unset = `auto`.
    key("oxa.thinking.policy", Kind::Enum(&["fixed", "auto"]), "null"),
    // Thinking on tool follow-ups under policy `auto` (oxa-core `think.rs`).
    key(
        "oxa.thinking.toolFollowups",
        Kind::Enum(&["auto", "on", "off"]),
        "\"auto\"",
    ),
    key("oxa.toolOutputCap", Kind::Number, "16384"),
    key("oxa.toolRepair", Kind::Bool, "true"),
    // The repeat guard for identical tool calls (oxa-core `repeat.rs`).
    key("oxa.toolRepeatGuard", Kind::Bool, "true"),
    // O-169: `web_search` (oxa-web); declared only with a Brave key and never offline.
    key("oxa.webSearch.enabled", Kind::Bool, "true"),
    key("oxa.webSearch.maxTokens", Kind::Number, "2048"),
    key(
        "oxa.webSearch.provider",
        Kind::Enum(&["brave", "none"]),
        "\"brave\"",
    ),
    key("packages", Kind::Any, "[]"),
    // #202: the permission mode at start; a project's value may only lower it
    // (`Settings::default_permission_mode`).
    key(
        "permissions.defaultMode",
        Kind::Enum(&["standard", "auto", "yolo", "plan"]),
        "\"auto\"",
    ),
    key("prompts", Kind::StringList, "[]"),
    key("quietStartup", Kind::Any, "false"),
    key("retry.baseDelayMs", Kind::Number, "2000"),
    key("retry.enabled", Kind::Bool, "true"),
    key("retry.maxAgentDelayMs", Kind::Number, "60000"),
    key("retry.maxRetries", Kind::Number, "3"),
    key("retry.provider.maxRetries", Kind::Number, "0"),
    key("retry.provider.maxRetryDelayMs", Kind::Number, "60000"),
    key("retry.provider.timeoutMs", Kind::Number, "null"),
    verdict_o("selfUpdateCommand", Kind::StringList, "null"),
    key("sessionDir", Kind::String, "null"),
    verdict_o(
        "sessionShareDestination",
        Kind::Enum(&["auto", "gist", "radius"]),
        "\"auto\"",
    ),
    key("shellCommandPrefix", Kind::String, "null"),
    key("shellPath", Kind::String, "null"),
    key("showCacheMissNotices", Kind::Bool, "false"),
    key("skills", Kind::StringList, "[]"),
    key(
        "steeringMode",
        Kind::Enum(&["all", "one-at-a-time"]),
        "\"one-at-a-time\"",
    ),
    key("terminal.clearOnShrink", Kind::Bool, "false"),
    key("terminal.imageWidthCells", Kind::Number, "60"),
    key("terminal.showImages", Kind::Bool, "true"),
    key("theme", Kind::String, "\"system\""),
    key("themes", Kind::StringList, "[]"),
    verdict_o(
        "transport",
        Kind::Enum(&["auto", "sse", "websocket", "websocket-cached"]),
        "\"auto\"",
    ),
    key(
        "treeFilterMode",
        Kind::Enum(&["default", "no-tools", "user-only", "labeled-only", "all"]),
        "\"default\"",
    ),
    key(
        "tuiMode",
        Kind::Enum(&["regular", "fullscreen"]),
        "\"fullscreen\"",
    ),
    key(
        "ui.animation",
        Kind::Enum(&["arcade", "minimal", "off"]),
        "\"arcade\"",
    ),
    key("ui.animationGraphics", Kind::Enum(&["auto", "off"]), "\"auto\""),
    key(
        "ui.animationScene",
        Kind::Enum(&["graphics", "stack", "stars", "charge"]),
        "null",
    ),
    key(
        "ui.notify",
        Kind::Enum(&["off", "unfocused", "always"]),
        "\"unfocused\"",
    ),
    key("ui.reducedMotion", Kind::Bool, "false"),
    key("ui.skin", Kind::Enum(&["auto", "pi", "oxa", "ascii"]), "\"auto\""),
    verdict_o("warnings.anthropicExtraUsage", Kind::Bool, "true"),
    verdict_o("websocketConnectTimeoutMs", Kind::Any, "15000"),
];

/// Every key of pi's settings.md plus oxa's `oxa.*` keys (`oxa.toolOutputCap`,
/// `oxa.compaction.softTokens`, `oxa.mode`, `oxa.subagent.*`, O-152's `oxa.bootPreload`,
/// `oxa.builtinSkills`, `oxa.reportIssue.repo`, `oxa.thinking.policy`; `ui.skin`), sorted by key.
pub fn schema() -> &'static [KeyDef] {
    SCHEMA
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
    SCHEMA.iter().find(|d| d.key == key)
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
    if SCHEMA.iter().any(|d| d.key.starts_with(&prefix)) {
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
