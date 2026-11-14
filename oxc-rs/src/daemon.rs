use crate::{models, paths};
use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use serde_json::{json, Value};
use std::{
    io::Read,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

struct App {
    port: u16,
    last_config: Mutex<Value>,
}
struct Context {
    path: String,
    head: bool,
    id: String,
    cors: Option<String>,
}

fn random_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(format!(
        "req_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
impl Context {
    fn reply(&self, code: u16, value: Value) -> Response {
        let raw = paths::python_json(&value);
        let length = raw.len();
        let mut response = (
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [
                ("content-type", "application/json"),
                ("cache-control", "no-store"),
            ],
            if self.head { String::new() } else { raw },
        )
            .into_response();
        response
            .headers_mut()
            .insert("content-length", HeaderValue::from(length));
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&self.id).expect("generated request id"),
        );
        if matches!(code, 429 | 503) {
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("5"));
        }
        self.cors_headers(&mut response);
        response
    }
    fn cors_headers(&self, response: &mut Response) {
        if let Some(origin) = &self.cors {
            if let Ok(value) = HeaderValue::from_str(origin) {
                response
                    .headers_mut()
                    .insert("access-control-allow-origin", value);
            }
            response
                .headers_mut()
                .insert("vary", HeaderValue::from_static("Origin"));
        }
    }
    fn error(
        &self,
        code: u16,
        message: &str,
        kind: &str,
        label: &str,
        param: Option<&str>,
    ) -> Response {
        let value = if self.path.starts_with("/_local/") {
            json!({"error":message})
        } else {
            json!({"error":{"message":message,"type":kind,"param":param,"code":label}})
        };
        let mut response = self.reply(code, value);
        if matches!(code, 401 | 403 | 411 | 413) || message == "Invalid content length." {
            response
                .extensions_mut()
                .insert(crate::transport::CloseConnection);
        }
        response
    }
}
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}
fn has_one_or_none(headers: &HeaderMap, name: &str) -> bool {
    headers.get_all(name).iter().count() <= 1
}
fn loopback_origin(origin: &str, port: u16) -> bool {
    [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        format!("http://[::1]:{port}"),
    ]
    .contains(&origin.to_owned())
}
// None means authentication is disabled. An empty key means configured but
// unreadable/empty, and is deliberately impossible to authenticate with.
fn api_key(config: &Value) -> Option<Vec<u8>> {
    if let Ok(value) = std::env::var("LOCAL_AI_API_KEY") {
        if !value.trim().is_empty() {
            return Some(value.trim().as_bytes().to_vec());
        }
    }
    let path = config["openai_api"].get("api_key_file")?;
    let Some(path) = path
        .as_str()
        .filter(|path| std::path::Path::new(path).is_absolute())
    else {
        return Some(Vec::new());
    };
    let mut bytes = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return Some(Vec::new());
    };
    if file.take(4 * 4097).read_to_end(&mut bytes).is_err() {
        return Some(Vec::new());
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Some(Vec::new());
    };
    let first = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(4097)
        .collect::<String>();
    Some(first.trim().as_bytes().to_vec())
}
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        if value
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        {
            return Some(value[7..].trim());
        }
    }
    for name in ["x-api-key", "api-key"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(value);
        }
    }
    None
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let method = request.method().clone();
    let target = request.uri().to_string();
    let path = request.uri().path().trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path }.to_owned();
    let Ok(id) = random_id() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut context = Context {
        path: path.clone(),
        head: method == Method::HEAD,
        id,
        cors: None,
    };
    let headers = request.headers();
    let host = single_header(headers, "host").map(|s| s.trim().to_ascii_lowercase());
    let allowed = [
        format!("127.0.0.1:{}", app.port),
        format!("localhost:{}", app.port),
        format!("[::1]:{}", app.port),
    ];
    if !host.is_some_and(|s| allowed.contains(&s)) {
        return context.error(
            403,
            "Host not allowed: use 127.0.0.1, localhost or [::1] with this port.",
            "permission_error",
            "forbidden",
            None,
        );
    }
    if !target.starts_with('/') || target.starts_with("//") {
        return context.error(
            404,
            "Unknown route.",
            "invalid_request_error",
            "not_found",
            None,
        );
    }
    let fresh = paths::read_config();
    let config = {
        let mut last = app
            .last_config
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(value) = &fresh {
            *last = value.clone();
        }
        last.clone()
    };
    let key = api_key(&config);
    let origin = single_header(headers, "origin");
    if !path.starts_with("/_local/") {
        if let (Some(origin), Some(allowed)) =
            (origin, config["openai_api"]["cors_origins"].as_array())
        {
            if ((origin != "*" && origin != "null") && allowed.iter().any(|v| v == origin))
                || (key.is_some() && allowed.iter().any(|v| v == "*"))
            {
                context.cors = Some(origin.to_owned());
            }
        }
    }
    let navigation = origin.is_none()
        && [Method::GET, Method::HEAD].contains(&method)
        && path == "/"
        && [
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
            ("sec-fetch-user", "?1"),
        ]
        .iter()
        .all(|(k, v)| single_header(headers, k) == Some(v));
    let site = single_header(headers, "sec-fetch-site");
    let same_origin = has_one_or_none(headers, "origin")
        && has_one_or_none(headers, "sec-fetch-site")
        && (navigation
            || (site.is_none_or(|s| s == "same-origin" || s == "none")
                && origin.is_none_or(|origin| loopback_origin(origin, app.port))));
    if context.cors.is_none() && !same_origin {
        return context.error(
            403,
            "Cross-origin local AI request refused.",
            "permission_error",
            "forbidden",
            None,
        );
    }
    if context.cors.is_some() && method == Method::OPTIONS {
        // Bug-for-bug: Python preflight does not include x-request-id.
        let mut reply = StatusCode::NO_CONTENT.into_response();
        context.cors_headers(&mut reply);
        reply.headers_mut().insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        reply.headers_mut().insert(
            "access-control-allow-headers",
            headers
                .get("access-control-request-headers")
                .cloned()
                .unwrap_or(HeaderValue::from_static("authorization, content-type")),
        );
        reply
            .headers_mut()
            .insert("access-control-max-age", HeaderValue::from_static("600"));
        if headers
            .get("access-control-request-private-network")
            .is_some_and(|v| v == "true")
        {
            reply.headers_mut().insert(
                "access-control-allow-private-network",
                HeaderValue::from_static("true"),
            );
        }
        return reply;
    }
    if let Some(key) = key.filter(|_| path != "/health" && !path.starts_with("/_local/")) {
        let authorized = !key.is_empty()
            && presented_key(headers).is_some_and(|value| bool::from(value.as_bytes().ct_eq(&key)));
        if !authorized {
            return context.error(
                401,
                "Missing or wrong API key. Send \"Authorization: Bearer <key>\".",
                "authentication_error",
                "invalid_api_key",
                None,
            );
        }
    }
    if [Method::GET, Method::HEAD].contains(&method)
        && (path == "/v1/models"
            || path == "/models"
            || path.starts_with("/v1/models/")
            || path.starts_with("/models/"))
    {
        let rows = fresh.as_ref().map(models::rows).unwrap_or_default();
        if fresh.is_none() || ((path == "/v1/models" || path == "/models") && rows.is_empty()) {
            return context.error(
                503,
                "Model setup is not finished yet. Run `local-ai status` to follow it.",
                "unavailable_error",
                "service_unavailable",
                None,
            );
        }
        if let Some((_, wanted)) = path.split_once("/models/") {
            let resolved = models::resolve(&config, Some(wanted)).ok();
            for row in rows {
                if row["id"] == wanted
                    || (["openai/", "openai_compatible/", "local/"]
                        .iter()
                        .any(|prefix| wanted.starts_with(prefix))
                        && resolved
                            .as_ref()
                            .is_some_and(|p| row["id"] == p["model_alias"]))
                {
                    return context.reply(200, row);
                }
            }
            return context.error(
                404,
                models::UNKNOWN,
                "invalid_request_error",
                "model_not_found",
                Some("model"),
            );
        }
        return context.reply(200, json!({"object":"list","data":rows}));
    }
    if path == "/_local/status" {
        let selected = models::selected(&config);
        let profile = models::resolve(&config, None).unwrap_or_else(|_| json!({}));
        return context.reply(200,json!({"state":"unloaded","model":selected,"selected_model":selected,"validated":models::eligible(&profile),"active_requests":0,"queued_requests":0,"capabilities":["models"],"context_size":profile["context_size"]}));
    }
    if path == "/_local/models" {
        return context.reply(200, json!({"object":"list","data":models::rows(&config)}));
    }
    if path == "/_local/ledger" {
        return context.reply(
            200,
            json!({"object":"list","available":false,"rows":[],"medians":{}}),
        );
    }
    if path == "/health" && models::resolve(&config, None).is_ok_and(|p| models::eligible(&p)) {
        return context.reply(200, json!({"status":"ok","sleeping":true}));
    }
    let encoding = headers
        .get("content-encoding")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if headers.contains_key("transfer-encoding") {
        return context.error(
            411,
            "Use a content length for request bodies.",
            "invalid_request_error",
            "length_required",
            None,
        );
    }
    if let Some(length) = headers.get("content-length").and_then(|h| h.to_str().ok()) {
        match length.parse::<i64>() {
            Ok(length) if !(0..=16 * 1024 * 1024).contains(&length) => {
                return context.error(
                    413,
                    "Request is too large.",
                    "invalid_request_error",
                    "request_too_large",
                    None,
                )
            }
            Err(_) => {
                return context.error(
                    400,
                    "Invalid content length.",
                    "invalid_request_error",
                    "invalid_request",
                    None,
                )
            }
            _ => {}
        }
    }
    let raw = match axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024).await {
        Ok(raw) => raw,
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
    let raw = if encoding == "gzip" {
        let mut raw_decompressed = Vec::new();
        if flate2::read::GzDecoder::new(raw.as_ref())
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut raw_decompressed)
            .is_err()
            || raw_decompressed.len() > 16 * 1024 * 1024
        {
            return context.error(
                400,
                "Invalid model request.",
                "invalid_request_error",
                "invalid_request",
                None,
            );
        }
        raw_decompressed
    } else {
        raw.to_vec()
    };
    let payload = if !raw.is_empty() && method == Method::POST {
        match serde_json::from_slice::<Value>(&raw) {
            Ok(value) => Some(value),
            Err(_) => {
                return context.error(
                    400,
                    "Invalid model request.",
                    "invalid_request_error",
                    "invalid_request",
                    None,
                )
            }
        }
    } else {
        None
    };
    if let Some(model) = payload.as_ref().and_then(|p| p.get("model")) {
        let Some(model) = model.as_str() else {
            return context.error(
                400,
                "Invalid model request.",
                "invalid_request_error",
                "invalid_request",
                None,
            );
        };
        if models::resolve(&config, Some(model)).is_err() {
            return context.error(
                404,
                models::UNKNOWN,
                "invalid_request_error",
                "model_not_found",
                Some("model"),
            );
        }
    }
    if fresh.is_some()
        && path.starts_with("/v1/")
        && ![
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/responses",
            "/v1/models",
        ]
        .contains(&path.as_str())
    {
        return context.error(
            404,
            &format!("Invalid URL ({method} {path}): this local model server does not provide it."),
            "invalid_request_error",
            "unknown_url",
            None,
        );
    }
    context.error(
        503,
        "Local model setup is still being prepared.",
        "local_backend_busy",
        "service_unavailable",
        None,
    )
}

pub async fn run() -> i32 {
    let port = std::env::var("LOCAL_LLM_PORT").unwrap_or_else(|_| "8092".into());
    let Ok(port) = port.parse::<u16>() else {
        eprintln!("local-ai: invalid LOCAL_LLM_PORT");
        return 1;
    };
    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("local-ai: {error}");
            return 1;
        }
    };
    let app = Arc::new(App {
        port,
        last_config: Mutex::new(paths::read_config().unwrap_or_else(|| json!({}))),
    });
    let router = Router::new().fallback(handle).with_state(app);
    let mut connections = tokio::task::JoinSet::new();
    let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        return 1;
    };
    loop {
        tokio::select! {
            accepted=listener.accept()=>match accepted {
                Ok((stream,_))=>{connections.spawn(crate::transport::serve(stream,router.clone()));},
                Err(error)=>{eprintln!("local-ai: {error}");return 1;}
            },
            _=term.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            Some(_)=connections.join_next()=>{},
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    0
}
