//! models.json providers and model pattern resolution.
// ported from pi packages/coding-agent/src/core/model-resolver.ts (engine-only subset)
use crate::env::{ENGINE_URL, MODELS_DEFAULT_KEY};
use crate::{Diagnostic, Dirs, Env};
use oxa_proto::{codes, Level, OxaError, ThinkingLevel};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

/// The endpoint name oxa gives the local engine (`OXA_PROVIDER` is always this).
pub(crate) const ENGINE_ENDPOINT: &str = "oxe";
/// Used when neither models.json nor `OXA_ENGINE_URL` names an endpoint: the oxc
/// controller on loopback (its OpenAI-compatible API, `/_local/status` with the per-model
/// limits and capabilities, the queue). A bare engine (`oxe-server`, default port 8080) is
/// reached by naming it in models.json or `OXA_ENGINE_URL`.
pub(crate) const DEFAULT_ENGINE_URL: &str = "http://127.0.0.1:8092";
/// The only API oxa speaks; an entry naming another API is not an engine endpoint.
const ENGINE_API: &str = "openai-completions";
/// pi's cloud provider ids. oxa has one provider type, its engine, so these are refused with a
/// diagnostic (or an `unsupported_in_oxa` error) rather than treated as endpoints.
const NON_ENGINE_PROVIDERS: &[&str] = &[
    "amazon-bedrock",
    "anthropic",
    "azure",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "deepseek",
    "fireworks",
    "github-copilot",
    "google",
    "google-vertex",
    "groq",
    "huggingface",
    "kimi-coding",
    "meta",
    "minimax",
    "minimax-cn",
    "mistral",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "openai",
    "openai-codex",
    "openrouter",
    "radius",
    "together",
    "vercel-ai-gateway",
    "xai",
    "xiaomi",
    "zai",
    "zai-coding-cn",
];

/// One engine endpoint of models.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub name: String,
    /// e.g. `http://127.0.0.1:8092`. Plain HTTP; the engine is local or on the LAN.
    pub base_url: String,
    /// Optional bearer key (literal or from the environment per models.json).
    pub api_key: Option<String>,
}

/// Compiled protocol for a config-defined HTTPS provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum CloudApi {
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
}

/// A user-owned provider entry. `key` is the name of an entry in the existing cloud key file.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudProvider {
    pub base_url: String,
    pub api: CloudApi,
    pub key: String,
    pub pool: String,
    #[serde(default)]
    pub models: Vec<CloudModel>,
}

impl fmt::Debug for CloudProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudProvider")
            .field("api", &self.api)
            .field("model_count", &self.models.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudModel {
    pub id: String,
    pub context: u32,
    pub price: Option<CloudPrice>,
}

/// Owner-configured USD rates per million tokens, in the gateway's input/output shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudPrice {
    pub input: serde_json::Number,
    pub output: serde_json::Number,
}

/// User-configured models.json providers. The default is still a local engine endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsConfig {
    pub endpoints: Vec<Endpoint>,
    /// Name of the default endpoint.
    pub default: String,
    pub cloud_providers: BTreeMap<String, CloudProvider>,
}

