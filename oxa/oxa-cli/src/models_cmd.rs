//! `oxa models [--json] [search]` (#1542): the model catalog of this install, for people and
//! for the ox AI panel. The listing is the same engine selection as `--list-models`: the
//! native local aliases, the configured and recommended cloud routes of the in-process
//! gateway, or the default endpoint's engine. It reads no key and no ledger, starts no
//! subprocess and runs no model. `--json` prints one `oxa_models` object, or one `oxa_error`
//! object and exit 1. Plain `oxa models` prints the catalog table: where each model runs,
//! its limits and which one is the default. A search word filters both outputs by place,
//! provider or model name.
use crate::dispatch::{
    bool_word, default_endpoint, empty_list, engine_for, format_tokens, render_table, row_provider,
    write_diagnostics,
};
use oxa_config::{Dirs, Env, ModelsConfig, Settings};
use oxa_proto::{codes, Engine, EngineInfo, Level, OxaError};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

const USAGE: &str = "usage: oxa models [--json] [search]";

/// The engine that lists the models, and the settings the listing reports.
struct Catalog {
    engine: Arc<dyn Engine>,
    /// The name of the default endpoint: the provider of a model that publishes none.
    provider: String,
    /// `defaultModel`, when set.
    default: Option<String>,
    /// `oxa.lead.model` (user-only), when set.
    lead_model: Option<String>,
    /// Backend identity from the configured route, retained even when that backend cannot answer.
    routes: BTreeMap<String, (bool, String)>,
}

/// One row of the plain table. `place` says where a request for this model runs: `local`, or
/// `cloud · <provider>`.
struct Row {
    place: String,
    model: String,
    context: Option<u32>,
    max_output: Option<u32>,
    thinking: Option<bool>,
    images: Option<bool>,
    default: bool,
}

impl Row {
    fn matches(&self, needle: &str) -> bool {
        format!("{} {}", self.place, self.model)
            .to_lowercase()
            .contains(needle)
    }
}

/// The words after `models`: an optional `--json` and an optional search word, in either
/// order. Anything else is a usage error, never a prompt.
fn parse_words(words: &[String]) -> Result<(bool, Option<String>), OxaError> {
    let mut json_mode = false;
    let mut search: Option<String> = None;
    for word in words {
        match word.as_str() {
            "--json" => json_mode = true,
            _ if !word.starts_with('-') && search.is_none() => search = Some(word.clone()),
            _ => {
                return Err(
                    OxaError::new(codes::USAGE, format!("unexpected argument {word}")).with_hint(USAGE)
                )
            }
        }
    }
    Ok((json_mode, search))
}

/// Run `oxa models [--json] [search]`. `words` are the words after `models`.
pub(crate) fn run(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let (json_mode, search) = parse_words(words)?;
    let catalog = match catalog(dirs, cwd, env, err) {
        Ok(catalog) => catalog,
        Err(error) => return failed(error, json_mode, out),
    };
    let needle = search.as_deref().map(str::to_lowercase);
    let needle = needle.as_deref();
    if json_mode {
        return match listing(&catalog, needle) {
            Ok(value) => {
                write_line(out, &value)?;
                Ok(0)
            }
            Err(error) => failed(error, json_mode, out),
        };
    }
    table(&catalog, needle, out, err)
}

