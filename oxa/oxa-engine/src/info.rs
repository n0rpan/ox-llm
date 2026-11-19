//! A2: engine info and tokenize.
use crate::{http_request, HttpRequest, Url};
use oxa_proto::{codes, EngineCaps, EngineInfo, OxaError, Value, CANCEL_HINT, CANCEL_PATH};

/// Timeout (silence limit) of the non-streaming requests: info and tokenize.
pub(crate) const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Output limit when the engine publishes none: the controller's approved per-profile default
/// (`default_max_tokens` 16384), at most a quarter of the context so compaction keeps room.
pub const DEFAULT_MAX_OUTPUT: u32 = 16_384;

/// Build [`EngineInfo`] from the two JSON documents: `/v1/models` (`data[].id`) and the status.
/// The limits are the loaded model's; see [`parse_info_for`].
pub fn parse_info(base_url: &str, models: &Value, status: &Value) -> Result<EngineInfo, OxaError> {
    parse_info_for(base_url, models, status, None)
}

/// Build [`EngineInfo`] from `/v1/models` (`data[].id`) and the status, with the limits of
/// `target` when the status publishes them (X3).
///
/// The status is the oxc controller's `/_local/status` (`StatusMixin.status` in the lab's
/// `runtime/ctl_status.py`), or a plain llama-server / oxe `/props` when the controller is
/// absent. Field mapping, first present wins:
///
/// * `model`: `model`, `model_alias` (`/props`), else the first `/v1/models` id.
/// * Limits row: `model_limits[target]` (`target` = the given one, else `model`), the
///   controller's per-model limits (lab `docs/UI-DATA.md`); it holds `cap`, `mode`, `modes` and
///   `max_output` for that model, loaded or not. When the row exists (with a positive `cap`)
///   the limits below are read from it and `limits_for` is that key; otherwise from the top
///   level and `limits_for = model`.
/// * Remote catalog models (`data[].remote == true`) use their positive `context_length`
///   and `max_output_tokens` ahead of local status limits, without a local compaction threshold.
/// * `n_ctx`: row `cap`; top level `context.cap` (the cap of the selected context mode),
///   `context_size`, `n_ctx`, `default_generation_settings.n_ctx` (`/props`). None of them =
///   `engine_http` error.
/// * `max_output`: row `max_output`, top level `max_output` when published, else
///   `min(DEFAULT_MAX_OUTPUT, n_ctx / 4)`.
/// * `queue`: `queued_requests`, `active.queued_requests`, `queue`; else 0.
/// * `compact_at`: `modes[id == mode].compact_at` of the row, or of the top-level `context`
///   (D5), else `None`.
/// * Capabilities (F4 / EG2): `model_limits[limits_for].capabilities`, published by the
///   controller BEFORE the model loads: `prefill_only` (bool), `snapshot_hint` (int ->
///   `caps.snapshot_keep`), `hint_clear` (bool), `thinking_budget` (local-only bool),
///   `preserve_thinking` (bool -> `Some`), `park ==
///   "disk"` (`caps.park_disk_only`). When that row has no `capabilities` block, the loaded
///   engine's `/props` `oxe_hints` ([`fetch_info`] merges it into the controller's status):
///   `prefill_only`, `snapshot_keep`, `hint_clear`; `preserve_thinking` is then `None` and
///   `park_disk_only` `false`. The generation-policy capability `thinking_budget` is read
///   from `/props` top level, not `oxe_hints` (hints must not change output). Neither =
///   nothing advertised.
/// * F14: a status without any context size whose `state` is `loading`, `starting` or
///   `stopping` (the controller answers so while it loads a model) is the retryable
///   `engine_busy`, not `engine_http`.
pub fn parse_info_for(
    base_url: &str,
    models: &Value,
    status: &Value,
    target: Option<&str>,
) -> Result<EngineInfo, OxaError> {
    let ids: Vec<String> = models
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // A cloud-only controller (`engine_mode: "none"`) still reports its default local alias as
    // `model`; no engine runs it, so the first listed (remote) model is the one to use.
    let no_engine = status.get("engine_mode").and_then(Value::as_str) == Some("none");
    let model = ["/model", "/model_alias"]
        .iter()
        .filter(|_| !(no_engine && !ids.is_empty()))
        .find_map(|p| {
            status
                .pointer(p)
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
        })
        .map(str::to_string)
        .or_else(|| ids.first().cloned())
        .unwrap_or_default();

    let key = target.filter(|t| !t.is_empty()).unwrap_or(model.as_str());
    let row = status
        .get("model_limits")
        .and_then(|m| m.get(key))
        .filter(|r| first_u32(r, &["/context_window", "/cap"]).is_some_and(|n| n > 0));

    // Cloud limits belong to the requested catalog model, never the loaded local engine.
    let remote = models
        .get("data")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|m| m.get("id").and_then(Value::as_str) == Some(key))
        })
        .filter(|m| m.get("remote").and_then(Value::as_bool) == Some(true));
    let remote_ctx = remote.and_then(|m| first_u32(m, &["/context_length"]).filter(|n| *n > 0));
    let (limits_for, n_ctx, published_output, compact_at) = match (remote_ctx, row) {
        (Some(n_ctx), _) => (
            key.to_string(),
            n_ctx,
            remote.and_then(|m| first_u32(m, &["/max_output_tokens"])),
            None,
        ),
        (None, Some(r)) => (
            key.to_string(),
            first_u32(r, &["/context_window", "/cap"]).unwrap_or(0),
            first_u32(r, &["/max_output"]),
            mode_compact_at(r),
        ),
        (None, None) => {
            let n_ctx = first_u32(
                status,
                &["/context/cap", "/context_size", "/n_ctx", "/default_generation_settings/n_ctx"],
            )
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                let state = status.get("state").and_then(Value::as_str).unwrap_or("");
                if matches!(state, "loading" | "starting" | "stopping") {
                    return OxaError::new(
                        codes::ENGINE_BUSY,
                        format!("the controller is {state} a model and reports no context size yet"),
                    )
                    .with_hint("the model is loading; try again in a few seconds");
                }
                OxaError::new(codes::ENGINE_HTTP, "the engine status reports no context size")
                    .with_hint(
                        "the controller's /_local/status must carry context.cap or context_size (or the engine's /props n_ctx); a controller that is still loading reports neither, so try again",
                    )
            })?;
            let compact_at = status.get("context").and_then(mode_compact_at);
            (
                model.clone(),
                n_ctx,
                first_u32(status, &["/max_output"]),
                compact_at,
            )
        }
    };

    let max_output = published_output
        .filter(|n| *n > 0)
        .unwrap_or_else(|| DEFAULT_MAX_OUTPUT.min(n_ctx / 4).max(1));
    let queue = first_u32(status, &["/queued_requests", "/active/queued_requests", "/queue"]).unwrap_or(0);
    let (prefill_only, mut caps) = capabilities(status, &limits_for);
    caps.remote = caps.remote || remote.is_some();
    // Untargeted discovery must expose the catalog so callers can resolve their selection.
    // Session startup checks the resolved model, including when it uses the default.
    if target.is_some() && caps.remote && oxa_proto::offline() {
        return Err(oxa_proto::offline_error(&format!("the cloud model {limits_for}")));
    }
    let (thinking, images) = modality(status, &limits_for);

    Ok(EngineInfo {
        base_url: base_url.to_string(),
        models: ids,
        model,
        limits_for,
        n_ctx,
        max_output,
        queue,
        compact_at,
        prefill_only,
        caps,
        thinking,
        images,
    })
}

