//! E9: `mcp.json` (read, merge, expand, edit in place). Owner: task E9. Signatures below are frozen; bodies are yours.
// ported from pi packages/mcp/src/config.ts and packages/coding-agent/src/core/mcp-servers.ts
use crate::names::{pattern_matches, same_server, valid_server_name};
use crate::{
    ClientRegistration, ConfigScope, HttpConfig, McpConfig, OAuthSettings, ServerConfig, ServerKind,
    StdioConfig, CONFIG_FILE, DEFAULT_TIMEOUT_S,
};
use oxa_proto::{codes, Exposure, OxaError, Value};
use serde_json::Map;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `<agent dir>/mcp.json`.
pub fn user_config_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(crate::CONFIG_FILE)
}

/// `<cwd>/.oxa/mcp.json`.
pub fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(oxa_proto::names::PROJECT_DIR).join(crate::CONFIG_FILE)
}

const EXPOSURE_MESSAGE: &str = "exposure must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\"";
/// Fields a project entry may set when it overrides a server defined elsewhere.
const OVERRIDE_KEYS: [&str; 3] = ["enabled", "exposure", "toolExposure"];

/// Read and merge the user file and (when `Some`, i.e. the project is trusted) the project
/// file. Missing files are empty. Rules (mcp.md "Configuration rules"):
///
/// * a project entry replaces the user entry of the same name; a project entry without
///   `command`, `url` and `type` overrides only `enabled`, `exposure`, `toolExposure`;
/// * `type` is optional (`stdio`, `http`, `streamable-http`); `sse` is rejected with a
///   diagnostic that suggests the `/mcp` endpoint;
/// * names: letters, digits, `_`, `-`; names that differ only in `-` / `_` are the same server
///   (the second is a diagnostic);
/// * `codemode-deferred` is an alias of `codemode`;
/// * an invalid entry is a diagnostic and is skipped; unparsable JSON is one diagnostic and an
///   empty file.
///
/// Never fails.
pub fn load_config(user_file: &Path, project_file: Option<&Path>) -> McpConfig {
    let mut config = McpConfig::default();
    let mut servers: BTreeMap<String, ServerConfig> = BTreeMap::new();
    load_file(user_file, ConfigScope::User, &mut servers, &mut config);
    if let Some(project) = project_file {
        load_file(project, ConfigScope::Project, &mut servers, &mut config);
    }
    config.servers = servers.into_values().collect();
    config
}

fn load_file(
    path: &Path,
    scope: ConfigScope,
    servers: &mut BTreeMap<String, ServerConfig>,
    config: &mut McpConfig,
) {
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            config.diagnostics.push(format!("{shown}: cannot read: {e}"));
            return;
        }
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let doc: serde_json::Value = match serde_json::from_str(text) {
        Ok(doc) => doc,
        Err(e) => {
            config.diagnostics.push(format!("{shown}: invalid JSON: {e}"));
            return;
        }
    };
    let Some(top) = doc.as_object() else {
        config.diagnostics.push(format!(
            "{shown}: expected an object with an \"mcpServers\" object"
        ));
        return;
    };
    if let Some(flag) = top.get("autoEnableCodemode") {
        match flag.as_bool() {
            Some(flag) => config.auto_enable_codemode = Some(flag),
            None => config
                .diagnostics
                .push(format!("{shown}: autoEnableCodemode must be a boolean")),
        }
    }
    let entries = match top.get("mcpServers") {
        None => return,
        Some(serde_json::Value::Object(entries)) => entries,
        Some(_) => {
            config.diagnostics.push(format!(
                "{shown}: expected an object with an \"mcpServers\" object"
            ));
            return;
        }
    };
    for (name, entry) in entries {
        if scope == ConfigScope::Project && is_override(entry) {
            let Some(base) = servers.get_mut(name) else {
                config.diagnostics.push(format!(
                    "{shown}: server \"{name}\" needs \"command\" or \"url\", or a global server to override"
                ));
                continue;
            };
            match parse_override(name, entry) {
                Ok(over) => {
                    if let Some(enabled) = over.enabled {
                        base.enabled = enabled;
                    }
                    if let Some(exposure) = over.exposure {
                        base.exposure = exposure;
                    }
                    if let Some(tools) = over.tool_exposure {
                        base.tool_exposure = tools;
                    }
                    base.project_override = true;
                }
                Err(message) => config.diagnostics.push(format!("{shown}: {message}")),
            }
            continue;
        }
        match parse_server(name, entry, scope) {
            Err(message) => config.diagnostics.push(format!("{shown}: {message}")),
            Ok(server) => {
                // Names that differ only in `-` and `_` would share a namespace. A project entry
                // with the same name as a user entry replaces it (the loop above keeps `name`).
                if let Some(other) = servers
                    .keys()
                    .find(|other| other.as_str() != name && same_server(other, name))
                {
                    config
                        .diagnostics
                        .push(format!("{shown}: server \"{name}\" conflicts with \"{other}\""));
                    continue;
                }
                servers.insert(name.clone(), server);
            }
        }
    }
}