impl ModelsConfig {
    /// Read `<agent_dir>/models.json`; without a file the single default endpoint is
    /// `OXA_ENGINE_URL` or `http://127.0.0.1:8092` (the local engine). Entries using the
    /// cloud key or pool fields are parsed separately from the legacy engine catalog.
    ///
    /// Legacy engine entry shape in the `providers` map:
    /// `{"default": "<name>", "providers": {"<name>": {"baseUrl": "http://..", "apiKey": "..", "api": "openai-completions"}}}`.
    /// `apiKey` is a literal, or `$NAME` / `${NAME}` read from the environment. A leading `!`
    /// command is not run; it is a diagnostic.
    pub fn load(dirs: &Dirs, env: &Env) -> (ModelsConfig, Vec<Diagnostic>) {
        let engine_url = env
            .get(ENGINE_URL)
            .unwrap_or(DEFAULT_ENGINE_URL)
            .trim_end_matches('/')
            .to_string();
        let mut diags = Vec::new();
        let mut endpoints: Vec<Endpoint> = Vec::new();
        let mut cloud_providers = BTreeMap::new();
        let mut default: Option<String> = None;
        let file = dirs.models_file();
        let source = file.display().to_string();

        match std::fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => {
                    default = parse_models(
                        &value,
                        env,
                        &engine_url,
                        &source,
                        &mut endpoints,
                        &mut cloud_providers,
                        &mut diags,
                    );
                }
                Err(e) => diags.push(diag(
                    Level::Error,
                    &source,
                    format!("models.json is not valid JSON, using the default endpoint: {e}"),
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => diags.push(diag(
                Level::Warning,
                &source,
                format!("models.json cannot be read, using the default endpoint: {e}"),
            )),
        }

        if endpoints.is_empty() {
            endpoints.push(Endpoint {
                name: ENGINE_ENDPOINT.to_string(),
                base_url: engine_url,
                api_key: None,
            });
        }
        let default = match default {
            Some(name) if endpoints.iter().any(|e| e.name == name) => name,
            Some(name) => {
                diags.push(diag(
                    Level::Warning,
                    &source,
                    format!(
                        "default endpoint {name:?} is not defined; using {:?}",
                        endpoints[0].name
                    ),
                ));
                endpoints[0].name.clone()
            }
            None => endpoints[0].name.clone(),
        };
        (
            ModelsConfig {
                endpoints,
                default,
                cloud_providers,
            },
            diags,
        )
    }
}

fn diag(level: Level, source: &str, message: String) -> Diagnostic {
    Diagnostic {
        level,
        source: source.to_string(),
        message,
    }
}

/// Fill `endpoints` from the parsed file; returns the `default` key when present.
fn parse_models(
    value: &serde_json::Value,
    env: &Env,
    engine_url: &str,
    source: &str,
    endpoints: &mut Vec<Endpoint>,
    cloud_providers: &mut BTreeMap<String, CloudProvider>,
    diags: &mut Vec<Diagnostic>,
) -> Option<String> {
    let Some(root) = value.as_object() else {
        diags.push(diag(
            Level::Error,
            source,
            "models.json must be a JSON object, using the default endpoint".into(),
        ));
        return None;
    };
    if let Some(providers) = root.get("providers") {
        match providers.as_object() {
            Some(map) => {
                for (name, entry) in map {
                    if is_cloud_entry(name, entry) {
                        match parse_cloud_provider(name, entry) {
                            Some(provider) => {
                                cloud_providers.insert(name.clone(), provider);
                            }
                            None => diags.push(diag(
                                Level::Warning,
                                source,
                                "Invalid HTTPS cloud provider configuration; skipped".into(),
                            )),
                        }
                        continue;
                    }
                    if let Some(ep) = parse_endpoint(name, entry, env, engine_url, source, diags) {
                        if endpoints.iter().any(|e| e.name == ep.name) {
                            diags.push(diag(
                                Level::Warning,
                                source,
                                format!("provider {name:?} is listed twice; the first entry is used"),
                            ));
                        } else {
                            endpoints.push(ep);
                        }
                    }
                }
            }
            None => diags.push(diag(
                Level::Warning,
                source,
                "\"providers\" must be an object; ignored".into(),
            )),
        }
    }
    match root.get(MODELS_DEFAULT_KEY) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        None => None,
        Some(_) => {
            diags.push(diag(
                Level::Warning,
                source,
                "\"default\" must be a string; ignored".into(),
            ));
            None
        }
    }
}

fn is_cloud_entry(name: &str, entry: &serde_json::Value) -> bool {
    let api = entry.get("api").and_then(|v| v.as_str());
    let reserved = matches!(
        name.to_ascii_lowercase().as_str(),
        ENGINE_ENDPOINT | "anthropic" | "openai" | "openrouter" | "chatgpt" | "opencode"
    );
    !reserved && api != Some(ENGINE_API) && (entry.get("key").is_some() || entry.get("pool").is_some())
}

fn parse_cloud_provider(name: &str, entry: &serde_json::Value) -> Option<CloudProvider> {
    let provider: CloudProvider = serde_json::from_value(entry.clone()).ok()?;
    if ![name, &provider.key, &provider.pool]
        .into_iter()
        .all(valid_cloud_name)
        || !https_base(&provider.base_url)
    {
        return None;
    }
    let mut ids = BTreeSet::new();
    let valid_models = provider.models.iter().all(|model| {
        !model.id.is_empty()
            && model.id.len() <= 128
            && model
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
            && model.context > 0
            && ids.insert(model.id.as_str())
            && model.price.as_ref().is_none_or(|price| {
                [&price.input, &price.output]
                    .into_iter()
                    .all(|value| value.as_f64().is_some_and(|rate| rate.is_finite() && rate >= 0.0))
            })
    });
    valid_models.then_some(provider)
}