/// Whether `key` thinks or takes images (see [`EngineInfo::thinking`], [`EngineInfo::images`]):
/// the controller's `model_limits[key].capabilities` first (`thinking`, `vision`), else `None`
/// for either field the controller did not publish. A plain OpenAI server publishes neither.
fn modality(status: &Value, key: &str) -> (Option<bool>, Option<bool>) {
    let caps = status
        .get("model_limits")
        .and_then(|m| m.get(key))
        .and_then(|r| r.get("capabilities"))
        .filter(|c| c.is_object());
    match caps {
        Some(c) => (
            // Any thinking value that is not "none"/"off"/false means the model thinks.
            c.get("thinking").and_then(|t| {
                if let Some(b) = t.as_bool() {
                    Some(b)
                } else {
                    t.as_str().map(|s| !matches!(s, "none" | "off"))
                }
            }),
            c.get("vision").and_then(Value::as_bool),
        ),
        None => (None, None),
    }
}

/// `prefill_only` and the other capabilities of `key` (see [`parse_info_for`]): the controller's
/// `model_limits[key].capabilities` first, else the merged `/props` `oxe_hints`.
fn capabilities(status: &Value, key: &str) -> (bool, EngineCaps) {
    let flag = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    let count = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let published = status
        .get("model_limits")
        .and_then(|m| m.get(key))
        .and_then(|r| r.get("capabilities"))
        .filter(|c| c.is_object());
    if let Some(c) = published {
        let prefill = flag(c, "prefill") || flag(c, "prefill_only");
        let caps = EngineCaps {
            snapshot_keep: count(c, "snapshot_hint"),
            hint_clear: flag(c, "hint_clear") || flag(c, "cache_hints"),
            preserve_thinking: c.get("preserve_thinking").and_then(Value::as_bool),
            thinking_budget: flag(c, "thinking_budget") && !flag(c, "remote"),
            park_disk_only: c.get("park").and_then(Value::as_str) == Some("disk"),
            remote: flag(c, "remote"),
            response_format: flag(c, "remote") && flag(c, "response_format"),
            remote_provider: match c.get("remote_provider").and_then(Value::as_str) {
                Some("openrouter") => Some("openrouter"),
                Some("anthropic") => Some("anthropic"),
                Some("openai") => Some("openai"),
                Some("chatgpt") => Some("chatgpt"),
                Some("opencode") => Some("opencode"),
                _ => None,
            },
            echo: flag(c, "echo"),
            cache_hints: flag(c, "cache_hints") || flag(c, "hint_clear"),
            cancel: flag(c, "cancel"),
            prefill,
            context_window: first_u32(c, &["/context_window"]),
        };
        return (prefill || flag(c, "prefill_only"), caps);
    }
    match status.get("oxe_hints").filter(|h| h.is_object()) {
        Some(h) => (
            flag(h, "prefill_only"),
            EngineCaps {
                snapshot_keep: count(h, "snapshot_keep"),
                hint_clear: flag(h, "hint_clear"),
                preserve_thinking: None,
                thinking_budget: flag(status, "thinking_budget"),
                park_disk_only: false,
                cache_hints: flag(h, "hint_clear"),
                prefill: flag(h, "prefill_only"),
                ..EngineCaps::default()
            },
        ),
        None => (false, EngineCaps::default()),
    }
}