/// An entry without `command`, `url` or `type` overrides a server defined elsewhere.
fn is_override(entry: &serde_json::Value) -> bool {
    entry.as_object().is_some_and(|obj| {
        !obj.contains_key("command") && !obj.contains_key("url") && !obj.contains_key("type")
    })
}

/// The fields a project override may set; `None` = not given (the server keeps its value).
struct Override {
    enabled: Option<bool>,
    exposure: Option<Exposure>,
    tool_exposure: Option<Vec<(String, Exposure)>>,
}

fn parse_override(name: &str, entry: &serde_json::Value) -> Result<Override, String> {
    let obj = entry
        .as_object()
        .ok_or_else(|| format!("server \"{name}\" must be an object"))?;
    if obj.keys().any(|key| !OVERRIDE_KEYS.contains(&key.as_str())) {
        return Err(format!(
            "server \"{name}\": an override can only set {}",
            OVERRIDE_KEYS.join(", ")
        ));
    }
    Ok(Override {
        enabled: parse_enabled(name, obj.get("enabled"))?,
        exposure: match obj.get("exposure") {
            None => None,
            Some(value) => Some(
                parse_exposure_value(value)
                    .ok_or_else(|| format!("server \"{name}\": {EXPOSURE_MESSAGE}"))?,
            ),
        },
        tool_exposure: match obj.get("toolExposure") {
            None => None,
            Some(value) => Some(parse_tool_exposure(value).map_err(|m| format!("server \"{name}\": {m}"))?),
        },
    })
}

fn parse_enabled(name: &str, value: Option<&serde_json::Value>) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("server \"{name}\": enabled must be a boolean")),
    }
}

/// `codemode-deferred` is accepted as an alias of `codemode`.
fn parse_exposure_value(value: &serde_json::Value) -> Option<Exposure> {
    let text = value.as_str()?;
    if text == "codemode-deferred" {
        return Some(Exposure::Codemode);
    }
    Exposure::parse(text)
}

fn parse_tool_exposure(value: &serde_json::Value) -> Result<Vec<(String, Exposure)>, String> {
    let map = value
        .as_object()
        .ok_or("toolExposure must map tool names to exposures")?;
    map.iter()
        .map(|(tool, exposure)| {
            parse_exposure_value(exposure)
                .map(|e| (tool.clone(), e))
                .ok_or_else(|| format!("toolExposure \"{tool}\" must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\""))
        })
        .collect()
}

