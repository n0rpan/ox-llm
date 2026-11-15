//! Cloud-only listener. This runtime never constructs a local engine manager.
use crate::{cloud_catalog as catalog, http_gate, paths, transport};
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use oxc_gateway::{catalog::Catalog, policy};
use serde_json::{json, Value};
use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};

struct App {
    port: u16,
    config: Mutex<Value>,
    preload: Mutex<Option<Value>>,
}

fn discovery(config: &Value) -> Catalog {
    // Engine mode is selected at startup. A configuration refresh cannot
    // construct a local engine or change this runtime into a local controller.
    let mut config = config.clone();
    config["cloud_only"] = json!(true);
    Catalog::from_config(&config, catalog::offline()).expect("cloud-only metadata")
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path }.to_owned();
    let Ok(id) = http_gate::random_id() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut context = http_gate::Context::<()> {
        path: path.clone(),
        head: method == Method::HEAD,
        id,
        cors: None,
        started: std::time::Instant::now(),
        remote: None,
    };
    if let Some(response) = http_gate::target(app.port, &request, &context) {
        return response;
    }
    let config = {
        let mut last = app.config.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fresh) = paths::read_config() {
            *last = fresh;
        }
        last.clone()
    };
    if let Some(response) = http_gate::authorize(
        &mut context,
        app.port,
        request.headers(),
        method.clone(),
        request.uri().query().is_some_and(|q| !q.is_empty()),
        &config,
    ) {
        return response;
    }
    if [Method::GET, Method::HEAD].contains(&method) {
        if ["/models", "/v1/models"].contains(&path.as_str()) {
            return context.reply(200, discovery(&config).models());
        }
        if let Some((_, wanted)) = path.split_once("/models/") {
            if let Some(item) = catalog::visible(&config)
                .iter()
                .find(|item| item["id"] == wanted)
                .cloned()
            {
                return context.reply(200, catalog::row(&item));
            }
            return context.error(
                404,
                "Unknown local model. Choose one listed in /v1/models.",
                "invalid_request_error",
                "model_not_found",
                Some("model"),
            );
        }
        if path == "/health" {
            return context.reply(200, discovery(&config).health());
        }
        if path == "/_local/status" {
            let mut status = catalog::status(&config, app.port);
            status["model_preload"] = app
                .preload
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or(Value::Null);
            return context.reply(200, status);
        }
        if path == "/_local/models" {
            return context.reply(200, catalog::catalog(&config));
        }
        if path == "/_local/ledger" {
            return context.reply(200, crate::request_ledger::recent(50));
        }
    }
    if method == Method::POST
        && ["/_local/load", "/_local/preload", "/_local/unload"].contains(&path.as_str())
    {
        let headers = request.headers();
        let length = headers
            .get("content-length")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("0")
            .trim()
            .parse::<i64>();
        let Ok(length) = length else {
            return context.error(400, "Invalid content length.", "", "", None);
        };
        if headers.contains_key("transfer-encoding") || !(0..=1024).contains(&length) {
            return context.error(413, "Control request is too large.", "", "", None);
        }
        let control = match axum::body::to_bytes(request.into_body(), 1024).await {
            Ok(data) if data.is_empty() => json!({}),
            Ok(data) => match serde_json::from_slice::<Value>(&data) {
                Ok(value) if value.is_object() => value,
                _ => return context.error(400, "Invalid control request.", "", "", None),
            },
            Err(_) => return context.error(400, "Invalid control request.", "", "", None),
        };
        if path == "/_local/load" {
            return context.error(503, catalog::local_refusal(&config), "", "", None);
        }
        if path == "/_local/unload" {
            return if !control["model"].is_null() {
                context.reply(409, json!({"error":"That model is not loaded."}))
            } else {
                context.reply(200, json!({"state":"unloaded"}))
            };
        }
        let alias = control["model"]
            .as_str()
            .or_else(|| config["selected_model"].as_str())
            .or_else(|| config["model_alias"].as_str())
            .unwrap_or("qwen-local")
            .to_owned();
        // The reference acknowledges a preload job before resolving its local
        // profile. In cloud-only mode resolution fails without lease/engine work.
        let worker = app.clone();
        let model = alias.clone();
        tokio::spawn(async move {
            *worker.preload.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                json!({"state":"failed","model":model,
                "error":"Model setup is still running. Run `oxc status` to follow it.","t":chrono::Local::now().timestamp()}),
            );
        });
        return context.reply(202, json!({"state":"loading","model":alias,"started":true}));
    }
    if path.starts_with("/_local/") {
        return context.error(404, "Unknown local control route.", "", "", None);
    }
    if method == Method::POST
        && ["/v1/chat/completions", "/chat/completions"].contains(&path.as_str())
    {
        let headers = request.headers();
        let length = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<i64>().ok());
        if headers.contains_key("transfer-encoding") || length.is_none() {
            return context.error(
                411,
                "Use a content length for request bodies.",
                "invalid_request_error",
                "length_required",
                None,
            );
        }
        if !(0..=16 * 1024 * 1024).contains(&length.unwrap()) {
            return context.error(
                413,
                "Request is too large.",
                "invalid_request_error",
                "request_too_large",
                None,
            );
        }
        let gzip = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            == Some("gzip");
        let raw = match axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024).await {
            Ok(data) => data,
            Err(_) => {
                return context.error(
                    413,
                    "Request is too large.",
                    "invalid_request_error",
                    "request_too_large",
                    None,
                )
            }
        };
        let raw = if gzip {
            http_gate::decompress_gzip(&raw).ok()
        } else {
            Some(raw.to_vec())
        };
        let payload = raw.and_then(|data| serde_json::from_slice::<Value>(&data).ok());
        let Some(payload) = payload.filter(Value::is_object) else {
            return context.error(
                400,
                "Invalid model request.",
                "invalid_request_error",
                "invalid_request",
                None,
            );
        };
        let alias = payload["model"].as_str().unwrap_or("");
        let entries = catalog::entries(&config);
        let item = entries.iter().find(|item| item["id"] == alias).or_else(|| {
            ["openai/", "openai_compatible/"].iter().find_map(|prefix| {
                alias
                    .strip_prefix(prefix)
                    .and_then(|alias| entries.iter().find(|item| item["id"] == alias))
            })
        });
        let Some(item) = item else {
            return context.error(
                404,
                "Unknown local model. Choose one listed in /v1/models.",
                "invalid_request_error",
                "model_not_found",
                Some("model"),
            );
        };
        if let Err(error) = policy::before_credentials(
            &payload,
            false,
            catalog::offline(),
            item["pricing"].is_object(),
        ) {
            return context.reply(error.status, error.body());
        }
        if let Err(message) = catalog::secret_result(item["provider"].as_str().unwrap(), &config) {
            let refusal = policy::Refusal::credentials();
            return context.error(
                refusal.status,
                &message,
                "cloud_error",
                refusal.code,
                Some(refusal.param),
            );
        }
        let price = &item["pricing"];
        let tiers = price["tiers"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![price.clone()]);
        let input = tiers
            .iter()
            .filter_map(|p| {
                p["input_per_million"].as_f64().map(|base| {
                    let anthropic = item["provider"] == "anthropic"
                        || item["upstream_model"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("anthropic/"));
                    base.max(p["cache_read_per_million"].as_f64().unwrap_or(base))
                        .max(
                            p["cache_write_per_million"]
                                .as_f64()
                                .unwrap_or(base * if anthropic { 2.0 } else { 1.0 }),
                        )
                })
            })
            .fold(0.0_f64, f64::max);
        let output = tiers
            .iter()
            .filter_map(|p| p["output_per_million"].as_f64())
            .fold(0.0_f64, f64::max);
        let maximum = item["max_output"].as_u64().unwrap_or(4096);
        let mut tokens = maximum;
        for field in ["max_tokens", "max_completion_tokens", "n_predict"] {
            if let Some(value) = payload.get(field) {
                let Some(value) = value.as_u64().filter(|n| *n > 0) else {
                    return context.error(
                        400,
                        "The output token limit must be a positive integer.",
                        "cloud_error",
                        "invalid_value",
                        Some(field),
                    );
                };
                tokens = value.min(maximum);
                break;
            }
        }
        let required = (item["context_window"].as_f64().unwrap_or(f64::INFINITY) * input
            + tokens as f64 * output)
            / 1e6
            * if item["provider"] == "openrouter" {
                1.25
            } else {
                1.0
            };
        if let Err(error) = policy::daily_budget(
            &config["cloud"]["daily_budget_usd"],
            std::env::var("OXC_LEDGER").as_deref() != Ok("0"),
            catalog::spend(),
            0.0,
            required,
        ) {
            return context.reply(error.status, error.body());
        }
    }
    context.error(
        503,
        "Cloud requests are unavailable.",
        "cloud_error",
        "cloud_unavailable",
        Some("model"),
    )
}

pub async fn run() -> i32 {
    let Ok(port) = std::env::var("LOCAL_LLM_PORT")
        .unwrap_or_else(|_| "8092".into())
        .parse::<u16>()
    else {
        eprintln!("oxc: invalid LOCAL_LLM_PORT");
        return 1;
    };
    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("oxc: {error}");
            return 1;
        }
    };
    let app = Arc::new(App {
        port,
        config: Mutex::new(paths::read_config().unwrap_or_else(|| json!({}))),
        preload: Mutex::new(None),
    });
    if let Some(catalog) = Catalog::from_config(
        &app.config.lock().unwrap_or_else(|e| e.into_inner()),
        catalog::offline(),
    ) {
        eprintln!("{}", catalog.startup_line());
    }
    let router = Router::new().fallback(handle).with_state(app);
    let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        return 1;
    };
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted=listener.accept()=>match accepted {Ok((stream,_))=>{connections.spawn(transport::serve(stream,router.clone()));},Err(error)=>{eprintln!("oxc: {error}");return 1;}},
            _=term.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            Some(_)=connections.join_next()=>{},
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    0
}