/// `modes[id == mode].compact_at` of a context block (`{cap, mode, modes}`), when positive.
fn mode_compact_at(ctx: &Value) -> Option<u32> {
    let mode = ctx.get("mode").and_then(Value::as_str)?;
    ctx.get("modes")
        .and_then(Value::as_array)?
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(mode))
        .and_then(|row| row.get("compact_at").and_then(Value::as_u64))
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}

/// The first of `pointers` that holds a `u32`.
fn first_u32(v: &Value, pointers: &[&str]) -> Option<u32> {
    pointers.iter().find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    })
}

/// GET `/v1/models`, `/_local/status` and, best effort, `/props`, then [`parse_info`] (the
/// loaded model's limits). [`fetch_info_for`] reads a target's limits.
///
/// `/props` is the engine's own document (the controller proxies it and never starts a model
/// for it; it answers 503 while no model is loaded). Its `oxe_hints` block and its `n_ctx` are
/// merged into the controller's status where the status lacks them. Without `/_local/status`
/// (a plain engine) `/props` is the status. A `/props` that fails or is absent only means
/// "no advertised capabilities".
pub fn fetch_info(url: &Url, api_key: Option<&str>) -> Result<EngineInfo, OxaError> {
    fetch_info_for(url, api_key, None)
}

/// [`fetch_info`] with the limits of `target` ([`parse_info_for`]).
pub fn fetch_info_for(
    url: &Url,
    api_key: Option<&str>,
    target: Option<&str>,
) -> Result<EngineInfo, OxaError> {
    fetch_info_full(url, api_key, target).map(|(info, _, _)| info)
}