/// The engine of `--list-models`: the same settings, the same selection and the same diagnostics.
fn catalog(dirs: &Dirs, cwd: &Path, env: &Env, err: &mut dyn Write) -> Result<Catalog, OxaError> {
    let (models, diags) = ModelsConfig::load(dirs, env);
    write_diagnostics(
        err,
        diags
            .iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    let endpoint = default_endpoint(&models)?;
    let user = Settings::load(dirs, cwd, false);
    let mut routes = BTreeMap::new();
    if let Some(local) = user.get("oxa.local.models").and_then(Value::as_object) {
        routes.extend(
            local
                .keys()
                .map(|id| (id.clone(), (false, endpoint.name.clone()))),
        );
    }
    if let Some(cloud) = user.get("oxa.cloud.models").and_then(Value::as_object) {
        routes.extend(cloud.iter().filter_map(|(id, route)| {
            route["provider"]
                .as_str()
                .map(|provider| (id.clone(), (true, provider.to_owned())))
        }));
    }
    let (engine, notices) = crate::in_process::select_catalog(&user, dirs, || engine_for(&endpoint))?;
    write_diagnostics(err, notices.into_iter().map(|text| (Level::Info, text)));
    Ok(Catalog {
        engine,
        provider: endpoint.name.clone(),
        default: user.string("defaultModel"),
        lead_model: user.string("oxa.lead.model"),
        routes,
    })
}

/// The `oxa_models` object: one row per model, sorted by id, as `--list-models` orders them.
/// A row whose own info call fails stays, with `state` `unavailable` and a `row_failed`
/// warning. `needle`, when a search word was given, filters the rows (and their warnings) by
/// kind, provider or id.
fn listing(catalog: &Catalog, needle: Option<&str>) -> Result<Value, OxaError> {
    let base = catalog.engine.info(None)?;
    let mut ids = base.models.clone();
    if !ids.contains(&base.model) {
        ids.push(base.model.clone());
    }
    ids.sort();
    ids.dedup();
    let mut models = Vec::with_capacity(ids.len());
    let mut warnings = Vec::new();
    for id in ids {
        let info = if id == base.limits_for {
            Ok(base.clone())
        } else {
            catalog.engine.info(Some(&id))
        };
        match info {
            Ok(info) => models.push(model_row(&id, &info, &catalog.provider)),
            Err(_) => {
                warnings.push(json!({"code": "row_failed", "id": id}));
                models.push(unavailable_row(&id, catalog, &base));
            }
        }
    }
    if let Some(needle) = needle.map(str::to_lowercase) {
        let matches = |row: &Value| {
            format!(
                "{} {} {}",
                row["kind"].as_str().unwrap_or_default(),
                row["provider"].as_str().unwrap_or_default(),
                row["id"].as_str().unwrap_or_default()
            )
            .to_lowercase()
            .contains(&needle)
        };
        models.retain(matches);
        warnings.retain(|warning| {
            warning["id"]
                .as_str()
                .is_some_and(|id| models.iter().any(|row| row["id"] == id))
        });
    }
    Ok(json!({
        "type": "oxa_models",
        "version": 1,
        "default": catalog.default,
        "lead_model": catalog.lead_model,
        "models": models,
        "warnings": warnings
    }))
}

/// The catalog table: the `--list-models` limits plus where each model runs and which one is
/// the default. A failing per-model info call degrades only that row (the backend identity
/// stays, from the configured route) and the listing continues.
fn table(
    catalog: &Catalog,
    needle: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let base = catalog.engine.info(None)?;
    let mut ids = base.models.clone();
    if !ids.contains(&base.model) {
        ids.push(base.model.clone());
    }
    ids.sort();
    ids.dedup();
    // The model a fresh session uses without `--model`: `defaultModel` when it is one of the
    // listed models, else the engine's selected model.
    let default = catalog
        .default
        .clone()
        .filter(|model| ids.contains(model))
        .unwrap_or_else(|| base.model.clone());
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let info = if id == base.limits_for {
            Ok(base.clone())
        } else {
            catalog.engine.info(Some(&id))
        };
        let (limits, cloud, provider) = match info {
            Ok(info) => (
                (
                    Some(info.n_ctx),
                    Some(info.max_output),
                    info.thinking,
                    info.images,
                ),
                info.caps.remote,
                row_provider(&catalog.provider, &info),
            ),
            Err(e) => {
                let _ = writeln!(err, "warning: cannot list {id}: {}", e.message);
                let (cloud, provider) = catalog
                    .routes
                    .get(&id)
                    .cloned()
                    .unwrap_or((base.caps.remote, catalog.provider.clone()));
                ((None, None, None, None), cloud, provider)
            }
        };
        let (context, max_output, thinking, images) = limits;
        let is_default = default == id;
        rows.push(Row {
            place: if cloud {
                format!("cloud · {provider}")
            } else {
                "local".to_string()
            },
            model: id,
            context,
            max_output,
            thinking,
            images,
            default: is_default,
        });
    }
    if let Some(needle) = needle.map(str::to_lowercase) {
        rows.retain(|row| row.matches(&needle));
    }
    if rows.is_empty() {
        return empty_list(needle, out);
    }
    let unknown = |value: Option<bool>| match value {
        Some(value) => bool_word(value),
        None => "?".to_string(),
    };
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.place.clone(),
                row.model.clone(),
                row.context
                    .map_or_else(|| "?".to_string(), |n| format_tokens(n as u64)),
                row.max_output
                    .map_or_else(|| "?".to_string(), |n| format_tokens(n as u64)),
                unknown(row.thinking),
                unknown(row.images),
                bool_word(row.default),
            ]
        })
        .collect();
    render_table(
        &[
            "where", "model", "context", "max-out", "thinking", "images", "default",
        ],
        &body,
        out,
    )?;
    Ok(0)
}

fn unavailable_row(id: &str, catalog: &Catalog, base: &EngineInfo) -> Value {
    let (cloud, provider) = catalog
        .routes
        .get(id)
        .cloned()
        .unwrap_or_else(|| (base.caps.remote, catalog.provider.clone()));
    json!({
        "id": id,
        "kind": if cloud { "cloud" } else { "local" },
        "provider": provider,
        "billing": billing(cloud, &provider),
        "pool": billing(cloud, &provider),
        "state": "unavailable",
        "source": "configured",
        "context": null,
        "max_output": null,
        "thinking": null,
        "images": null,
        "fix": null
    })
}

/// One ready row. `pool` is the usage pool the model draws from (`metered`, `subscription` or
/// `local`), so the panel can join it with `oxa usage --json`.
fn model_row(id: &str, info: &EngineInfo, endpoint: &str) -> Value {
    let provider = row_provider(endpoint, info);
    let cloud = info.caps.remote;
    let pool = billing(cloud, &provider);
    json!({
        "id": id,
        "kind": if cloud { "cloud" } else { "local" },
        "provider": provider,
        "billing": pool,
        "pool": pool,
        "state": "ready",
        "source": "configured",
        "context": info.n_ctx,
        "max_output": info.max_output,
        "thinking": info.thinking,
        "images": info.images,
        "fix": null
    })
}

