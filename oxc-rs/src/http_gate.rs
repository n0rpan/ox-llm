//! Engine-independent HTTP reply, Host/Origin/auth and gzip policy.
//! The runtime supplies configuration and optional sharing admission state;
//! this module does not create an engine, acquire a lease or route upstream.
use crate::paths;
use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};
use std::io::Read;
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct Context<R> {
    pub path: String,
    pub head: bool,
    pub id: String,
    pub cors: Option<String>,
    pub started: std::time::Instant,
    pub remote: Option<R>,
}

pub fn random_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(format!(
        "req_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
impl<R> Context<R> {
    pub fn reply(&self, code: u16, value: Value) -> Response {
        let value = if code >= 400 && !self.path.starts_with("/_local/") {
            normalize_error(code, value)
        } else {
            value
        };
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
    pub fn error(
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
    pub fn stream(&self, body: axum::body::Body) -> Response {
        let mut response = body.into_response();
        for (name, value) in [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-store"),
            ("transfer-encoding", "chunked"),
        ] {
            response
                .headers_mut()
                .insert(name, HeaderValue::from_static(value));
        }
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&self.id).expect("generated request id"),
        );
        self.cors_headers(&mut response);
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
    if let Ok(value) = std::env::var("OXC_API_KEY") {
        if !value.trim().is_empty() {
            return Some(value.trim().as_bytes().to_vec());
        }
    }
    let path = config["openai_api"]
        .get("api_key_file")
        .filter(|path| !path.is_null())?;
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
        .split(['\r', '\n'])
        .next()
        .unwrap_or("")
        .chars()
        .take(4097)
        .collect::<String>();
    Some(first.trim().as_bytes().to_vec())
}
pub fn decompress_gzip(mut input: &[u8]) -> std::io::Result<Vec<u8>> {
    const LIMIT: usize = 16 * 1024 * 1024;
    let mut output = Vec::new();
    while !input.is_empty() {
        // A bufread decoder stops precisely after one member, so subsequent
        // members and their checksums are not silently discarded. Python also
        // accepts zero padding after/between members, but never before the first.
        let mut decoder = flate2::bufread::GzDecoder::new(input);
        decoder
            .by_ref()
            .take((LIMIT + 1 - output.len()) as u64)
            .read_to_end(&mut output)?;
        if output.len() > LIMIT {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let rest = decoder.into_inner();
        if rest.len() >= input.len() {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        input = rest;
        while input.first() == Some(&0) {
            input = &input[1..];
        }
    }
    Ok(output)
}
fn header_text(value: &HeaderValue) -> String {
    // BaseHTTPRequestHandler decodes field bytes as ISO-8859-1. API key
    // comparison then uses UTF-8 for both this text and the private key file.
    value
        .as_bytes()
        .iter()
        .map(|byte| char::from(*byte))
        .collect()
}
pub fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("authorization").map(header_text) {
        if value
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        {
            return Some(value[7..].trim().to_owned());
        }
    }
    for name in ["x-api-key", "api-key"] {
        if let Some(value) = headers
            .get(name)
            .map(header_text)
            .filter(|s| !s.trim().is_empty())
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

pub fn normalize_error(status: u16, mut value: Value) -> Value {
    if status < 400 {
        return value;
    }
    let Some(error) = value.get_mut("error") else {
        return value;
    };
    if !error.is_object() {
        *error = json!({"message":error.as_str().map(str::to_owned).unwrap_or_else(||error.to_string())});
    }
    let message = error["message"].as_str().unwrap_or("").to_owned();
    let lower = message.to_lowercase();
    let kind = error["type"].as_str().unwrap_or("").to_owned();
    let code = error["code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            {
                if kind == "exceed_context_size_error"
                    || [
                        "context size",
                        "context length",
                        "exceeds the available context",
                    ]
                    .iter()
                    .any(|s| lower.contains(s))
                {
                    "context_length_exceeded"
                } else if status == 404 && lower.contains("model") {
                    "model_not_found"
                } else {
                    match status {
                        400 => "invalid_request",
                        401 => "invalid_api_key",
                        403 => "forbidden",
                        404 => "not_found",
                        408 | 504 => "timeout",
                        409 => "conflict",
                        411 => "length_required",
                        413 => "request_too_large",
                        429 => "rate_limit_exceeded",
                        499 => "client_closed_request",
                        501 => "not_implemented",
                        502 => "bad_gateway",
                        503 => "service_unavailable",
                        _ if status >= 500 => "server_error",
                        _ => "invalid_request",
                    }
                }
            }
            .into()
        });
    let fallback = match code.as_str() {
        "context_length_exceeded" => json!("messages"),
        "model_not_found" => json!("model"),
        _ => Value::Null,
    };
    let param = error["param"]
        .as_str()
        .map(|v| json!(v))
        .unwrap_or(fallback);
    error["message"] = json!(message);
    error["type"] = json!(if kind.is_empty() {
        match status {
            400 | 411 | 413 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            409 => "conflict_error",
            429 => "rate_limit_error",
            501 => "not_supported_error",
            503 => "unavailable_error",
            _ => "server_error",
        }
    } else {
        &kind
    });
    error["code"] = json!(code);
    error["param"] = param;
    value
}

pub fn target<R>(port: u16, request: &Request, context: &Context<R>) -> Option<Response> {
    let headers = request.headers();
    let target = request.uri().to_string();
    let host = single_header(headers, "host").map(|s| s.trim().to_ascii_lowercase());
    let allowed = [
        format!("127.0.0.1:{}", port),
        format!("localhost:{}", port),
        format!("[::1]:{}", port),
    ];
    if !(if context.remote.is_some() {
        headers.get_all("host").iter().count() == 1
    } else {
        host.is_some_and(|s| allowed.contains(&s))
    }) {
        return Some(context.error(
            403,
            "Host not allowed: use 127.0.0.1, localhost or [::1] with this port.",
            "permission_error",
            "forbidden",
            None,
        ));
    }
    if !target.starts_with('/') || target.starts_with("//") {
        return Some(context.error(
            404,
            "Unknown route.",
            "invalid_request_error",
            "not_found",
            None,
        ));
    }
    None
}

pub fn authorize<R>(
    context: &mut Context<R>,
    port: u16,
    headers: &HeaderMap,
    method: Method,
    has_query: bool,
    config: &Value,
) -> Option<Response> {
    let path = context.path.clone();
    let key = api_key(config);
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
        && !has_query
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
                && origin.is_none_or(|origin| loopback_origin(origin, port))));
    if context.cors.is_none() && !same_origin {
        return Some(context.error(
            403,
            "Cross-origin local AI request refused.",
            "permission_error",
            "forbidden",
            None,
        ));
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
                .filter(|value| !value.is_empty())
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
        return Some(reply);
    }
    if let Some(key) = key
        .filter(|_| context.remote.is_none() && path != "/health" && !path.starts_with("/_local/"))
    {
        let authorized = !key.is_empty()
            && presented_key(headers).is_some_and(|value| bool::from(value.as_bytes().ct_eq(&key)));
        if !authorized {
            return Some(context.error(
                401,
                "Missing or wrong API key. Send \"Authorization: Bearer <key>\".",
                "authentication_error",
                "invalid_api_key",
                None,
            ));
        }
    }
    None
}