/// [`fetch_info_for`] plus the deliberate-cancel capability of the endpoint
/// ([`cancel_advertised`] on its `/props`). The flag is kept by the engine client, not in
/// [`EngineInfo`], so the published info type stays unchanged.
pub(crate) fn fetch_info_full(
    url: &Url,
    api_key: Option<&str>,
    target: Option<&str>,
) -> Result<(EngineInfo, bool, Option<String>), OxaError> {
    let headers = auth_headers(api_key);
    let models = get_json(url, "/v1/models", &headers)?.ok_or_else(|| {
        OxaError::new(codes::ENGINE_HTTP, "the engine has no /v1/models endpoint")
            .with_hint("the engine must serve the OpenAI-compatible /v1/models")
    })?;
    let props = get_json(url, "/props", &headers).ok().flatten();
    // A4: a plain engine answers 404 here; asked once per endpoint and process.
    let status = if endpoint_absent(url, "/_local/status") {
        None
    } else {
        let v = get_json(url, "/_local/status", &headers)?;
        if v.is_none() {
            mark_absent(url, "/_local/status");
        }
        v
    };
    // Read before props is merged into the status below.
    let cancel = cancel_advertised(status.as_ref(), props.as_ref());
    let handshake = crate::handshake::handshake_warning(status.as_ref(), props.as_ref());
    let status = match status {
        Some(mut v) => {
            if let (Some(obj), Some(p)) = (v.as_object_mut(), props.as_ref()) {
                for key in ["oxe_hints", "default_generation_settings"] {
                    if let Some(x) = p.get(key) {
                        obj.entry(key).or_insert_with(|| x.clone());
                    }
                }
            }
            v
        }
        None => props.ok_or_else(|| {
            OxaError::new(
                codes::ENGINE_HTTP,
                "the engine has neither /_local/status nor /props",
            )
            .with_hint("the engine must report its context size on /_local/status or /props")
        })?,
    };
    let info = parse_info_for(&url_string(url), &models, &status, target)?;
    Ok((info, cancel, handshake))
}

/// PROVISIONAL, capability-gated proposal (cancel-contract.md section 7, see [`CANCEL_PATH`]):
/// `true` when the endpoint oxa talks to advertises [`CANCEL_HINT`] in one of two places. The
/// controller advertises it in the top-level `capabilities` array of `GET /_local/status` (its
/// own list, not the per-model `model_limits[..].capabilities` objects); an engine advertises it
/// in the `oxe_hints.hints` array of `/props`. The path stays PROVISIONAL until the controller
/// implements `POST /_local/cancel`.
pub(crate) fn cancel_advertised(status: Option<&Value>, props: Option<&Value>) -> bool {
    let in_status = status
        .and_then(|s| s.get("capabilities"))
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(|c| c.as_str() == Some(CANCEL_HINT)));
    let in_props = props
        .and_then(|p| p.get("oxe_hints"))
        .and_then(|h| h.get("hints"))
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(|h| h.as_str() == Some(CANCEL_HINT)));
    in_status || in_props
}

/// Capability-gated proposal (cancel-contract.md section 7, see [`CANCEL_PATH`]): `POST {"idempotency_key", "session"}`
/// to the engine, on a thread of its own so an abort is never delayed by it. Fire-and-forget:
/// every error and every non-2xx answer is dropped. The timeout bounds connect and silence (the
/// HTTP client has no wall-clock limit), which is enough: nothing waits for this thread.
pub(crate) fn send_cancel(url: &Url, api_key: Option<&str>, idem_key: &str, session: &str) {
    const CANCEL_TIMEOUT_MS: u64 = 1_000;
    let mut headers = auth_headers(api_key);
    if !session.is_empty() {
        headers.push(("X-Oxe-Session", session.to_string()));
        headers.push(("X-Session-Id", session.to_string()));
    }
    headers.push(("Content-Type", "application/json".to_string()));
    let Ok(body) = serde_json::to_vec(&serde_json::json!({
        "idempotency_key": idem_key,
        "session": session,
    })) else {
        return;
    };
    let url = url.clone();
    let _ = std::thread::Builder::new()
        .name("oxa-cancel".into())
        .spawn(move || {
            let _ = http_request(&HttpRequest {
                method: "POST",
                url: &url,
                path: CANCEL_PATH,
                headers: &headers,
                body: &body,
                timeout_ms: CANCEL_TIMEOUT_MS,
            });
        });
}