/// Validate a cloud provider, key-file selector or budget pool identifier.
pub fn valid_cloud_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

// Reject unsupported transport and inline credentials before retaining the user entry.
// The existing gateway URL parser validates the endpoint when constructing a route.
fn https_base(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && !base.contains(['?', '#', '\\'])
        && !base.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn parse_endpoint(
    name: &str,
    entry: &serde_json::Value,
    env: &Env,
    engine_url: &str,
    source: &str,
    diags: &mut Vec<Diagnostic>,
) -> Option<Endpoint> {
    let Some(obj) = entry.as_object() else {
        diags.push(diag(
            Level::Warning,
            source,
            format!("provider {name:?} must be an object; skipped"),
        ));
        return None;
    };
    let api = obj.get("api").and_then(|v| v.as_str());
    if is_non_engine(name)
        || (name != ENGINE_ENDPOINT && api.is_none())
        || api.is_some_and(|a| a != ENGINE_API)
    {
        diags.push(diag(Level::Warning, source, unsupported_provider_message(name)));
        return None;
    }
    let base = match obj.get("baseUrl").and_then(|v| v.as_str()) {
        Some(b) => b.trim_end_matches('/').to_string(),
        None if name == ENGINE_ENDPOINT => engine_url.to_string(),
        None => {
            diags.push(diag(
                Level::Warning,
                source,
                format!("provider {name:?} has no baseUrl; skipped"),
            ));
            return None;
        }
    };
    if base.starts_with("https://") {
        diags.push(diag(
            Level::Warning,
            source,
            format!("provider {name:?}: https is not supported, the engine is plain HTTP; skipped"),
        ));
        return None;
    }
    if !base.starts_with("http://") {
        diags.push(diag(
            Level::Warning,
            source,
            format!("provider {name:?}: baseUrl must start with http://; skipped"),
        ));
        return None;
    }
    let api_key = match obj.get("apiKey").and_then(|v| v.as_str()) {
        None => None,
        Some(raw) if raw.starts_with('!') => {
            diags.push(diag(
                Level::Warning,
                source,
                format!("provider {name:?}: apiKey commands are not run by oxa; use $NAME or a literal"),
            ));
            None
        }
        Some(raw) => match env_reference(raw) {
            Some(var) => match env.get(var) {
                Some(v) => Some(v.to_string()),
                None => {
                    diags.push(diag(
                        Level::Warning,
                        source,
                        format!("provider {name:?}: environment variable {var} is not set; no key"),
                    ));
                    None
                }
            },
            None => Some(raw.to_string()),
        },
    };
    Some(Endpoint {
        name: name.to_string(),
        base_url: base,
        api_key,
    })
}

/// `$NAME` or `${NAME}` (whole value) -> `NAME`.
fn env_reference(raw: &str) -> Option<&str> {
    let inner = if let Some(rest) = raw.strip_prefix("${") {
        rest.strip_suffix('}')?
    } else {
        raw.strip_prefix('$')?
    };
    let valid = !inner.is_empty()
        && inner
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some(inner)
}

fn is_non_engine(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    NON_ENGINE_PROVIDERS.contains(&lower.as_str())
}

fn unsupported_provider_message(name: &str) -> String {
    format!(
        "unsupported_in_oxa: provider {name:?} is not an engine endpoint; oxa talks only to its local engine. \
         Hint: give the entry a baseUrl of an oxa engine with \"api\": \"openai-completions\", or remove it."
    )
}

fn unsupported_provider(name: &str) -> OxaError {
    OxaError::new(codes::UNSUPPORTED_IN_OXA, format!("provider {name:?} is not supported in oxa"))
        .with_hint("oxa talks only to its local engine; use the default provider, or point models.json at an engine endpoint")
}

/// A model on an endpoint, optionally with a `:thinking` suffix level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub endpoint: String,
    pub model: String,
    pub thinking: Option<ThinkingLevel>,
}

