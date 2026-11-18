//! A11: settings.json loading, merge and typed access.
// ported from pi packages/coding-agent/src/core/settings-manager.ts
use crate::schema::{check_root, find_key, kind_matches, kind_name, merge_settings, warn, DEFAULT_TOOLS};
use crate::{Diagnostic, Dirs};
use oxa_proto::{codes, OxaError, PermissionMode, ThinkingLevel, Value};
use serde_json::Map;
use std::path::{Path, PathBuf};

/// pi's `steeringMode` / `followUpMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

/// The `compaction.*` group with oxa's defaults (plan sec. 1): `enabled` true, `reserve_tokens`
/// 16384, `keep_recent_tokens` 4000, `soft_tokens` from `oxa.compaction.softTokens` (`None` =
/// derive from the context window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u32,
    pub keep_recent_tokens: u32,
    pub soft_tokens: Option<u32>,
}

/// The `retry.*` group (pi's keys and defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

/// H: the `retry.provider.*` group (pi's keys and defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRetrySettings {
    /// HTTP-level attempts after the first one (default 0).
    pub max_retries: u32,
    /// Longest server-requested wait honoured, in milliseconds; 0 = no limit (default 60000).
    pub max_retry_delay_ms: u64,
    /// The request's silence limit in milliseconds; `None` = `httpIdleTimeoutMs`.
    pub timeout_ms: Option<u64>,
}

/// The merged settings (defaults < user < project). Cheap to clone.
///
/// Consumers read values by pi's dotted key (`"compaction.reserveTokens"`, `"theme"`,
/// `"oxa.toolOutputCap"`) through the typed getters; a missing or mistyped value yields the
/// default passed by the caller. Hot groups have typed views.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    merged: Value,
    user: Value,
    project: Value,
    diagnostics: Vec<Diagnostic>,
    /// A settings file existed but could not be read or parsed (its layer is empty).
    layer_failed: bool,
}