/// Parse one `mcpServers` entry. `Err` = the diagnostic line.
pub fn parse_server(name: &str, entry: &Value, scope: ConfigScope) -> Result<ServerConfig, String> {
    if !valid_server_name(name) {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let fail = |message: &str| format!("server \"{name}\": {message}");
    let obj = entry
        .as_object()
        .ok_or_else(|| format!("server \"{name}\" must be an object"))?;

    let enabled = parse_enabled(name, obj.get("enabled"))?.unwrap_or(true);
    let description = match obj.get("description") {
        None => None,
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(fail("description must be a string")),
    };
    let timeout_s = match obj.get("timeout") {
        None => DEFAULT_TIMEOUT_S,
        Some(value) => match value.as_f64() {
            Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
                seconds.ceil().min(f64::from(u32::MAX)) as u32
            }
            _ => return Err(fail("timeout must be a positive number of seconds")),
        },
    };
    let exposure = match obj.get("exposure") {
        None => Exposure::Codemode,
        Some(value) => parse_exposure_value(value).ok_or_else(|| fail(EXPOSURE_MESSAGE))?,
    };
    let tool_exposure = match obj.get("toolExposure") {
        None => Vec::new(),
        Some(value) => parse_tool_exposure(value).map_err(|m| fail(&m))?,
    };

    let transport = obj.get("type").and_then(serde_json::Value::as_str);
    if obj.get("type").is_some() && !matches!(transport, Some("stdio" | "http" | "streamable-http")) {
        if transport == Some("sse") {
            return Err(fail(
                "legacy SSE transport is not supported; use the streamable HTTP URL (often ending in /mcp instead of /sse)",
            ));
        }
        return Err(fail("type must be \"stdio\", \"http\" or \"streamable-http\""));
    }

    let url = obj.get("url").and_then(serde_json::Value::as_str);
    let command = obj.get("command").and_then(serde_json::Value::as_str);
    let kind = match (url, command) {
        (Some(url), _) if matches!(transport, None | Some("http" | "streamable-http")) => {
            ServerKind::Http(parse_http(obj, url).map_err(|m| fail(&m))?)
        }
        (_, Some(command)) if matches!(transport, None | Some("stdio")) => {
            ServerKind::Stdio(parse_stdio(obj, command).map_err(|m| fail(&m))?)
        }
        _ => {
            return Err(fail(
                "needs either \"command\" (stdio) or \"url\" (streamable HTTP)",
            ))
        }
    };

    // Item 4 (sec-prod): remote servers do not get their read-only hints trusted by default.
    let trust_annotations = match obj.get("trustAnnotations") {
        None => matches!(kind, ServerKind::Stdio(_)),
        Some(serde_json::Value::Bool(on)) => *on,
        Some(_) => return Err(fail("trustAnnotations must be true or false")),
    };

    Ok(ServerConfig {
        name: name.to_string(),
        kind,
        enabled,
        exposure,
        tool_exposure,
        description,
        timeout_s,
        defined_in: scope,
        project_override: false,
        trust_annotations,
    })
}

fn parse_stdio(obj: &Map<String, serde_json::Value>, command: &str) -> Result<StdioConfig, String> {
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    let args = match obj.get("args") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "args must be an array of strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("args must be an array of strings".to_string()),
    };
    let env = string_pairs(obj.get("env")).ok_or_else(|| "env must map names to strings".to_string())?;
    let cwd = match obj.get("cwd") {
        None => None,
        Some(serde_json::Value::String(dir)) => Some(dir.clone()),
        Some(_) => return Err("cwd must be a string".to_string()),
    };
    Ok(StdioConfig {
        command: command.to_string(),
        args,
        env,
        cwd,
    })
}

fn parse_http(obj: &Map<String, serde_json::Value>, url: &str) -> Result<HttpConfig, String> {
    if !is_http_url(url) {
        return Err("url must be an http or https URL".to_string());
    }
    let headers =
        string_pairs(obj.get("headers")).ok_or_else(|| "headers must map names to strings".to_string())?;
    let oauth = match obj.get("oauth") {
        None => None,
        Some(value) => Some(parse_oauth(value)?),
    };
    Ok(HttpConfig {
        url: url.to_string(),
        headers,
        oauth,
        proxy: None,
    })
}

/// `key -> string` pairs in file order; `None` when the value is present but not such an object.
fn string_pairs(value: Option<&serde_json::Value>) -> Option<Vec<(String, String)>> {
    match value {
        None => Some(Vec::new()),
        Some(serde_json::Value::Object(map)) => map
            .iter()
            .map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect(),
        Some(_) => None,
    }
}