/// `POST /tokenize {"content": text}` -> number of tokens.
pub fn count_tokens(url: &Url, api_key: Option<&str>, text: &str) -> Result<u32, OxaError> {
    let body = serde_json::to_vec(&serde_json::json!({ "content": text }))
        .map_err(|e| OxaError::new(codes::INTERNAL, format!("tokenize body: {e}")))?;
    let mut headers = auth_headers(api_key);
    headers.push(("Content-Type", "application/json".to_string()));
    let resp = http_request(&HttpRequest {
        method: "POST",
        url,
        path: "/tokenize",
        headers: &headers,
        body: &body,
        timeout_ms: REQUEST_TIMEOUT_MS,
    })?;
    if !(200..300).contains(&resp.status) {
        return Err(http_error(resp.status, &resp.body));
    }
    let v: Value = serde_json::from_slice(&resp.body).map_err(|e| {
        OxaError::new(
            codes::ENGINE_HTTP,
            format!("the tokenize answer is not JSON: {e}"),
        )
    })?;
    let tokens = v
        .get("tokens")
        .and_then(Value::as_array)
        .ok_or_else(|| OxaError::new(codes::ENGINE_HTTP, "the tokenize answer has no `tokens` array"))?;
    u32::try_from(tokens.len())
        .map_err(|_| OxaError::new(codes::ENGINE_HTTP, "too many tokens in the answer"))
}

/// OXA-EXACT-COUNTS: `POST /apply-template` with the request's own chat body plus
/// `"return_tokens": true` (the engine's `service.rs`: the prompt's token ids exactly as a chat
/// request tokenizes it, generation opener included) -> the number of ids. A 404 (an engine
/// or proxy without the endpoint) is `Ok(None)` and remembered for the endpoint (asked once);
/// an answer without `tokens` is `Ok(None)` too.
pub(crate) fn count_prompt(
    url: &Url,
    api_key: Option<&str>,
    chat_body: &[u8],
) -> Result<Option<u64>, OxaError> {
    const PATH: &str = "/apply-template";
    if endpoint_absent(url, PATH) {
        return Ok(None);
    }
    // The body's bytes stay as built (the tools JSON is part of what is counted): the flag is
    // added before the closing brace.
    let Some(end) = chat_body.iter().rposition(|&b| b == b'}') else {
        return Err(OxaError::new(
            codes::INTERNAL,
            "the chat body is not a JSON object",
        ));
    };
    let mut body = chat_body[..end].to_vec();
    body.extend_from_slice(b",\"return_tokens\":true}");
    let mut headers = auth_headers(api_key);
    headers.push(("Content-Type", "application/json".to_string()));
    let resp = http_request(&HttpRequest {
        method: "POST",
        url,
        path: PATH,
        headers: &headers,
        body: &body,
        timeout_ms: REQUEST_TIMEOUT_MS,
    })?;
    if resp.status == 404 {
        mark_absent(url, PATH);
        return Ok(None);
    }
    if !(200..300).contains(&resp.status) {
        return Err(http_error(resp.status, &resp.body));
    }
    let v: Value = serde_json::from_slice(&resp.body).map_err(|e| {
        OxaError::new(
            codes::ENGINE_HTTP,
            format!("the apply-template answer is not JSON: {e}"),
        )
    })?;
    Ok(v.get("tokens").and_then(Value::as_array).map(|t| t.len() as u64))
}

/// EG5: `POST /_local/cache/clear {"scope":"hint","hint":h}` (the controller); a 404 falls back
/// to the engine's own `POST /state-cache/clear` with the same body. 409 (a request is running)
/// -> `busy`; any other non-2xx -> [`http_error`].
pub(crate) fn clear_hint(url: &Url, api_key: Option<&str>, hint: &str) -> Result<(), OxaError> {
    let body = serde_json::to_vec(&serde_json::json!({ "scope": "hint", "hint": hint }))
        .map_err(|e| OxaError::new(codes::INTERNAL, format!("clear body: {e}")))?;
    let mut headers = auth_headers(api_key);
    headers.push(("Content-Type", "application/json".to_string()));
    let post = |path: &str| {
        http_request(&HttpRequest {
            method: "POST",
            url,
            path,
            headers: &headers,
            body: &body,
            timeout_ms: REQUEST_TIMEOUT_MS,
        })
    };
    let mut resp = if endpoint_absent(url, "/_local/cache/clear") {
        post("/state-cache/clear")?
    } else {
        post("/_local/cache/clear")?
    };
    if resp.status == 404 && !endpoint_absent(url, "/_local/cache/clear") {
        mark_absent(url, "/_local/cache/clear");
        resp = post("/state-cache/clear")?;
    }
    match resp.status {
        200..=299 => Ok(()),
        409 => Err(OxaError::new(
            codes::BUSY,
            "the engine refused the hint clear while a request runs (HTTP 409)",
        )
        .with_hint("retry the clear when the engine is idle")),
        s => Err(http_error(s, &resp.body)),
    }
}