/// Resolve `--model <pattern>` against the available models: exact id, then `endpoint/model`,
/// then case-insensitive substring (fuzzy), then glob; a `:level` suffix sets `thinking`.
/// `--provider` with anything but an engine endpoint name = `unsupported_in_oxa` with a hint.
/// No match = `not_found` listing the candidates.
///
/// Ties inside one step go to the first entry of `available`, so the caller orders the list
/// (the default endpoint first).
pub fn resolve_model(
    pattern: &str,
    provider: Option<&str>,
    available: &[ModelRef],
) -> Result<ModelRef, OxaError> {
    let text = pattern.trim();
    if text.is_empty() {
        return Err(OxaError::new(codes::NOT_FOUND, "empty model pattern").with_hint(list_hint(available)));
    }
    let mut scope: Option<&str> = None;
    if let Some(p) = provider {
        match find_endpoint(available, p) {
            Some(name) => scope = Some(name),
            None if is_non_engine(p) => return Err(unsupported_provider(p)),
            None => return Err(not_found(pattern, available)),
        }
    } else if let Some((head, _)) = text.split_once('/') {
        let base = split_thinking(text).map_or(text, |(b, _)| b);
        let is_id = |t: &str| available.iter().any(|m| m.model.eq_ignore_ascii_case(t));
        if is_non_engine(head) && find_endpoint(available, head).is_none() && !is_id(text) && !is_id(base) {
            return Err(unsupported_provider(head));
        }
    }
    let pool: Vec<&ModelRef> = available
        .iter()
        .filter(|m| scope.is_none_or(|s| m.endpoint == s))
        .collect();

    // The whole text first: model ids may themselves contain a colon.
    if let Some(m) = match_in(text, &pool) {
        return Ok(m.clone());
    }
    if let Some((base, level)) = split_thinking(text) {
        if let Some(m) = match_in(base, &pool) {
            return Ok(ModelRef {
                thinking: Some(level),
                ..m.clone()
            });
        }
        // `endpoint/model:level`: the endpoint prefix scopes the match.
        if let Some((head, rest)) = base.split_once('/') {
            if let Some(name) = find_endpoint(available, head) {
                let scoped: Vec<&ModelRef> = pool.iter().copied().filter(|m| m.endpoint == name).collect();
                if let Some(m) = match_in(rest, &scoped) {
                    return Ok(ModelRef {
                        thinking: Some(level),
                        ..m.clone()
                    });
                }
            }
        }
    }
    if let Some((head, rest)) = text.split_once('/') {
        if let Some(name) = find_endpoint(available, head) {
            let scoped: Vec<&ModelRef> = pool.iter().copied().filter(|m| m.endpoint == name).collect();
            if let Some(m) = match_in(rest, &scoped) {
                return Ok(m.clone());
            }
        }
    }
    Err(not_found(pattern, available))
}

/// Resolve `--models` / `enabledModels` patterns into the scoped list for Ctrl+P cycling, in
/// pattern order, without duplicates. Patterns that match nothing are returned as diagnostics.
pub fn resolve_scoped(patterns: &[String], available: &[ModelRef]) -> (Vec<ModelRef>, Vec<Diagnostic>) {
    let mut out: Vec<ModelRef> = Vec::new();
    let mut diags: Vec<Diagnostic> = Vec::new();
    let push = |out: &mut Vec<ModelRef>, m: ModelRef| {
        if !out.iter().any(|o| o.endpoint == m.endpoint && o.model == m.model) {
            out.push(m);
        }
    };
    for raw in patterns {
        let pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        if is_glob(pattern) {
            let (base, level) = match split_thinking(pattern) {
                Some((b, l)) => (b, Some(l)),
                None => (pattern, None),
            };
            let hits: Vec<&ModelRef> = available.iter().filter(|m| glob_matches(base, m)).collect();
            if hits.is_empty() {
                diags.push(diag(
                    Level::Warning,
                    "--models",
                    format!("no model matches pattern {pattern:?}"),
                ));
            }
            for m in hits {
                push(
                    &mut out,
                    ModelRef {
                        thinking: level,
                        ..m.clone()
                    },
                );
            }
            continue;
        }
        match resolve_model(pattern, None, available) {
            Ok(m) => push(&mut out, m),
            Err(e) => diags.push(diag(
                Level::Warning,
                "--models",
                format!("pattern {pattern:?}: {e}"),
            )),
        }
    }
    (out, diags)
}

fn find_endpoint<'a>(available: &'a [ModelRef], name: &str) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    available
        .iter()
        .map(|m| m.endpoint.as_str())
        .find(|e| e.to_ascii_lowercase() == lower)
}