fn parse_oauth(value: &serde_json::Value) -> Result<OAuthSettings, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "oauth must be an object".to_string())?;
    let text = |key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            None => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("oauth.{key} must be a string")),
        }
    };
    let client_id = text("clientId")?;
    let client_secret = text("clientSecret")?;
    let scope = text("scope")?;
    let client_name = text("clientName")?;
    if client_name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err("oauth.clientName must be a non-empty string".to_string());
    }
    let callback_port = match obj.get("callbackPort") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|p| (1..=65535).contains(p))
                .map(|p| p as u16)
                .ok_or_else(|| "oauth.callbackPort must be a port number".to_string())?,
        ),
    };
    let callback_url = text("callbackUrl")?;
    if let Some(url) = &callback_url {
        let parts = loopback_parts(url).ok_or_else(|| {
            "oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment".to_string()
        })?;
        if let (Some(url_port), Some(port)) = (parts.port, callback_port) {
            if url_port != port {
                return Err("oauth.callbackUrl and oauth.callbackPort name different ports".to_string());
            }
        }
    }
    let client_registration = match obj.get("clientRegistration") {
        None => ClientRegistration::Auto,
        Some(serde_json::Value::String(mode)) => match mode.as_str() {
            "auto" => ClientRegistration::Auto,
            "dcr" => ClientRegistration::Dcr,
            "cimd" => {
                if client_id.is_some() || client_name.is_some() {
                    return Err("oauth.clientRegistration \"cimd\" cannot be combined with oauth.clientId or oauth.clientName".to_string());
                }
                // A document's redirect URI must be `/callback` on localhost or 127.0.0.1.
                if let Some(url) = &callback_url {
                    let document_ok = matches!(
                        loopback_parts(url),
                        Some(parts) if parts.host != "[::1]" && parts.path == "/callback"
                    );
                    if !document_ok {
                        return Err("oauth.clientRegistration \"cimd\" requires oauth.callbackUrl on localhost or 127.0.0.1 with path /callback".to_string());
                    }
                }
                ClientRegistration::Cimd
            }
            _ => return Err("oauth.clientRegistration must be \"auto\", \"dcr\", or \"cimd\"".to_string()),
        },
        Some(_) => return Err("oauth.clientRegistration must be \"auto\", \"dcr\", or \"cimd\"".to_string()),
    };
    let auth_server_metadata_url = text("authServerMetadataUrl")?;
    if let Some(url) = &auth_server_metadata_url {
        let ok = url.starts_with("https://") || (url.starts_with("http://") && is_loopback_url(url));
        if !ok {
            return Err(
                "oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]"
                    .to_string(),
            );
        }
    }
    Ok(OAuthSettings {
        client_id,
        client_secret,
        callback_port,
        callback_url,
        scope,
        client_name,
        client_registration,
        auth_server_metadata_url,
    })
}

/// `scheme://host[:port]/path` with an http or https scheme and a host.
fn is_http_url(url: &str) -> bool {
    let rest = match url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        Some(rest) => rest,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty() && !url.chars().any(char::is_whitespace)
}

struct LoopbackParts<'a> {
    host: &'a str,
    port: Option<u16>,
    path: &'a str,
}

/// An `http` URI on a loopback host without query or fragment, split into its parts.
fn loopback_parts(url: &str) -> Option<LoopbackParts<'_>> {
    let rest = url.strip_prefix("http://")?;
    if rest.contains(['?', '#']) {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        // IPv6 literal: `[::1]` or `[::1]:port`.
        let host = &authority[..end + 2];
        let after = &authority[end + 2..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
            None => (authority, None),
        }
    };
    if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        return None;
    }
    Some(LoopbackParts { host, port, path })
}