/// Endpoints found absent (404) per base URL, for the life of the process (A4): a plain engine
/// has no controller endpoints, a server without `/apply-template` has no exact counts; each is
/// asked once.
static ABSENT: std::sync::Mutex<Vec<(String, &'static str)>> = std::sync::Mutex::new(Vec::new());

/// `true` when `path` answered 404 on this endpoint before.
pub(crate) fn endpoint_absent(url: &Url, path: &'static str) -> bool {
    let key = url_string(url);
    ABSENT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|(u, p)| *u == key && *p == path)
}

/// Remember that `path` is absent on this endpoint.
pub(crate) fn mark_absent(url: &Url, path: &'static str) {
    let key = url_string(url);
    let mut g = ABSENT.lock().unwrap_or_else(|p| p.into_inner());
    if !g.iter().any(|(u, p)| *u == key && *p == path) {
        g.push((key, path));
    }
}

/// The base URL as text, `http://host:port/base` (brackets around an IPv6 host).
pub(crate) fn url_string(url: &Url) -> String {
    let host = if url.host.contains(':') {
        format!("[{}]", url.host)
    } else {
        url.host.clone()
    };
    format!("http://{host}:{}{}", url.port, url.base_path)
}

/// `Authorization: Bearer` when a key is set; nothing otherwise.
pub(crate) fn auth_headers(api_key: Option<&str>) -> Vec<(&'static str, String)> {
    match api_key.filter(|k| !k.is_empty()) {
        Some(k) => vec![("Authorization", format!("Bearer {k}"))],
        None => Vec::new(),
    }
}

/// GET `path` and parse its JSON. `Ok(None)` for a 404 (the endpoint is absent); any other
/// non-2xx status or a non-JSON body is an error.
fn get_json(url: &Url, path: &str, headers: &[(&'static str, String)]) -> Result<Option<Value>, OxaError> {
    let resp = http_request(&HttpRequest {
        method: "GET",
        url,
        path,
        headers,
        body: b"",
        timeout_ms: REQUEST_TIMEOUT_MS,
    })?;
    if resp.status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&resp.status) {
        return Err(http_error(resp.status, &resp.body));
    }
    serde_json::from_slice(&resp.body)
        .map(Some)
        .map_err(|e| OxaError::new(codes::ENGINE_HTTP, format!("GET {path} did not answer JSON: {e}")))
}

/// The error for a non-2xx answer. A 400 `exceed_context_size_error` (any status carrying that
/// error type) is `context_overflow`; a 503 is `engine_busy`; everything else is `engine_http`
/// with the engine's own message when it gives one.
pub(crate) fn http_error(status: u16, body: &[u8]) -> OxaError {
    http_error_with(status, body, None)
}

/// Gateway refusals whose classes must survive HTTP normalization. Credentials and ledger
/// availability can recover; the core applies bounded retries. A missing SDK is permanent.
const TYPED_CLOUD_REFUSALS: &[&str] = &[
    "cloud_credentials_unavailable",
    "cloud_sdk_unavailable",
    "cloud_ledger_unavailable",
];

/// [`http_error`] with the response's `Retry-After` (seconds). A 503 (the controller's queue-full
/// reply, `local_backend_busy`, or the engine's own `unavailable_error`) is `engine_busy`; its
/// message names `HTTP 503` and, with a `Retry-After`, `retry after <n> s`, which the core's
/// retry honours.
pub(crate) fn http_error_with(status: u16, body: &[u8], retry_after: Option<u32>) -> OxaError {
    let text = String::from_utf8_lossy(body);
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let err = parsed.as_ref().and_then(|v| v.get("error"));
    if err.is_some_and(is_overflow) {
        return overflow();
    }
    let detail = err
        .and_then(|e| e.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| text.trim().chars().take(300).collect());
    if status == 429 && err.and_then(|e| e.get("code").and_then(Value::as_str)) == Some("budget_exceeded") {
        return OxaError::new(
            codes::BUDGET_EXHAUSTED,
            format!("the cloud gateway refused the request (budget_exceeded): {detail}"),
        )
        .with_hint("wait for the next spend day; this is not retried");
    }
    if status == 503 {
        if let Some(code) = err
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("cloud_error"))
            .and_then(|e| e.get("code").and_then(Value::as_str))
            .filter(|c| TYPED_CLOUD_REFUSALS.contains(c))
        {
            // Preserve the class for the retry policy and the terminal outcome. Unknown 503
            // refusals retain their HTTP status and the ordinary transient-error policy.
            return OxaError::new(
                match code {
                    "cloud_credentials_unavailable" => "cloud_credentials_unavailable",
                    "cloud_ledger_unavailable" => "cloud_ledger_unavailable",
                    _ => "cloud_sdk_unavailable",
                },
                format!("the cloud gateway refused the request ({code}): {detail}"),
            );
        }
    }
    status_error(status, &detail, retry_after)
}