fn split_thinking(text: &str) -> Option<(&str, ThinkingLevel)> {
    let idx = text.rfind(':')?;
    let level = ThinkingLevel::parse(&text[idx + 1..])?;
    Some((&text[..idx], level))
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// Exact id, then exact `endpoint/model`, then case-insensitive substring, then glob.
fn match_in<'a>(text: &str, pool: &[&'a ModelRef]) -> Option<&'a ModelRef> {
    let lower = text.to_ascii_lowercase();
    if let Some(m) = pool.iter().find(|m| m.model.to_ascii_lowercase() == lower) {
        return Some(m);
    }
    if let Some(m) = pool.iter().find(|m| qualified(m).to_ascii_lowercase() == lower) {
        return Some(m);
    }
    let fuzzy = pool
        .iter()
        .filter(|m| {
            m.model.to_ascii_lowercase().contains(&lower)
                || qualified(m).to_ascii_lowercase().contains(&lower)
        })
        .min_by_key(|m| m.model.len());
    if let Some(m) = fuzzy {
        return Some(m);
    }
    if is_glob(text) {
        return pool.iter().find(|m| glob_matches(text, m)).copied();
    }
    None
}

fn qualified(m: &ModelRef) -> String {
    if m.endpoint.is_empty() {
        m.model.clone()
    } else {
        format!("{}/{}", m.endpoint, m.model)
    }
}

fn glob_matches(pattern: &str, m: &ModelRef) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let id = m.model.to_ascii_lowercase();
    let q = qualified(m).to_ascii_lowercase();
    glob_match(&p, &id.chars().collect::<Vec<_>>()) || glob_match(&p, &q.chars().collect::<Vec<_>>())
}

/// `*`, `?` and `[...]` (with `!` or `^` to negate and `a-z` ranges). Input is lowercased by the caller.
fn glob_match(pat: &[char], s: &[char]) -> bool {
    match pat.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| glob_match(&pat[1..], &s[i..])),
        Some('?') => !s.is_empty() && glob_match(&pat[1..], &s[1..]),
        Some('[') => {
            let Some(end) = pat.iter().skip(2).position(|&c| c == ']').map(|i| i + 2) else {
                return !s.is_empty() && s[0] == '[' && glob_match(&pat[1..], &s[1..]);
            };
            let Some(&c) = s.first() else { return false };
            let (negate, body) = match pat[1] {
                '!' | '^' => (true, &pat[2..end]),
                _ => (false, &pat[1..end]),
            };
            let mut hit = false;
            let mut i = 0;
            while i < body.len() {
                if i + 2 < body.len() && body[i + 1] == '-' {
                    hit |= body[i] <= c && c <= body[i + 2];
                    i += 3;
                } else {
                    hit |= body[i] == c;
                    i += 1;
                }
            }
            hit != negate && glob_match(&pat[end + 1..], &s[1..])
        }
        Some(&p) => !s.is_empty() && s[0] == p && glob_match(&pat[1..], &s[1..]),
    }
}

fn list_hint(available: &[ModelRef]) -> String {
    // One endpoint: bare ids, as the picker and `--model` show them.
    let single = available.windows(2).all(|w| w[0].endpoint == w[1].endpoint);
    let names: Vec<String> = available
        .iter()
        .take(20)
        .map(|m| if single { m.model.clone() } else { qualified(m) })
        .collect();
    if names.is_empty() {
        "no models are available from the engine yet; check that it is running".to_string()
    } else {
        format!("available: {}", names.join(", "))
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The available id closest to a mistyped pattern: a substring match, or a small edit distance.
fn suggest(pattern: &str, available: &[ModelRef]) -> Option<String> {
    let p = pattern.to_lowercase();
    let single = available.windows(2).all(|w| w[0].endpoint == w[1].endpoint);
    let name = |m: &ModelRef| if single { m.model.clone() } else { qualified(m) };
    if p.len() >= 3 {
        if let Some(m) = available.iter().find(|m| m.model.to_lowercase().contains(&p)) {
            return Some(name(m));
        }
    }
    available
        .iter()
        .map(|m| {
            let id = m.model.to_lowercase();
            let tail = id.rsplit('/').next().unwrap_or(&id);
            (edit_distance(&p, &id).min(edit_distance(&p, tail)), m)
        })
        .filter(|(d, _)| *d <= 3.max(p.len() / 4))
        .min_by_key(|(d, _)| *d)
        .map(|(_, m)| name(m))
}

fn not_found(pattern: &str, available: &[ModelRef]) -> OxaError {
    OxaError::new(codes::NOT_FOUND, format!("model {pattern:?} not found")).with_hint(
        match suggest(pattern, available) {
            Some(s) => format!("did you mean {s}?\n{}", list_hint(available)),
            None => list_hint(available),
        },
    )
}