/// `http://` URL whose host is `localhost`, `127.0.0.1` or `[::1]` (any port, path or query).
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if authority.starts_with("[::1]") {
        "[::1]"
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Expand one `env` / `headers` value: `${VAR}` anywhere in the value (unset = empty string),
/// or, when the whole value starts with `!`, the trimmed stdout of that shell command (`run`).
/// `lookup` reads the environment; both are injected so tests are hermetic.
pub fn expand_value(
    raw: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    run: &dyn Fn(&str) -> Result<String, OxaError>,
) -> Result<String, OxaError> {
    if let Some(command) = raw.strip_prefix('!') {
        return run(command.trim()).map(|stdout| stdout.trim().to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unclosed `${` stays as written.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        if name.is_empty() {
            out.push_str("${}");
        } else {
            out.push_str(&lookup(name).unwrap_or_default());
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The effective exposure of one tool: an exact `toolExposure` name wins over patterns; among
/// patterns (`*` matches any characters) the first match wins; else the server's exposure.
pub fn effective_exposure(server: &ServerConfig, tool: &str) -> Exposure {
    let exact = server
        .tool_exposure
        .iter()
        .find(|(key, _)| !key.contains('*') && key == tool);
    if let Some((_, exposure)) = exact {
        return *exposure;
    }
    let pattern = server
        .tool_exposure
        .iter()
        .find(|(key, _)| key.contains('*') && pattern_matches(key, tool));
    match pattern {
        Some((_, exposure)) => *exposure,
        None => server.exposure,
    }
}

/// Add or replace the entry `name` in `file` (created with its parent directory when missing),
/// keeping every unrelated key, entry and their order. Written atomically (temp file + rename).
pub fn add_server(file: &Path, name: &str, entry: &Value) -> Result<(), OxaError> {
    let (mut doc, indent) = read_doc(file)?;
    servers_of(&mut doc, file)?.insert(name.to_string(), entry.clone());
    write_doc(file, &doc, &indent)
}

/// Remove the entry `name`. `Ok(false)` when it was not there. Unrelated content is kept.
pub fn remove_server(file: &Path, name: &str) -> Result<bool, OxaError> {
    if !file.exists() {
        return Ok(false);
    }
    let (mut doc, indent) = read_doc(file)?;
    let servers = servers_of(&mut doc, file)?;
    if servers.shift_remove(name).is_none() {
        return Ok(false);
    }
    write_doc(file, &doc, &indent)?;
    Ok(true)
}

/// Set one field (`enabled`, `exposure`, `toolExposure`, ..) of the entry `name` in `file`,
/// creating a bare override entry when the file does not have the server (project override of
/// a user-level server). Unrelated content is kept.
pub fn set_server_field(file: &Path, name: &str, key: &str, value: &Value) -> Result<(), OxaError> {
    let (mut doc, indent) = read_doc(file)?;
    let servers = servers_of(&mut doc, file)?;
    if !servers.contains_key(name) {
        let mut bare = Map::new();
        bare.insert(key.to_string(), value.clone());
        servers.insert(name.to_string(), Value::Object(bare));
    } else if let Some(Value::Object(entry)) = servers.get_mut(name) {
        entry.insert(key.to_string(), value.clone());
    } else {
        return Err(invalid(file, &format!("server \"{name}\" must be an object")));
    }
    write_doc(file, &doc, &indent)
}

/// Build the JSON entry for `oxa mcp add` from its options (cli.md "MCP commands").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddOptions {
    /// `-- <command> [args...]`.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    /// `--bearer-token-env-var NAME` -> `Authorization: Bearer ${NAME}`.
    pub bearer_token_env_var: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_callback_port: Option<u16>,
    pub oauth_client_registration: Option<String>,
    pub exposure: Option<String>,
    pub description: Option<String>,
}

/// Validate the options (exactly one of command / url; option combinations as cli.md) and
/// build the entry. Errors: `usage`.
pub fn entry_from_add_options(name: &str, o: &AddOptions) -> Result<Value, OxaError> {
    let usage = |message: &str| OxaError::new(codes::USAGE, message);
    if !valid_server_name(name) {
        return Err(usage(&format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        )));
    }
    let has_oauth = o.oauth_client_id.is_some()
        || o.oauth_client_secret.is_some()
        || o.oauth_callback_port.is_some()
        || o.oauth_client_registration.is_some();
    let mut entry = Map::new();
    match (o.command.is_empty(), o.url.as_deref()) {
        (false, None) => {
            if !o.headers.is_empty() || o.bearer_token_env_var.is_some() || has_oauth {
                return Err(usage(
                    "--header, --bearer-token-env-var and --oauth-* apply to --url servers only",
                ));
            }
            entry.insert("command".into(), Value::String(o.command[0].clone()));
            if o.command.len() > 1 {
                entry.insert(
                    "args".into(),
                    Value::Array(o.command[1..].iter().cloned().map(Value::String).collect()),
                );
            }
            if !o.env.is_empty() {
                entry.insert("env".into(), pairs_object(&o.env));
            }
            if let Some(cwd) = &o.cwd {
                entry.insert("cwd".into(), Value::String(cwd.clone()));
            }
        }
        (true, Some(url)) => {
            if o.cwd.is_some() || !o.env.is_empty() {
                return Err(usage("--env and --cwd apply to stdio servers only"));
            }
            entry.insert("url".into(), Value::String(url.to_string()));
            let mut headers = o.headers.clone();
            if let Some(var) = &o.bearer_token_env_var {
                if var.is_empty() {
                    return Err(usage("--bearer-token-env-var needs a variable name"));
                }
                if headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                {
                    return Err(usage(
                        "--bearer-token-env-var conflicts with an Authorization header",
                    ));
                }
                headers.push(("Authorization".into(), format!("Bearer ${{{var}}}")));
            }
            if !headers.is_empty() {
                entry.insert("headers".into(), pairs_object(&headers));
            }
            if has_oauth {
                entry.insert("oauth".into(), oauth_object(o, &usage)?);
            }
        }
        (false, Some(_)) => return Err(usage("give either a command after -- or --url, not both")),
        (true, None) => return Err(usage("missing a command after -- or a --url")),
    }
    if let Some(exposure) = &o.exposure {
        let parsed = parse_exposure_value(&Value::String(exposure.clone()))
            .ok_or_else(|| usage(&format!("unknown exposure \"{exposure}\"")))?;
        entry.insert("exposure".into(), Value::String(parsed.as_str().to_string()));
    }
    if let Some(description) = &o.description {
        entry.insert("description".into(), Value::String(description.clone()));
    }
    let entry = Value::Object(entry);
    // The same validation `load_config` applies, so `add` never writes an entry the loader rejects.
    parse_server(name, &entry, ConfigScope::User).map_err(|m| usage(&m))?;
    Ok(entry)
}

fn pairs_object(pairs: &[(String, String)]) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(map)
}

fn oauth_object(o: &AddOptions, usage: &dyn Fn(&str) -> OxaError) -> Result<Value, OxaError> {
    let mut map = Map::new();
    if let Some(id) = &o.oauth_client_id {
        map.insert("clientId".into(), Value::String(id.clone()));
    }
    if let Some(secret) = &o.oauth_client_secret {
        map.insert("clientSecret".into(), Value::String(secret.clone()));
    }
    if let Some(port) = o.oauth_callback_port {
        if port == 0 {
            return Err(usage("--oauth-callback-port must be a port number"));
        }
        map.insert("callbackPort".into(), Value::from(port));
    }
    if let Some(mode) = &o.oauth_client_registration {
        if !matches!(mode.as_str(), "auto" | "cimd" | "dcr") {
            return Err(usage("--oauth-client-registration must be auto, cimd or dcr"));
        }
        map.insert("clientRegistration".into(), Value::String(mode.clone()));
    }
    Ok(Value::Object(map))
}

// ---- file editing helpers ---------------------------------------------------------------

fn invalid(file: &Path, message: &str) -> OxaError {
    OxaError::new(codes::INVALID, format!("{}: {message}", file.display()))
        .with_hint("fix the file by hand; oxa has not changed it")
}

/// The file as a JSON object whose `mcpServers` is an object (created when missing), plus the
/// indentation to write it back with. A missing file is an empty document.
fn read_doc(file: &Path) -> Result<(Map<String, Value>, String), OxaError> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Map::new(), "  ".to_string())),
        Err(e) => return Err(OxaError::io(&format!("read {}", file.display()), &e)),
    };
    let indent = indentation_of(&text);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| invalid(file, &format!("invalid JSON: {e}")))?;
    let Value::Object(doc) = parsed else {
        return Err(invalid(file, "expected an object with an \"mcpServers\" object"));
    };
    Ok((doc, indent))
}

