//! #1542: `oxa models [--json] [search]`, the model catalog of this install.
//!
//! The catalog is read in-process: the native local engine's configured aliases, the
//! configured cloud routes of the native gateway, or the models of the default controller
//! endpoint. It starts no subprocess (no `oxc`, no `local-ai`), prints no credential and
//! contacts no provider while catalog discovery is off.
//!
//! Exit codes: 0 listed; 1 a runtime failure (no catalog, no engine); 2 a usage error.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, OxaError};
use serde_json::json;
use std::io::Write;
use std::path::Path;

const USAGE: &str = "usage: oxa models [--json] [search]";

fn usage(message: &str) -> OxaError {
    OxaError::new(codes::USAGE, message).with_hint(USAGE)
}

/// One listed model. `provider` is the controller's remote provider, the remote model id's
/// provider prefix, or the default endpoint's name (see `dispatch::row_provider`).
struct Row {
    provider: String,
    remote: bool,
    model: String,
    context: Option<u32>,
    max_output: Option<u32>,
    thinking: Option<bool>,
    images: Option<bool>,
}

impl Row {
    /// `local`, or `cloud · <provider>`: where a request for this model runs.
    fn place(&self) -> String {
        if self.remote {
            format!("cloud · {}", self.provider)
        } else {
            "local".to_string()
        }
    }
    fn filtered(&self, needle: &str) -> bool {
        format!("{} {} {}", self.place(), self.provider, self.model)
            .to_lowercase()
            .contains(needle)
    }
}

/// Run `oxa models ..`: the words after `models`. Returns the exit code.
pub fn run_models(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let mut as_json = false;
    let mut search: Option<&str> = None;
    for word in words {
        match word.as_str() {
            "--json" => as_json = true,
            _ if !word.starts_with('-') && search.is_none() => search = Some(word),
            _ => return Err(usage(&format!("unexpected argument {word}"))),
        }
    }
    let catalog = crate::dispatch::catalog_engine(dirs, cwd, env, None, err)?;
    crate::dispatch::write_diagnostics(
        err,
        catalog
            .notices
            .into_iter()
            .map(|text| (oxa_proto::Level::Info, text)),
    );
    let info = catalog.engine.info(None)?;
    let mut ids = info.models.clone();
    if !ids.contains(&info.model) {
        ids.push(info.model.clone());
    }
    ids.sort();
    ids.dedup();
    // The model a fresh session uses without `--model`: `defaultModel` when it is one of the
    // listed models, else the engine's selected model.
    let default = Settings::load(dirs, cwd, false)
        .string("defaultModel")
        .filter(|model| ids.contains(model))
        .unwrap_or_else(|| info.model.clone());
    let needle = search.map(str::to_lowercase);
    let rows: Vec<Row> = ids
        .into_iter()
        .map(|id| {
            let model_info = if id == info.limits_for || id == info.model {
                Ok(info.clone())
            } else {
                catalog.engine.info(Some(&id))
            };
            match model_info {
                Ok(model_info) => Row {
                    provider: crate::dispatch::row_provider(&catalog.endpoint, &model_info),
                    remote: model_info.caps.remote,
                    model: id,
                    context: Some(model_info.n_ctx),
                    max_output: Some(model_info.max_output),
                    thinking: model_info.thinking,
                    images: model_info.images,
                },
                // A failing per-model call degrades only this row, as `--list-models` does.
                Err(e) => {
                    let _ = writeln!(err, "warning: cannot list {id}: {}", e.message);
                    Row {
                        provider: catalog.endpoint.clone(),
                        remote: false,
                        model: id,
                        context: None,
                        max_output: None,
                        thinking: None,
                        images: None,
                    }
                }
            }
        })
        .filter(|row| needle.as_ref().is_none_or(|n| row.filtered(n)))
        .collect();
    if as_json {
        return json_out(&rows, &default, info.queue, &catalog.endpoint, out);
    }
    table(&rows, &default, search, out)
}

/// The table: the same columns as `--list-models` plus where the model runs and whether it is
/// the default.
fn table(rows: &[Row], default: &str, search: Option<&str>, out: &mut dyn Write) -> Result<i32, OxaError> {
    if rows.is_empty() {
        return crate::dispatch::empty_list(search, out);
    }
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let unknown = |value: Option<bool>| match value {
                Some(value) => crate::dispatch::bool_word(value),
                None => "?".to_string(),
            };
            vec![
                row.place(),
                row.model.clone(),
                row.context
                    .map_or_else(|| "?".to_string(), |n| crate::dispatch::format_tokens(n as u64)),
                row.max_output
                    .map_or_else(|| "?".to_string(), |n| crate::dispatch::format_tokens(n as u64)),
                unknown(row.thinking),
                unknown(row.images),
                crate::dispatch::bool_word(row.model == default),
            ]
        })
        .collect();
    crate::dispatch::render_table(
        &[
            "where", "model", "context", "max-out", "thinking", "images", "default",
        ],
        &body,
        out,
    )?;
    Ok(0)
}

fn json_out(
    rows: &[Row],
    default: &str,
    queue: u32,
    endpoint: &str,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let models: Vec<_> = rows
        .iter()
        .map(|row| {
            json!({
                "model": row.model,
                "where": row.place(),
                "provider": row.provider,
                "remote": row.remote,
                "context_window": row.context,
                "max_output": row.max_output,
                "thinking": row.thinking,
                "images": row.images,
                "default": row.model == default,
            })
        })
        .collect();
    let document = json!({
        "endpoint": endpoint,
        "default": default,
        "queue": queue,
        "count": models.len(),
        "models": models,
    });
    writeln!(out, "{document}").map_err(|e| OxaError::io("cannot write the model list", &e))?;
    Ok(0)
}