/// F3: the error object of an in-stream error (the controller's `event: error` after early
/// headers, `{"message","type","code"}`, or the engine's own `data: {"error":..}`), classified
/// as [`http_error`] classifies a status reply: `exceed_context_size_error` (type or code) ->
/// `context_overflow`; a status (a numeric `code`, a 3-digit string `code`, or a numeric
/// `status`; type `local_backend_busy` = 503) -> `engine_busy` for 503, else `engine_http` with
/// `HTTP <status>` in the message, so the core retries 5xx / 429 and not 4xx; no usable status ->
/// `stream_broken`.
pub(crate) fn stream_error(err: &Value) -> OxaError {
    if is_overflow(err) {
        return overflow();
    }
    let detail = err
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| err.as_str().map(str::to_string))
        .unwrap_or_else(|| err.to_string());
    let as_status = |v: &Value| -> Option<u16> {
        let n = match v {
            Value::Number(n) => n.as_u64()?,
            Value::String(s) if s.len() == 3 => s.parse::<u64>().ok()?,
            _ => return None,
        };
        u16::try_from(n).ok().filter(|s| (100..600).contains(s))
    };
    let status = err
        .get("code")
        .and_then(as_status)
        .or_else(|| err.get("status").and_then(as_status))
        .or_else(|| (err.get("type").and_then(Value::as_str) == Some("local_backend_busy")).then_some(503));
    match status {
        Some(s) => status_error(s, &detail, None),
        None => OxaError::new(
            codes::STREAM_BROKEN,
            format!("the engine reported an error mid-stream: {detail}"),
        ),
    }
}

fn is_overflow(err: &Value) -> bool {
    ["type", "code"]
        .iter()
        .any(|k| err.get(*k).and_then(Value::as_str) == Some("exceed_context_size_error"))
}

fn overflow() -> OxaError {
    OxaError::new(
        codes::CONTEXT_OVERFLOW,
        "the request does not fit the engine's context window",
    )
    .with_hint("the session is too long for this context; compaction shrinks it")
}

fn status_error(status: u16, detail: &str, retry_after: Option<u32>) -> OxaError {
    let msg = format!("the engine answered HTTP {status}: {detail}");
    if status == 503 {
        let msg = match retry_after {
            Some(n) => format!("{msg} (retry after {n} s)"),
            None => msg,
        };
        return OxaError::new(codes::ENGINE_BUSY, msg).with_hint(
            "the engine is busy (its queue is full or no model is loaded); the request is retried",
        );
    }
    OxaError::new(codes::ENGINE_HTTP, msg)
}

#[cfg(test)]
mod cloud_refusal_tests {
    use super::http_error_with;
    use oxa_proto::codes;