/// The leading whitespace of the first indented line, `"  "` when there is none.
fn indentation_of(text: &str) -> String {
    text.lines()
        .find(|line| line.starts_with([' ', '\t']) && !line.trim().is_empty())
        .map(|line| {
            line.chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect::<String>()
        })
        .filter(|indent| !indent.is_empty())
        .unwrap_or_else(|| "  ".to_string())
}

/// The `mcpServers` object of `doc`, created at the end when missing.
fn servers_of<'a>(
    doc: &'a mut Map<String, Value>,
    file: &Path,
) -> Result<&'a mut Map<String, Value>, OxaError> {
    doc.entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    doc.get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid(file, "expected an object with an \"mcpServers\" object"))
}

/// Serialize with the file's indentation (pretty output indents by two spaces per level, so
/// each leading run is rescaled), then write atomically: a temp file in the same directory,
/// synced, renamed over the target. Created files are 0600 (they may hold credentials); an
/// existing file keeps its mode.
fn write_doc(file: &Path, doc: &Map<String, Value>, indent: &str) -> Result<(), OxaError> {
    let pretty = serde_json::to_string_pretty(&Value::Object(doc.clone()))
        .map_err(|e| OxaError::new(codes::INTERNAL, format!("serialize {}: {e}", file.display())))?;
    let mut text = String::with_capacity(pretty.len() + 1);
    for line in pretty.lines() {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        for _ in 0..spaces / 2 {
            text.push_str(indent);
        }
        text.push_str(&line[spaces..]);
        text.push('\n');
    }
    write_atomic(file, text.as_bytes())
}

fn write_atomic(file: &Path, bytes: &[u8]) -> Result<(), OxaError> {
    let shown = file.display().to_string();
    if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| OxaError::io(&format!("create {}", parent.display()), &e))?;
    }
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| CONFIG_FILE.to_string());
    let tmp = file.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mode = std::fs::metadata(file)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let result = (|| -> std::io::Result<()> {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        drop(out);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, file)
    })();
    result.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        OxaError::io(&format!("write {shown}"), &e)
    })
}