impl Settings {
    /// Read `<agent_dir>/settings.json` and, when `project_trusted`, `<cwd>/.oxa/settings.json`;
    /// merge per pi's rules ([`crate::merge_settings`]); validate ([`crate::validate`]). Missing
    /// files are empty objects. Never fails: problems become diagnostics.
    pub fn load(dirs: &Dirs, cwd: &Path, project_trusted: bool) -> Settings {
        let mut diagnostics = Vec::new();
        let user_path = dirs.settings_file();
        let user = read_layer(&user_path, &mut diagnostics);
        let (project, project_src) = if project_trusted {
            let path = dirs.project_settings_file(cwd);
            let value = read_layer(&path, &mut diagnostics);
            (value, path.display().to_string())
        } else {
            (Value::Null, String::from("project settings"))
        };
        // `read_layer` is the only source of diagnostics so far.
        let layer_failed = !diagnostics.is_empty();
        let mut settings = build(
            user,
            project,
            diagnostics,
            &user_path.display().to_string(),
            &project_src,
        );
        settings.layer_failed = layer_failed;
        settings
    }
    /// Whether [`Settings::load`] met a file it could not read or parse. A caller that applies
    /// a changed file live keeps its previous settings then.
    pub fn layer_failed(&self) -> bool {
        self.layer_failed
    }
    /// Whether the project's own settings file declares a non-empty `done` list. Reads the file
    /// whatever the trust (it only looks at that key and never merges it), so the caller can
    /// say that the checks were skipped for an untrusted project.
    pub fn project_declares_done(dirs: &Dirs, cwd: &Path) -> bool {
        let layer = read_layer(&dirs.project_settings_file(cwd), &mut Vec::new());
        layer
            .get("done")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|c| c.as_str().is_some_and(|c| !c.trim().is_empty())))
    }
    /// Build from already-parsed values (tests, SDK).
    pub fn from_values(user: Value, project: Value) -> Settings {
        build(user, project, Vec::new(), "user settings", "project settings")
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    /// Raw merged value at a dotted key.
    /// The whole merged settings object (pi's `getSettings()` copy).
    pub fn merged(&self) -> &Value {
        &self.merged
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        let mut cur = &self.merged;
        for part in key.split('.') {
            cur = cur.as_object()?.get(part)?;
        }
        Some(cur)
    }
    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(default)
    }
    pub fn u32(&self, key: &str, default: u32) -> u32 {
        self.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
    }
    fn u64(&self, key: &str, default: u64) -> u64 {
        self.get(key).and_then(Value::as_u64).unwrap_or(default)
    }
    pub fn string(&self, key: &str) -> Option<String> {
        self.get(key).and_then(Value::as_str).map(str::to_string)
    }
    /// #202: the permission mode at start (`permissions.defaultMode`): the user's value, else
    /// `auto`. A project's value counts only when it does not allow more than that (it may
    /// lower the mode towards `standard` or `plan`, never raise it).
    pub fn default_permission_mode(&self) -> PermissionMode {
        let read = |layer: &Value| {
            layer
                .get("permissions")
                .and_then(|p| p.get("defaultMode"))
                .and_then(Value::as_str)
                .and_then(PermissionMode::parse)
        };
        let user = read(&self.user).unwrap_or(PermissionMode::DEFAULT);
        match read(&self.project) {
            Some(p) if p.rank() <= user.rank() => p,
            _ => user,
        }
    }
    /// A string list; `+x` / `-x` patch entries are already applied over the lower layer.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn compaction(&self) -> CompactionSettings {
        CompactionSettings {
            enabled: self.bool("compaction.enabled", true),
            reserve_tokens: self.u32("compaction.reserveTokens", 16384),
            keep_recent_tokens: self.u32("compaction.keepRecentTokens", 4000),
            soft_tokens: self
                .get("oxa.compaction.softTokens")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok()),
        }
    }
    pub fn retry(&self) -> RetrySettings {
        RetrySettings {
            enabled: self.bool("retry.enabled", true),
            max_retries: self.u32("retry.maxRetries", 3),
            base_delay_ms: self.u64("retry.baseDelayMs", 2000),
            max_delay_ms: self.u64("retry.maxAgentDelayMs", 60000),
        }
    }
    /// H: the `retry.provider.*` group (pi's keys and defaults). In oxa there is one HTTP
    /// layer, so these are the HTTP-level attempts inside one agent-level attempt.
    pub fn provider_retry(&self) -> ProviderRetrySettings {
        ProviderRetrySettings {
            max_retries: self.u32("retry.provider.maxRetries", 0),
            max_retry_delay_ms: self.u64("retry.provider.maxRetryDelayMs", 60000),
            timeout_ms: self.get("retry.provider.timeoutMs").and_then(Value::as_u64),
        }
    }
    /// H: `httpIdleTimeoutMs`: the silence limit of the engine stream in milliseconds; `None`
    /// when disabled (`0` or `"disabled"`). A value of another shape is the default (300000).
    pub fn http_idle_timeout_ms(&self) -> Option<u64> {
        match self.get("httpIdleTimeoutMs") {
            Some(Value::String(s)) if s == "disabled" => None,
            Some(v) => match v.as_u64() {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(300_000),
            },
            None => Some(300_000),
        }
    }
    /// H: the startup thinking level of `model` (pi's sdk.ts order after the CLI choices):
    /// `modelThinkingLevels[model]` (keyed by the engine model id; pi's `provider/modelId`
    /// form is accepted with the provider `oxe`), else `defaultThinkingLevel`, else `medium`.
    pub fn startup_thinking(&self, model: &str) -> ThinkingLevel {
        let per_model = self
            .get("modelThinkingLevels")
            .and_then(Value::as_object)
            .and_then(|m| {
                m.get(model)
                    .or_else(|| m.get(&format!("{}/{model}", crate::models::ENGINE_ENDPOINT)))
                    .and_then(Value::as_str)
                    .and_then(ThinkingLevel::parse)
            });
        per_model
            .or_else(|| {
                self.string("defaultThinkingLevel")
                    .as_deref()
                    .and_then(ThinkingLevel::parse)
            })
            .unwrap_or(ThinkingLevel::Medium)
    }
    /// H: the `sessionDir` setting as a path: `~` expands to `home`, a relative path resolves
    /// from `cwd` (pi settings.md). `None` when unset or empty.
    pub fn session_dir(&self, home: &Path, cwd: &Path) -> Option<PathBuf> {
        self.string("sessionDir")
            .filter(|s| !s.is_empty())
            .map(|s| resolve_user_path(&s, home, cwd))
    }
    /// H: `shellPath` with a leading `~` expanded (pi: `normalizePath`). `None` when unset.
    pub fn shell_path(&self, home: &Path) -> Option<PathBuf> {
        self.string("shellPath")
            .filter(|s| !s.is_empty())
            .map(|s| resolve_user_path(&s, home, Path::new("/")))
    }
    /// `done`: shell commands oxa runs itself before it reports completion; empty = off.
    pub fn done_commands(&self) -> Vec<String> {
        self.list("done")
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .collect()
    }
    /// H: `shellCommandPrefix`; `None` when unset or empty.
    pub fn shell_command_prefix(&self) -> Option<String> {
        self.string("shellCommandPrefix").filter(|s| !s.is_empty())
    }
    /// `steeringMode` (default `OneAtATime`, as pi).
    pub fn steering_mode(&self) -> QueueMode {
        queue_mode(self.string("steeringMode").as_deref())
    }
    /// `followUpMode` (default `OneAtATime`, as pi).
    pub fn follow_up_mode(&self) -> QueueMode {
        queue_mode(self.string("followUpMode").as_deref())
    }
    /// `oxa.toolOutputCap` in bytes (W4; default 16 KiB).
    pub fn tool_output_cap(&self) -> usize {
        self.u32("oxa.toolOutputCap", 16384) as usize
    }
    /// `defaultTools` after patches; `None` = oxa's default built-ins, i.e. no
    /// layer sets `defaultTools`.
    pub fn default_tools(&self) -> Option<Vec<String>> {
        if self.user.get("defaultTools").is_none() && self.project.get("defaultTools").is_none() {
            return None;
        }
        Some(self.list("defaultTools"))
    }
    /// The `+x` / `-x` entries of `defaultTools` when every layer that sets it only edits the
    /// default selection (no layer gives a plain list), user layer first; `None` otherwise.
    /// [`Settings::default_tools`] resolves such edits against the default built-ins; the tool
    /// selection uses these entries to edit the members of oxa's default selection that come
    /// from hook sets (the `subagent` tool, O-146). Within one layer an exclusion wins, as in
    /// the merge, so a `+x` of a layer that also has `-x` is left out.
    pub fn default_tools_edits(&self) -> Option<Vec<String>> {
        let layers: Vec<Vec<String>> = [&self.user, &self.project]
            .into_iter()
            .filter_map(|layer| layer.get("defaultTools"))
            .map(|v| {
                v.as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default()
            })
            .collect();
        let edit = |s: &String| s.starts_with('+') || s.starts_with('-');
        if layers.is_empty() || layers.iter().any(|l| l.is_empty() || !l.iter().all(edit)) {
            return None;
        }
        let mut out = Vec::new();
        for layer in layers {
            let excluded: Vec<&str> = layer.iter().filter_map(|e| e.strip_prefix('-')).collect();
            out.extend(
                layer
                    .iter()
                    .filter(|e| e.strip_prefix('+').is_none_or(|n| !excluded.contains(&n)))
                    .cloned(),
            );
        }
        Some(out)
    }
    /// Exec hooks: `hooks: {event: command}`.
    pub fn exec_hooks(&self) -> Vec<(String, String)> {
        self.get("hooks")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(event, cmd)| cmd.as_str().map(|c| (event.clone(), c.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// E11: `value` at `key` over every layer, in memory only (never persisted). A subagent
    /// child applies its soft compaction limit (`oxa.compaction.softTokens`) this way. A later
    /// [`Settings::set`] recomposes the layers and drops the override.
    pub fn with_override(mut self, key: &str, value: Value) -> Settings {
        if let Value::Object(root) = &mut self.merged {
            set_nested(root, key, value);
        }
        self
    }
    /// Set a key in memory and persist it to the user file (`project = false`) or the project
    /// file, keeping unrelated content and key order of that file.
    pub fn set(
        &mut self,
        dirs: &Dirs,
        cwd: &Path,
        key: &str,
        value: Value,
        project: bool,
    ) -> Result<(), OxaError> {
        if key.is_empty() || key.split('.').any(str::is_empty) {
            return Err(OxaError::new(
                codes::INVALID,
                format!("invalid setting key: {key:?}"),
            ));
        }
        if let Some(def) = find_key(key) {
            if !kind_matches(def.kind, &value) {
                return Err(OxaError::new(
                    codes::INVALID,
                    format!("setting `{key}` expects {}", kind_name(def.kind)),
                ));
            }
        }
        let path = if project {
            dirs.project_settings_file(cwd)
        } else {
            dirs.settings_file()
        };
        let mut root = read_object_for_write(&path)?;
        set_nested(&mut root, key, value);
        let mut text = serde_json::to_string_pretty(&Value::Object(root.clone()))
            .map_err(|e| OxaError::new(codes::INVALID, format!("cannot serialise settings: {e}")))?;
        text.push('\n');
        // The user file is private to the user; a project file keeps the umask default.
        write_atomic(&path, &text, if project { 0o666 } else { 0o600 })?;

        let source = path.display().to_string();
        let (checked, _) = check_root(&Value::Object(root), &source);
        if project {
            self.project = checked;
        } else {
            self.user = checked;
        }
        self.merged = compose(&self.user, &self.project);
        Ok(())
    }
}

/// Merge the layers over the built-in defaults and validate each layer. Wrongly typed values
/// are dropped before the merge, so a bad project value falls back to the user's value.
fn build(
    user: Value,
    project: Value,
    mut diagnostics: Vec<Diagnostic>,
    user_src: &str,
    project_src: &str,
) -> Settings {
    let (user, user_diags) = check_root(&user, user_src);
    let (project, project_diags) = check_root(&project, project_src);
    diagnostics.extend(user_diags);
    diagnostics.extend(project_diags);
    let merged = compose(&user, &project);
    Settings {
        merged,
        user,
        project,
        diagnostics,
        layer_failed: false,
    }
}

/// defaults < user < project. The defaults layer carries oxa's default tool list so that a
/// `+x` / `-x` patch with no explicit base patches the seven built-ins.
fn compose(user: &Value, project: &Value) -> Value {
    let defaults = defaults_layer();
    merge_settings(&merge_settings(&defaults, user), project)
}

fn defaults_layer() -> Value {
    let tools: Vec<Value> = DEFAULT_TOOLS
        .iter()
        .map(|t| Value::String((*t).to_string()))
        .collect();
    let mut m = Map::new();
    m.insert("defaultTools".to_string(), Value::Array(tools));
    Value::Object(m)
}

fn queue_mode(value: Option<&str>) -> QueueMode {
    match value {
        Some("all") => QueueMode::All,
        _ => QueueMode::OneAtATime,
    }
}

/// Read one settings file. A missing file is an empty object; an unreadable or unparsable file
/// is a diagnostic and an empty object.
fn read_layer(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Value {
    let source = path.display().to_string();
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => v,
            Err(e) => {
                diagnostics.push(warn(
                    &source,
                    format!("settings file is not valid JSON ({e}); ignored"),
                ));
                Value::Null
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => {
            diagnostics.push(warn(&source, format!("cannot read settings file: {e}")));
            Value::Null
        }
    }
}

/// The current content of a settings file as an object, for a read-modify-write.
fn read_object_for_write(path: &Path) -> Result<Map<String, Value>, OxaError> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Map::new()),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(Value::Null) => Ok(Map::new()),
            Ok(_) => Err(OxaError::new(
                codes::INVALID,
                format!("{} must contain a JSON object", path.display()),
            )
            .with_hint("Fix or remove the file, then set the value again.")),
            Err(e) => Err(OxaError::new(
                codes::INVALID,
                format!("{} is not valid JSON: {e}", path.display()),
            )
            .with_hint("Fix or remove the file, then set the value again.")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(OxaError::io(&format!("read {}", path.display()), &e)),
    }
}

/// Write `text` through a fresh, uniquely named sibling file (created exclusively with `mode`,
/// synced, then renamed over `path`), so a crash never leaves a half-written file and the write
/// never goes through a file that was already there.
fn write_atomic(path: &Path, text: &str, mode: u32) -> Result<(), OxaError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OxaError::io(&format!("create {}", parent.display()), &e))?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp-{}-{nonce}-{count}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| OxaError::io(&format!("create {}", tmp.display()), &e))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| OxaError::io(&format!("write {}", tmp.display()), &e))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| OxaError::io(&format!("replace {}", path.display()), &e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else if let Some(parent) = path.parent() {
        // Best effort: make the rename itself durable.
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }
    result
}

/// `~` and `~/rest` expand to `home`; a relative path resolves from `base`.
fn resolve_user_path(raw: &str, home: &Path, base: &Path) -> PathBuf {
    let path = if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// Set `key` (dotted) in `root`, creating or replacing intermediate objects as needed.
fn set_nested(root: &mut Map<String, Value>, key: &str, value: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cur: &mut Map<String, Value> = root;
    for part in parents {
        let slot = cur
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !slot.is_object() {
            *slot = Value::Object(Map::new());
        }
        cur = match slot {
            Value::Object(m) => m,
            _ => return,
        };
    }
    cur.insert((*last).to_string(), value);
}