    fn body(code: &str, message: &str) -> Vec<u8> {
        format!(r#"{{"error":{{"message":"{message}","type":"cloud_error","param":null,"code":"{code}"}}}}"#)
            .into_bytes()
    }

    /// The daily budget refusal (429 `budget_exceeded`) is its own code: it is not retried
    /// (the core retries 429 `engine_http`) and the exit code can tell it apart.
    #[test]
    fn a_budget_429_is_budget_exhausted_and_not_retried() {
        let e = http_error_with(
            429,
            &body("budget_exceeded", "The daily cloud budget is exhausted."),
            None,
        );
        assert_eq!(e.code, codes::BUDGET_EXHAUSTED);
        assert!(
            e.message.contains("daily cloud budget is exhausted"),
            "{}",
            e.message
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("wait for the next spend day; this is not retried"),
            "shared normalization must not point native cloud users to a controller setting"
        );
        let plain = http_error_with(429, br#"{"error":{"message":"slow down"}}"#, None);
        assert_eq!(plain.code, codes::ENGINE_HTTP, "other 429s stay retryable");
    }

    /// A typed cloud refusal keeps the gateway's own message and class. Retry decisions use
    /// that class rather than depending on the wording of its human-readable message.
    #[test]
    fn a_cloud_503_shows_the_servers_message_not_engine_busy() {
        for code in ["cloud_credentials_unavailable", "cloud_sdk_unavailable"] {
            let message = "The Anthropic SDK is not installed on this host.";
            let e = http_error_with(503, &body(code, message), None);
            assert_eq!(e.code, code, "{code}");
            assert!(
                e.message.contains(message) && e.message.contains(code),
                "{}",
                e.message
            );
            assert!(!e.message.contains("HTTP 503"), "{}", e.message);
            assert!(
                e.hint.as_deref().is_none_or(|h| !h.contains("busy")),
                "{:?}",
                e.hint
            );
        }
    }

    /// Transient and unknown `cloud_*` 503s keep the `HTTP 503` marker, which
    /// oxa-core/src/retry.rs `http_status` reads (`engine_http` with status >= 500 is retryable).
    #[test]
    fn transient_or_unknown_cloud_503_keeps_the_http_marker_for_retry() {
        for code in [
            "cloud_upstream_error",
            "cloud_unavailable",
            "cloud_stream_error",
            "cloud_interrupted",
            "cloud_xyz",
        ] {
            let e = http_error_with(503, &body(code, "try later"), None);
            assert_eq!(e.code, codes::ENGINE_BUSY, "{code}");
            assert!(e.message.contains("HTTP 503"), "{code}: {}", e.message);
        }
        let e = http_error_with(503, br#"{"error":{"message":"x","type":"cloud_error"}}"#, None);
        assert!(e.message.contains("HTTP 503"), "{}", e.message);
    }

    #[test]
    fn a_ledger_refusal_keeps_its_class() {
        let e = http_error_with(503, &body("cloud_ledger_unavailable", "m"), None);
        assert_eq!(e.code, "cloud_ledger_unavailable");
        assert!(!e.message.contains("HTTP 503"), "{}", e.message);
    }

    #[test]
    fn a_plain_503_is_still_engine_busy() {
        let e = http_error_with(
            503,
            br#"{"error":{"message":"queue full","type":"local_backend_busy"}}"#,
            None,
        );
        assert_eq!(e.code, codes::ENGINE_BUSY);
    }
}

#[cfg(test)]
mod cancel_advertised_tests {
    use super::cancel_advertised;
    use serde_json::json;

    #[test]
    fn status_capabilities_array_advertises() {
        let status = json!({"capabilities": ["models", "ledger", "cancel"]});
        assert!(cancel_advertised(Some(&status), None));
    }

    #[test]
    fn model_limits_capabilities_do_not_advertise() {
        let status = json!({
            "capabilities": ["models"],
            "model_limits": [{"model": "m", "capabilities": {"cancel": true, "hints": ["cancel"]}}],
        });
        assert!(!cancel_advertised(Some(&status), None));
    }

    #[test]
    fn neither_advertises() {
        let status = json!({"capabilities": ["models"]});
        let props = json!({"oxe_hints": {"snapshot_keep": 2}});
        assert!(!cancel_advertised(Some(&status), Some(&props)));
        assert!(!cancel_advertised(None, None));
    }

    #[test]
    fn props_hints_advertise() {
        let props = json!({"oxe_hints": {"hints": ["cancel"]}});
        assert!(cancel_advertised(None, Some(&props)));
    }
}

#[cfg(test)]
mod modality_tests {
    use super::modality;
    use serde_json::json;

    #[test]
    fn null_thinking_is_unknown() {
        let status = json!({
            "model_limits": {"m": {"capabilities": {"thinking": null, "vision": false}}}
        });
        let (thinking, images) = modality(&status, "m");
        assert_eq!(thinking, None, "a null thinking value is unknown, not `no`");
        assert_eq!(images, Some(false));
    }

    #[test]
    fn non_bool_non_string_thinking_is_unknown() {
        let status = json!({
            "model_limits": {"m": {"capabilities": {"thinking": 7}}}
        });
        let (thinking, _) = modality(&status, "m");
        assert_eq!(thinking, None, "a numeric thinking value is unknown");
    }
}
