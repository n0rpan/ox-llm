//! `oxa models [--json]` (#1542): the models the in-process catalog serves, for people and for
//! the ox AI panel. It uses the same engine selection as `--list-models`. Listing reads no key and
//! no ledger and runs no model. `--json` prints one `oxa_models` object, or one `oxa_error`
//! object and exit 1. Plain `oxa models` prints the `--list-models` table.
use crate::dispatch::{default_endpoint, engine_for, list_models, row_provider, write_diagnostics};
use oxa_config::{Dirs, Env, ModelsConfig, Settings};
use oxa_proto::{codes, Engine, EngineInfo, Level, OxaError};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

const USAGE: &str = "Usage: oxa models [--json]";

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

/// Run `oxa models [--json]`. `words` are the words after `models`.
pub(crate) fn run(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let json_mode = match words {
        [] => false,
        [word] if word == "--json" => true,
        _ => return Err(OxaError::new(codes::USAGE, USAGE)),
    };
    let catalog = match catalog(dirs, cwd, env, err) {
        Ok(catalog) => catalog,
        Err(error) => return failed(error, json_mode, out),
    };
    if !json_mode {
        return list_models(catalog.engine.as_ref(), &catalog.provider, None, out, err);
    }
    match listing(&catalog) {
        Ok(value) => {
            write_line(out, &value)?;
            Ok(0)
        }
        Err(error) => failed(error, json_mode, out),
    }
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
    let (engine, notices) = crate::in_process::select_catalog(&user, &user, dirs, || engine_for(&endpoint))?;
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
/// A row whose own info call fails stays, with `state` `unavailable` and a `row_failed` warning.
fn listing(catalog: &Catalog) -> Result<Value, OxaError> {
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
    Ok(json!({
        "type": "oxa_models",
        "version": 1,
        "default": catalog.default,
        "lead_model": catalog.lead_model,
        "models": models,
        "warnings": warnings
    }))
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
}