/// `subscription` for the plan routes (ChatGPT, OpenCode Go), `local` for an engine that is not
/// remote, `metered` for every other cloud route.
fn billing(cloud: bool, provider: &str) -> &'static str {
    if !cloud {
        "local"
    } else if matches!(provider, "chatgpt" | "opencode") {
        "subscription"
    } else {
        "metered"
    }
}

/// A failure. With `--json` it is one `oxa_error` object on stdout and exit 1; otherwise the
/// error goes to the caller, which prints it.
fn failed(error: OxaError, json_mode: bool, out: &mut dyn Write) -> Result<i32, OxaError> {
    if !json_mode {
        return Err(error);
    }
    write_line(
        out,
        &json!({"type": "oxa_error", "version": 1, "command": "models", "code": error.code}),
    )?;
    Ok(1)
}

fn write_line(out: &mut dyn Write, value: &Value) -> Result<(), OxaError> {
    writeln!(out, "{value}").map_err(|e| OxaError::io("cannot write the model list", &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture of the panel contract: the keys of one `oxa_models` row and of the object.
    const FIXTURE: &str = include_str!("../../omarchy-panel/tests/fixtures/models.json");

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        keys
    }

    fn word(text: &str) -> String {
        text.to_string()
    }

    #[test]
    fn models_json_matches_fixture_shape() {
        let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
        let info = EngineInfo {
            base_url: String::new(),
            models: vec!["openai/gpt-6.2-sol".to_string()],
            model: "openai/gpt-6.2-sol".to_string(),
            limits_for: "openai/gpt-6.2-sol".to_string(),
            n_ctx: 400_000,
            max_output: 128_000,
            queue: 0,
            compact_at: None,
            prefill_only: false,
            caps: oxa_proto::EngineCaps {
                remote: true,
                remote_provider: Some("openai"),
                ..oxa_proto::EngineCaps::default()
            },
            thinking: Some(true),
            images: Some(true),
        };
        let row = model_row("openai/gpt-6.2-sol", &info, "oxe");
        assert_eq!(
            keys(&row),
            keys(&fixture["models"][0]),
            "row keys match the fixture"
        );
        assert_eq!(row["billing"], "metered");
        assert_eq!(row["pool"], "metered");
        assert_eq!(row["state"], "ready");
        assert_eq!(row["kind"], "cloud");
        assert_eq!(row["provider"], "openai");
        assert_eq!(row["context"], 400_000);
    }

    #[test]
    fn billing_names_the_usage_pool() {
        assert_eq!(billing(false, "oxe"), "local");
        assert_eq!(billing(true, "chatgpt"), "subscription");
        assert_eq!(billing(true, "opencode"), "subscription");
        assert_eq!(billing(true, "anthropic"), "metered");
    }

    #[test]
    fn unavailable_row_keeps_its_configured_backend() {
        let mut routes = BTreeMap::new();
        routes.insert("cloud-down".into(), (true, "anthropic".into()));
        let catalog = Catalog {
            engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
            provider: "oxe".into(),
            default: None,
            lead_model: None,
            routes,
        };
        let base = catalog.engine.info(None).unwrap();
        assert!(!base.caps.remote, "the usable base row is local");
        let row = unavailable_row("cloud-down", &catalog, &base);
        assert_eq!(row["kind"], "cloud");
        assert_eq!(row["provider"], "anthropic");
        assert_eq!(row["billing"], "metered");
    }

    #[test]
    fn failed_json_is_one_oxa_error_object() {
        let mut out: Vec<u8> = Vec::new();
        let code = failed(OxaError::new(codes::USAGE, "bad"), true, &mut out).expect("json failure");
        assert_eq!(code, 1);
        let value: Value = serde_json::from_slice(&out).expect("one JSON line");
        assert_eq!(value["type"], "oxa_error");
        assert_eq!(value["command"], "models");
        assert_eq!(value["code"], codes::USAGE);
    }

    /// A bare word is a search, `--json` is a flag in either order, and anything else is a
    /// usage error with the command's usage line.
    #[test]
    fn words_parse_into_json_flag_and_search() {
        assert_eq!(parse_words(&[]).unwrap(), (false, None));
        assert_eq!(parse_words(&[word("--json")]).unwrap(), (true, None));
        assert_eq!(
            parse_words(&[word("ANTHROPIC")]).unwrap(),
            (false, Some(word("ANTHROPIC")))
        );
        assert_eq!(
            parse_words(&[word("anthropic"), word("--json")]).unwrap(),
            (true, Some(word("anthropic")))
        );
        for junk in [vec![word("--frobnicate")], vec![word("local"), word("cloud")]] {
            let error = parse_words(&junk).unwrap_err();
            assert_eq!(error.code, codes::USAGE);
            assert!(error.message.contains("unexpected argument"), "{}", error.message);
            assert!(
                error
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("oxa models")),
                "hint: {:?}",
                error.hint
            );
        }
    }
}
