//! A14: the fake engine over HTTP + SSE. Owner: task A14. Signatures below are frozen; bodies are yours.
use crate::record::{parse_recorded, write_log};
use crate::{cache_bytes, Fault, Recorded, ScriptTurn};
use oxa_proto::{codes, Finish, OxaError, Usage};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Server-wide options (also the `oxa-fake` binary's flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeOpts {
    /// `--port` (0 = pick a free port).
    pub port: u16,
    pub n_ctx: u32,
    /// Kept for the flag set only: the real controller publishes no output limit, so the fake's
    /// `/_local/status` does not report it either (clients fall back to their default).
    pub max_output: u32,
    pub model: String,
    /// `--done-gap-ms`: pause between the last data chunk and `[DONE]`.
    pub done_gap_ms: u64,
    /// `--hold-headers <ms>`: applied to every chat request.
    pub hold_headers_ms: u64,
    /// `--comments-only <ms>`: applied to every chat request.
    pub comments_only_ms: u64,
    /// `--drop-at <bytes>`: applied to the first chat request only.
    pub drop_at: Option<usize>,
    /// Advertise `oxe_hints` on `/props` and accept the hints H2-H6. `false` = behave like a
    /// server without the to-build hints (they are ignored).
    pub hints: bool,
    /// Advertise the deliberate-cancel capability (`CANCEL_HINT`) in the top-level
    /// `capabilities` array of `/_local/status`, as a controller does. `false` (default) = the
    /// array is unchanged.
    pub status_cancel: bool,
    /// The rows of `/_local/status` `model_limits` (F4 / EG2), each with its `capabilities`
    /// block. Empty = one row for `model` (cap `n_ctx`, no `max_output`) with the Rust MoE
    /// engine's capabilities when `hints`, and without a `capabilities` block otherwise (an
    /// older controller).
    pub models: Vec<FakeModel>,
    /// Omit the `thinking` and `vision` fields of every `model_limits[].capabilities` block,
    /// modelling a controller that publishes no input-modality or thinking field (oxstack/ox-llm
    /// #67). `false` (default) = the fields are present.
    pub omit_modality: bool,
    /// `/props` answers 503 "no model loaded" (the controller with nothing loaded). Also
    /// switchable at run time ([`FakeServer::set_props_unloaded`]).
    pub props_unloaded: bool,
    /// `/_local/status` answers the controller's short `state: loading` body (no model, no
    /// context; F14). Also switchable at run time ([`FakeServer::set_loading`]).
    pub loading: bool,
    /// Serve the controller endpoints (`/_local/status`, `/_local/cache/clear`). `false` = a
    /// plain engine: they answer 404.
    pub controller: bool,
    /// F13: refuse (400) a request that re-sends an assistant tool call whose `arguments` is not
    /// valid JSON, as the engine does (`chat::request`). Default `true`.
    pub strict_tool_args: bool,
    /// `--hold-prefill <ms>`: hold the response headers of every H6 request (`max_tokens: 0`:
    /// the idle prefill, the boot preload, the park) this long, so a client can cancel it while
    /// it runs (ENG-002). A client that closes the connection meanwhile is recorded
    /// ([`ChatTiming::client_closed`]).
    pub hold_prefill_ms: u64,
    /// `--rewrite-resume-block <n>`: model the oxe engine as measured today (OXE-POSTCOMPACT-MISS):
    /// a request that rewrites the history the session holds (a compaction, a fork in place)
    /// resumes at the deepest `n`-token block of the shared prefix, and so does the NEXT request
    /// of that session, even a pure append. `None` (the default) = the correct engine: the next
    /// append continues the live state (C3).
    pub rewrite_resume_block: Option<usize>,
    /// B6: the chat template drops the reasoning of the assistant turns at or before the last
    /// user message (`preserve_thinking: false` on the Qwen templates); see
    /// [`crate::render_history`]. Default `false` (the template default keeps it).
    pub drop_reasoning: bool,
    /// B6: the usage leaves out `prompt_tokens_details.cached_tokens` (a server that does not
    /// report it). Default `false` (the oxe engine always reports it, 0 included).
    pub omit_cached_tokens: bool,
}

/// When one chat request ran on the fake (ENG-001 / ENG-002): milliseconds since the server
/// started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTiming {
    /// The request's `seq` (`None` for a body that could not be parsed or an H2 replay).
    pub seq: Option<usize>,
    pub max_tokens: Option<u32>,
    /// The request was read completely.
    pub start_ms: u64,
    /// The answer was written, or the client went away.
    pub end_ms: u64,
    /// The client closed the connection before the answer was complete (a cancel).
    pub client_closed: bool,
}

/// PROVISIONAL (CANCEL-GRACE-CPU): the fake's deliberate-cancel behaviour, switched at run time
/// with [`FakeServer::set_cancel`]. `Off` (the default) = not advertised, `POST /_local/cancel`
/// answers 404 like any unknown path. The other modes advertise `CANCEL_HINT` on `/props` and
/// serve the endpoint: `Advertised` answers 200, `Error` answers 500, `Hang` answers 200 only
/// after 10 s (the client has given up by then). Every request is recorded either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CancelMode {
    #[default]
    Off,
    Advertised,
    Error,
    Hang,
}

/// One `POST /_local/cancel` the fake received (the body's fields, the headers lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRecord {
    /// The body's `idempotency_key` ("" when absent).
    pub idem_key: String,
    /// The body's `session` ("" when absent).
    pub session: String,
    pub headers: Vec<(String, String)>,
}

/// One row of the fake controller's `model_limits` (lab `ctl_models.model_limits` and
/// `capabilities`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeModel {
    /// The `/v1/models` id (the row key).
    pub alias: String,
    /// `cap`: the context of the model's current mode.
    pub cap: u32,
    /// `max_output` (`None` = published as null: the controller sets no output default).
    pub max_output: Option<u32>,
    /// The `capabilities` block; `None` = the row has none (an older controller).
    pub caps: Option<FakeCaps>,
}

/// `model_limits[id].capabilities` (EG2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCaps {
    pub prefill_only: bool,
    /// `snapshot_hint`: H4 checkpoints kept per session (0 = no hint).
    pub snapshot_hint: u32,
    pub hint_clear: bool,
    pub preserve_thinking: bool,
    /// `park`: `"ram+disk"` (Rust MoE), `"disk"` (Rust Flash) or `"none"`.
    pub park: String,
}

impl FakeCaps {
    /// The Rust MoE engine's block (`ctl_models.capabilities`, family `rust`).
    pub fn rust_moe() -> FakeCaps {
        FakeCaps {
            prefill_only: true,
            snapshot_hint: 2,
            hint_clear: true,
            preserve_thinking: true,
            park: "ram+disk".to_string(),
        }
    }
    /// The Rust Flash engine's block (family `rust-flash`): no `preserve_thinking`, disk park.
    pub fn rust_flash() -> FakeCaps {
        FakeCaps {
            preserve_thinking: false,
            park: "disk".to_string(),
            ..FakeCaps::rust_moe()
        }
    }
    /// An engine that claims nothing (classic llama.cpp, unknown).
    pub fn none() -> FakeCaps {
        FakeCaps {
            prefill_only: false,
            snapshot_hint: 0,
            hint_clear: false,
            preserve_thinking: false,
            park: "none".to_string(),
        }
    }
}

impl FakeModel {
    /// A Rust MoE row.
    pub fn moe(alias: &str, cap: u32) -> FakeModel {
        FakeModel {
            alias: alias.to_string(),
            cap,
            max_output: None,
            caps: Some(FakeCaps::rust_moe()),
        }
    }
    /// A Flash row (e.g. `FakeModel::flash("flash", 16384, Some(2048))`).
    pub fn flash(alias: &str, cap: u32, max_output: Option<u32>) -> FakeModel {
        FakeModel {
            alias: alias.to_string(),
            cap,
            max_output,
            caps: Some(FakeCaps::rust_flash()),
        }
    }
}

impl Default for FakeOpts {
    fn default() -> Self {
        FakeOpts {
            port: 0,
            n_ctx: 1 << 20,
            max_output: 4096,
            model: "fake-model".to_string(),
            done_gap_ms: 0,
            hold_headers_ms: 0,
            comments_only_ms: 0,
            drop_at: None,
            hints: true,
            status_cancel: false,
            omit_modality: false,
            models: Vec::new(),
            props_unloaded: false,
            loading: false,
            controller: true,
            strict_tool_args: true,
            hold_prefill_ms: 0,
            rewrite_resume_block: None,
            drop_reasoning: false,
            omit_cached_tokens: false,
        }
    }
}

/// A running fake engine on 127.0.0.1 (threads, `std::net`). Implements plan sec. 3:
/// `POST /v1/chat/completions` (SSE: OpenAI chunks with `reasoning_content`, `content`,
/// `tool_calls` deltas; a final chunk with `finish_reason`, `usage` incl.
/// `prompt_tokens_details.cached_tokens`, and `timings`; `: prefill <done>/<total> <s>s` and
/// `: queued [pos=<n>] wait_s=<s>` comments; 400 `exceed_context_size_error`), `GET
/// /v1/models`, `GET /_local/status` (with `model_limits` and capabilities), `GET /props`,
/// `POST /tokenize`, `POST /_local/cache/clear` and `POST /state-cache/clear` (EG5). Cache model, H2 (same key + same body = the same reply replayed without
/// consuming a turn), H4 (`timings.snap_pos`) and H6 as in the crate docs. Identical observable
/// behaviour to `ScriptedEngine` for the same script.
pub struct FakeServer {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    state: std::sync::Arc<std::sync::Mutex<Vec<Recorded>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
}

/// State shared by the accept loop and the connection threads.
struct Shared {
    opts: FakeOpts,
    /// Turns, cache model and idempotency memory. Locked for decisions only, never while
    /// writing to a socket.
    core: Mutex<Core>,
    /// Every recorded request (the `requests()` list). Always locked after `core`.
    state: Arc<Mutex<Vec<Recorded>>>,
    /// Run-time switches (initialised from `opts`).
    props_unloaded: AtomicBool,
    loading: AtomicBool,
    clear_busy: AtomicBool,
    /// EG5: the hint of every accepted hint clear, in order (the `clears()` list).
    clears: Mutex<Vec<String>>,
    /// ENG-001: chat requests being answered right now, and the most ever at once.
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// ENG-001 / ENG-002: one row per chat request, in completion order.
    timings: Mutex<Vec<ChatTiming>>,
    started: Instant,
    /// Requests waiting in a hold (their sockets are non-blocking meanwhile): checked for a
    /// client close whenever a new chat request arrives, so a cancel followed at once by the
    /// next request is never counted as overlap.
    holds: Mutex<Vec<(u64, TcpStream)>>,
    /// Held requests found closed: id -> the ms it was noticed (already out of `in_flight`).
    reaped: Mutex<Vec<(u64, u64)>>,
    next_id: AtomicU64,
    /// Every request line seen (method, path), in arrival order (A4).
    hits: Mutex<Vec<(String, String)>>,
    /// PROVISIONAL (CANCEL-GRACE-CPU): the deliberate-cancel mode and the notices received.
    cancel_mode: Mutex<CancelMode>,
    cancels: Mutex<Vec<CancelRecord>>,
}

struct Core {
    turns: VecDeque<ScriptTurn>,
    /// Per session: the bytes the engine holds (last prompt + reply).
    cache: Vec<(String, Vec<u8>)>,
    /// H2: complete replies by idempotency key.
    idem: Vec<IdemEntry>,
    /// `rewrite_resume_block`: per session, the block-floored resume point of its last rewrite,
    /// which caps the next request.
    sticky: Vec<(String, usize)>,
}

struct IdemEntry {
    key: String,
    /// The request body the reply was generated for.
    body: Vec<u8>,
    /// The complete SSE response body.
    response: Vec<u8>,
}

impl FakeServer {
    pub fn start(script: Vec<ScriptTurn>, opts: FakeOpts) -> Result<FakeServer, OxaError> {
        let listener = TcpListener::bind(("127.0.0.1", opts.port))
            .map_err(|e| OxaError::io("bind fake engine port", &e))?;
        let port = listener
            .local_addr()
            .map_err(|e| OxaError::io("fake engine address", &e))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|e| OxaError::io("fake engine listener", &e))?;
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Shared {
            props_unloaded: AtomicBool::new(opts.props_unloaded),
            loading: AtomicBool::new(opts.loading),
            clear_busy: AtomicBool::new(false),
            clears: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            timings: Mutex::new(Vec::new()),
            started: Instant::now(),
            holds: Mutex::new(Vec::new()),
            reaped: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            hits: Mutex::new(Vec::new()),
            cancel_mode: Mutex::new(CancelMode::Off),
            cancels: Mutex::new(Vec::new()),
            opts,
            core: Mutex::new(Core {
                turns: script.into(),
                cache: Vec::new(),
                idem: Vec::new(),
                sticky: Vec::new(),
            }),
            state: state.clone(),
        });
        let (stop2, shared2) = (stop.clone(), shared.clone());
        let thread = std::thread::spawn(move || accept_loop(listener, stop2, shared2));
        Ok(FakeServer {
            port,
            stop,
            state,
            thread: Some(thread),
            shared,
        })
    }
    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    /// Every chat request so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().clone()
    }
    /// Forget cached state (an engine restart) without closing the port.
    pub fn restart(&self) {
        lock(&self.shared.core).cache.clear();
    }
    /// EG5: the hint of every accepted hint clear (`/_local/cache/clear` or
    /// `/state-cache/clear`, scope `hint`), in arrival order.
    pub fn clears(&self) -> Vec<String> {
        lock(&self.shared.clears).clone()
    }
    /// PROVISIONAL (CANCEL-GRACE-CPU): set the deliberate-cancel mode (see [`CancelMode`]).
    pub fn set_cancel(&self, mode: CancelMode) {
        *lock(&self.shared.cancel_mode) = mode;
    }
    /// PROVISIONAL (CANCEL-GRACE-CPU): every `POST /_local/cancel` received, in arrival order.
    pub fn cancels(&self) -> Vec<CancelRecord> {
        lock(&self.shared.cancels).clone()
    }
    /// Every request line seen (method, path), in arrival order, any endpoint.
    pub fn request_log(&self) -> Vec<(String, String)> {
        lock(&self.shared.hits).clone()
    }
    /// `/props` answers 503 "no model loaded" while `true` (F4: the controller before a load).
    pub fn set_props_unloaded(&self, unloaded: bool) {
        self.shared.props_unloaded.store(unloaded, Ordering::SeqCst);
    }
    /// `/_local/status` answers the controller's `state: loading` body while `true` (F14).
    pub fn set_loading(&self, loading: bool) {
        self.shared.loading.store(loading, Ordering::SeqCst);
    }
    /// The hint clears answer 409 (a request is running) while `true`.
    pub fn set_clear_busy(&self, busy: bool) {
        self.shared.clear_busy.store(busy, Ordering::SeqCst);
    }
    /// The cache-model bytes the fake holds for session `session` (`None` = nothing held).
    /// After a reply that is the prompt plus the reply minus its last token (F1).
    pub fn held(&self, session: &str) -> Option<Vec<u8>> {
        lock(&self.shared.core)
            .cache
            .iter()
            .find(|(s, _)| s == session)
            .map(|(_, held)| held.clone())
    }
    /// How many requests `method path` the fake received (any endpoint, any answer).
    pub fn hits(&self, method: &str, path: &str) -> usize {
        lock(&self.shared.hits)
            .iter()
            .filter(|(m, p)| m == method && p == path)
            .count()
    }
    /// ENG-001: the most chat requests the fake was answering at the same time.
    pub fn max_in_flight(&self) -> usize {
        self.shared.max_in_flight.load(Ordering::SeqCst)
    }
    /// ENG-001: chat requests being answered right now.
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }
    /// ENG-001 / ENG-002: when each finished chat request ran, in completion order.
    pub fn timings(&self) -> Vec<ChatTiming> {
        lock(&self.shared.timings).clone()
    }
    /// Stop accepting and join the thread. Also done on drop.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn accept_loop(listener: TcpListener, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let sh = shared.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &sh);
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

struct Request {
    method: String,
    path: String,
    /// Names lowercased.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream, sh: &Shared) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    lock(&sh.hits).push((req.method.clone(), req.path.clone()));
    match (req.method.as_str(), req.path.as_str()) {
        // Every `model_limits` row is a servable model (the controller's aliases).
        ("GET", "/v1/models") => {
            let data: Vec<Value> = model_rows(&sh.opts)
                .iter()
                .map(|r| json!({"id": r.alias, "object": "model"}))
                .collect();
            respond_json(&mut stream, 200, &json!({"object": "list", "data": data}))
        }
        ("GET", "/_local/status") if sh.opts.controller => {
            let v = if sh.loading.load(Ordering::SeqCst) {
                loading_status()
            } else {
                controller_status(&sh.opts)
            };
            respond_json(&mut stream, 200, &v)
        }
        ("GET", "/props") => {
            if sh.props_unloaded.load(Ordering::SeqCst) {
                let v = json!({"error": {"code": 503, "message": "no model loaded", "type": "unavailable_error"}});
                respond_json(&mut stream, 503, &v)
            } else {
                let mode = *lock(&sh.cancel_mode);
                respond_json(&mut stream, 200, &engine_props(&sh.opts, mode))
            }
        }
        ("POST", "/_local/cancel") if *lock(&sh.cancel_mode) != CancelMode::Off => {
            cancel_request(&mut stream, sh, &req)
        }
        ("POST", "/_local/cache/clear") if sh.opts.controller => clear(&mut stream, sh, &req.body),
        ("POST", "/state-cache/clear") => clear(&mut stream, sh, &req.body),
        ("POST", "/apply-template") => match apply_template(&req) {
            Ok(v) => respond_json(&mut stream, 200, &v),
            Err(msg) => respond_json(&mut stream, 400, &error_value(codes::INVALID, &msg)),
        },
        ("POST", "/tokenize") => match tokenize(&req.body) {
            Ok(v) => respond_json(&mut stream, 200, &v),
            Err(msg) => respond_json(&mut stream, 400, &error_value(codes::INVALID, &msg)),
        },
        ("POST", "/v1/chat/completions") => chat(&mut stream, sh, &req),
        _ => respond_json(&mut stream, 404, &error_value("not_found", "not found")),
    }
}

/// `/_local/status` in the real controller's shape (the lab's `StatusMixin.status`,
/// `runtime/ctl_status.py`; key order as `tests/test_controller_contract.py` `STATUS_KEYS`),
/// reduced to the fields a client reads plus a few neighbours. The context is `context_size`
/// and `context.cap` (no `n_ctx`), the queue is `queued_requests` (no `queue`), and there is no
/// output limit: the controller publishes none, so neither does the fake (`max_output` is
/// never reported).
fn controller_status(o: &FakeOpts) -> Value {
    json!({
        "state": "ready", "validated": true, "download_state": "ready",
        "model": o.model, "selected_model": o.model, "selection_error": null,
        "backend": "cpu", "context_size": o.n_ctx, "threads": null,
        "active_requests": 0, "queued_requests": 0, "idle_unload_seconds": 600,
        "loaded_model": o.model, "backend_kind": "oxe", "engine": "rust", "engine_note": null,
        "capabilities": status_capabilities(o),
        "active": {"phase": "idle", "model": o.model, "seconds": null, "queued_requests": 0,
                   "prompt_tokens_done": null, "prompt_tokens_total": null, "tokens_out": null,
                   "tokens_per_second": null},
        "context": {"cap": o.n_ctx, "mode": null, "modes": []},
        "chat_url": "http://127.0.0.1:8092", "api_base": "http://127.0.0.1:8092/v1",
        "model_limits": model_limits(o),
    })
}

/// The top-level `capabilities` of `/_local/status`: the controller's own list, plus
/// [`CANCEL_HINT`] when [`FakeOpts::status_cancel`] is set.
fn status_capabilities(o: &FakeOpts) -> Value {
    let mut caps = vec![
        "models",
        "storage",
        "harness",
        "harness_learning",
        "ledger",
        "active",
        "openai_aliases",
        "state_cache",
        "context_modes",
        "preload",
    ];
    if o.status_cancel {
        caps.push(oxa_proto::CANCEL_HINT);
    }
    json!(caps)
}

/// The served models: [`FakeOpts::models`], or one row for `model` when it is empty.
fn model_rows(o: &FakeOpts) -> Vec<FakeModel> {
    if o.models.is_empty() {
        vec![FakeModel {
            alias: o.model.clone(),
            cap: o.n_ctx,
            max_output: None,
            caps: o.hints.then(FakeCaps::rust_moe),
        }]
    } else {
        o.models.clone()
    }
}

/// The context of the model a request names: its row's `cap` (the controller loads that model in
/// its mode), else `n_ctx`.
fn n_ctx_for(o: &FakeOpts, model: &str) -> u32 {
    model_rows(o)
        .iter()
        .find(|r| r.alias == model)
        .map_or(o.n_ctx, |r| r.cap)
}

/// `model_limits` (lab `ctl_models.model_limits`): per id `{cap, mode, modes, max_output,
/// capabilities}`; see [`FakeOpts::models`].
fn model_limits(o: &FakeOpts) -> Value {
    let mut out = serde_json::Map::new();
    for r in model_rows(o) {
        let mut row = json!({"cap": r.cap, "mode": null, "modes": [], "max_output": r.max_output});
        if let Some(c) = &r.caps {
            let mut caps = json!({
                "prefill_only": c.prefill_only, "snapshot_hint": c.snapshot_hint,
                "turn_kind": c.prefill_only, "hint_clear": c.hint_clear,
                "preserve_thinking": c.preserve_thinking, "mtp": null, "max_context": r.cap,
                "slots": 1, "park": c.park,
            });
            if !o.omit_modality {
                caps["thinking"] = json!("toggle");
                caps["vision"] = json!(false);
            }
            row["capabilities"] = caps;
        }
        out.insert(r.alias, row);
    }
    Value::Object(out)
}

/// The controller's `/_local/status` while it loads a model (`ctl_status.status` when its lock
/// is held): no model, no context (F14).
fn loading_status() -> Value {
    json!({
        "state": "loading", "active_requests": 0,
        "capabilities": ["models", "storage", "harness", "ledger", "active", "state_cache"],
        "active": {"phase": "loading", "model": null, "seconds": null, "queued_requests": 0,
                   "prompt_tokens_done": null, "prompt_tokens_total": null, "tokens_out": null,
                   "tokens_per_second": null},
    })
}

/// EG5: `{"scope": "hint", "hint": h}` drops what the fake holds for session `h` alone (other
/// sessions, a parent's included, stay). The hint is 1-128 characters of `[A-Za-z0-9._-]`;
/// anything else or a missing hint is 400; an unknown hint is 200 with `sessions: 0`. 409
/// while [`FakeServer::set_clear_busy`] is on. Other scopes are not modelled (400).
fn clear(s: &mut TcpStream, sh: &Shared, body: &[u8]) -> io::Result<()> {
    if sh.clear_busy.load(Ordering::SeqCst) {
        let v = json!({"error": {"code": 409, "message": "a request is running", "type": "conflict_error"}});
        return respond_json(s, 409, &v);
    }
    let bad = |s: &mut TcpStream, msg: &str| {
        respond_json(
            s,
            400,
            &json!({"error": {"code": 400, "message": msg, "type": "invalid_request_error"}}),
        )
    };
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return bad(s, "the body is not JSON"),
    };
    if v.get("scope").and_then(Value::as_str) != Some("hint") {
        return bad(s, "the fake models only scope `hint`");
    }
    let hint = match v.get("hint").and_then(Value::as_str) {
        Some(h) => h.to_string(),
        None => return bad(s, "missing `hint`"),
    };
    let valid = (1..=128).contains(&hint.len())
        && hint
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !valid {
        return bad(s, "`hint` must be 1-128 characters of [A-Za-z0-9._-]");
    }
    let (sessions, freed) = {
        let mut core = lock(&sh.core);
        let freed: usize = core
            .cache
            .iter()
            .filter(|(h, _)| *h == hint)
            .map(|(_, b)| b.len())
            .sum();
        let before = core.cache.len();
        core.cache.retain(|(h, _)| *h != hint);
        (before - core.cache.len(), freed)
    };
    lock(&sh.clears).push(hint);
    let v = json!({"freed_bytes": freed, "used_bytes": 0, "sessions": sessions, "released": 0,
                   "ram_states": sessions, "legacy_entries": 0, "echo_records": 0});
    respond_json(s, 200, &v)
}

/// PROVISIONAL (CANCEL-GRACE-CPU): `POST /_local/cancel`, recorded, then answered per the mode.
/// `Hang` sleeps past the client's 1 s bound before it answers; the client is gone by then.
fn cancel_request(s: &mut TcpStream, sh: &Shared, req: &Request) -> io::Result<()> {
    let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    lock(&sh.cancels).push(CancelRecord {
        idem_key: field("idempotency_key"),
        session: field("session"),
        headers: req.headers.clone(),
    });
    match *lock(&sh.cancel_mode) {
        CancelMode::Error => respond_json(s, 500, &error_value(codes::INTERNAL, "cancel failed")),
        CancelMode::Hang => {
            std::thread::sleep(Duration::from_secs(10));
            respond_json(s, 200, &json!({"cancelled": true}))
        }
        _ => respond_json(s, 200, &json!({"cancelled": true})),
    }
}

/// `/props` in the oxe engine's shape (`service.rs` `props`). `oxe_hints` (the H4 / H6
/// capability block) only when the fake implements the hints (`FakeOpts::hints`); the
/// deliberate-cancel hint is added when `cancel` is not [`CancelMode::Off`].
fn engine_props(o: &FakeOpts, cancel: CancelMode) -> Value {
    let mut v = json!({
        "default_generation_settings": {"n_ctx": o.n_ctx},
        "total_slots": 1,
        "model_alias": o.model,
        "is_sleeping": false,
    });
    if o.hints {
        v["oxe_hints"] = json!({"snapshot_keep": 2, "prefill_only": true, "hint_clear": true});
        if cancel != CancelMode::Off {
            v["oxe_hints"]["hints"] = json!([oxa_proto::CANCEL_HINT]);
        }
    }
    v
}

/// `POST /apply-template` (the engine's `service.rs`): the chat body (`messages`, `tools`, ...;
/// `max_tokens` optional) rendered by the fake's template, which is [`cache_bytes`]: `prompt` is
/// that text; with `"return_tokens": true` also `tokens` (one per byte, so `tokens.len()` is
/// exactly the `prompt_tokens` a chat request with this body reports, framing bytes included)
/// and `generation_prompt` (empty: the fake renders no opener).
fn apply_template(req: &Request) -> Result<Value, String> {
    let v: Value =
        serde_json::from_slice(&req.body).map_err(|e| format!("apply-template body is not JSON: {e}"))?;
    let rec = match parse_recorded(0, &req.headers, &req.body) {
        Ok(r) => r,
        // `max_tokens` is optional here: parse again with one.
        Err(_) if v.get("max_tokens").is_none() && v.is_object() => {
            let mut b = b"{\"max_tokens\":0,".to_vec();
            let rest = &req.body[req.body.iter().position(|&c| c == b'{').unwrap_or(0) + 1..];
            if rest.iter().all(u8::is_ascii_whitespace) || rest.trim_ascii_start().starts_with(b"}") {
                b.pop();
            }
            b.extend_from_slice(rest);
            parse_recorded(0, &req.headers, &b).map_err(|e| e.message)?
        }
        Err(e) => return Err(e.message),
    };
    let bytes = cache_bytes(&rec.system, &rec.tools_json, &rec.messages);
    let mut out = json!({"prompt": String::from_utf8_lossy(&bytes)});
    if v.get("return_tokens").and_then(Value::as_bool) == Some(true) {
        out["generation_prompt"] = json!("");
        out["tokens"] = json!(bytes.iter().map(|&b| u32::from(b)).collect::<Vec<u32>>());
    }
    Ok(out)
}

fn tokenize(body: &[u8]) -> Result<Value, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("tokenize body is not JSON: {e}"))?;
    let text = v
        .get("content")
        .and_then(Value::as_str)
        .ok_or("`content` must be a string")?;
    // One token per byte, the fake's cache model (crate docs).
    let tokens: Vec<u32> = text.bytes().map(u32::from).collect();
    Ok(json!({ "tokens": tokens }))
}

fn read_request(s: &mut TcpStream) -> Result<Request, String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 1 << 20 {
            return Err("headers too large".into());
        }
        let n = s.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before headers".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| "headers are not UTF-8".to_string())?;
    let mut lines = head.split("\r\n");
    let start = lines.next().unwrap_or("");
    let mut parts = start.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("");
    let path = target.split('?').next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > 64 << 20 {
        return Err("body too large".into());
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed inside the body".into());
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn respond(s: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}

fn respond_json(s: &mut TcpStream, status: u16, v: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(v).expect("json serialises");
    respond(s, status, "application/json", &body)
}

fn error_value(code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

fn overflow_value(prompt: usize, n_ctx: u32) -> Value {
    json!({"error": {
        "code": 400,
        "type": "exceed_context_size_error",
        "message": "the request exceeds the available context size",
        "n_prompt_tokens": prompt,
        "n_ctx": n_ctx,
    }})
}

/// What to do with one chat request, decided under the lock.
enum Plan {
    /// A complete JSON answer (errors, faults), with `Retry-After` when given.
    Json {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<u32>,
    },
    /// Close the connection without an answer.
    Refuse,
    /// An SSE stream.
    Stream(StreamPlan),
}

fn json_plan(status: u16, v: &Value) -> Plan {
    Plan::Json {
        status,
        body: json_bytes(v),
        retry_after: None,
    }
}

struct StreamPlan {
    hold_ms: u64,
    comments_ms: u64,
    /// Comment frames sent one by one (10 ms apart) before `body` (`Fault::Queued`,
    /// `Fault::StreamError`).
    pre: Vec<Vec<u8>>,
    /// The SSE body (data events, comments, `[DONE]`).
    body: Vec<u8>,
    /// Index in `body` where `data: [DONE]` begins (`body.len()` when the body has none).
    done_at: usize,
    done_gap_ms: u64,
    /// Close after this many body bytes (a failed reply).
    cut: Option<usize>,
    /// Omit the terminating zero-length chunk after `[DONE]`.
    early_close: bool,
    /// `Fault::Stall`: after this many body bytes, silence for this many ms.
    stall: Option<(usize, u64)>,
}

impl StreamPlan {
    fn new(body: Vec<u8>, done_at: usize) -> StreamPlan {
        StreamPlan {
            hold_ms: 0,
            comments_ms: 0,
            pre: Vec::new(),
            body,
            done_at,
            done_gap_ms: 0,
            cut: None,
            early_close: false,
            stall: None,
        }
    }
}

/// One chat request, counted in flight from the moment it is read until its answer is written
/// or the client went away (ENG-001), with its [`ChatTiming`].
fn chat(s: &mut TcpStream, sh: &Shared, req: &Request) -> io::Result<()> {
    reap(sh);
    let id = sh.next_id.fetch_add(1, Ordering::SeqCst);
    let now = sh.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    sh.max_in_flight.fetch_max(now, Ordering::SeqCst);
    let start_ms = sh.started.elapsed().as_millis() as u64;
    let mut meta = (None, None);
    let r = answer_chat(s, sh, req, &mut meta, id);
    let reaped = {
        let mut g = lock(&sh.reaped);
        let at = g.iter().position(|(i, _)| *i == id);
        at.map(|k| g.remove(k).1)
    };
    let (end_ms, client_closed) = match reaped {
        Some(t) => (t, true),
        None => {
            sh.in_flight.fetch_sub(1, Ordering::SeqCst);
            (sh.started.elapsed().as_millis() as u64, !matches!(r, Ok(false)))
        }
    };
    lock(&sh.timings).push(ChatTiming {
        seq: meta.0,
        max_tokens: meta.1,
        start_ms,
        end_ms,
        client_closed,
    });
    r.map(|_| ())
}

/// `true` when the peer closed this (non-blocking) socket.
fn peer_closed(s: &TcpStream) -> bool {
    let mut probe = [0u8; 1];
    match s.peek(&mut probe) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::Interrupted,
    }
}

/// Take every held request whose client went away out of `in_flight` (noting when).
fn reap(sh: &Shared) {
    let mut holds = lock(&sh.holds);
    let now = sh.started.elapsed().as_millis() as u64;
    holds.retain(|(id, st)| {
        if peer_closed(st) {
            sh.in_flight.fetch_sub(1, Ordering::SeqCst);
            lock(&sh.reaped).push((*id, now));
            false
        } else {
            true
        }
    });
}

/// Answer one chat request; `Ok(true)` when the client closed the connection first.
fn answer_chat(
    s: &mut TcpStream,
    sh: &Shared,
    req: &Request,
    meta: &mut (Option<usize>, Option<u32>),
    id: u64,
) -> io::Result<bool> {
    match plan_chat(sh, req, meta) {
        Plan::Json {
            status,
            body,
            retry_after,
        } => match retry_after {
            None => respond(s, status, "application/json", &body).map(|_| false),
            Some(n) => {
                let head = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nRetry-After: {n}\r\nConnection: close\r\n\r\n",
                    reason(status),
                    body.len()
                );
                s.write_all(head.as_bytes())?;
                s.write_all(&body)?;
                s.flush().map(|_| false)
            }
        },
        // Dropping the stream closes the socket with no answer.
        Plan::Refuse => Ok(false),
        Plan::Stream(p) => write_stream(s, p, sh, id),
    }
}

/// Wait `ms`, watching the socket: `true` as soon as the client closed it (a cancel). While it
/// waits the request is registered in `Shared::holds`, so a new request finds it closed too.
fn wait_watching(s: &TcpStream, ms: u64, sh: &Shared, id: u64) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    let _ = s.set_nonblocking(true);
    if let Ok(c) = s.try_clone() {
        lock(&sh.holds).push((id, c));
    }
    let is_reaped = || lock(&sh.reaped).iter().any(|(i, _)| *i == id);
    let mut closed = false;
    while Instant::now() < end {
        reap(sh);
        if is_reaped() {
            closed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2).min(end.saturating_duration_since(Instant::now())));
    }
    lock(&sh.holds).retain(|(i, _)| *i != id);
    // A close found between the last check and the removal still counts.
    closed = closed || is_reaped();
    let _ = s.set_nonblocking(false);
    closed
}

fn split_chunks(s: &str, chunk: usize) -> Vec<&str> {
    if chunk == 0 || s.len() <= chunk {
        return vec![s];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + chunk).min(s.len());
        while end < s.len() && !s.is_char_boundary(end) {
            end += 1;
        }
        out.push(&s[start..end]);
        start = end;
    }
    out
}

fn sse_data(out: &mut Vec<u8>, v: &Value) {
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&serde_json::to_vec(v).expect("json serialises"));
    out.extend_from_slice(b"\n\n");
}

fn delta_value(delta: Value) -> Value {
    json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk",
           "choices": [{"index": 0, "delta": delta, "finish_reason": Value::Null}]})
}

/// The engine's usage object (`api.rs`): `completion_tokens` counts every generated token,
/// the reasoning included; `completion_tokens_details.reasoning_tokens` is the reasoning part
/// (here: its bytes). `prompt_tokens_details` is left out when `cached_missing`.
fn usage_value(u: Usage, reasoning_tokens: usize) -> Value {
    let mut v = json!({
        "prompt_tokens": u.prompt,
        "completion_tokens": u.completion,
        "total_tokens": u.prompt + u.completion,
        "completion_tokens_details": {"reasoning_tokens": reasoning_tokens},
    });
    if !u.cached_missing {
        v["prompt_tokens_details"] = json!({"cached_tokens": u.cached});
    }
    v
}

/// H4 in the reply's timings: no key without the hint, else a position or null + reason.
enum Snap {
    NotAsked,
    Pos(u32),
    Null(String),
}

fn timings_value(prompt: usize, cached: usize, predicted: u32, snap: &Snap) -> Value {
    let mut v = json!({
        "cache_n": cached,
        "prompt_n": prompt - cached,
        "prompt_ms": 0.0,
        "predicted_n": predicted,
        "predicted_ms": 0.0,
    });
    match snap {
        Snap::NotAsked => {}
        Snap::Pos(p) => v["snap_pos"] = json!(p),
        Snap::Null(why) => {
            v["snap_pos"] = Value::Null;
            v["snap_reason"] = json!(why);
        }
    }
    v
}

fn finish_value(f: Finish) -> &'static str {
    match f {
        Finish::Stop => "stop",
        Finish::ToolCalls => "tool_calls",
        Finish::Length => "length",
    }
}

/// F13: the first re-sent assistant tool call whose `arguments` is not JSON, as the engine's
/// 400 message (`chat::request`: "Failed to parse tool call arguments as JSON").
pub(crate) fn bad_tool_args(messages: &[oxa_proto::WireMsg]) -> Option<String> {
    messages
        .iter()
        .filter(|m| m.role == oxa_proto::Role::Assistant)
        .flat_map(|m| m.tool_calls.iter())
        .find_map(|c| {
            serde_json::from_str::<Value>(&c.args_raw)
                .err()
                .map(|e| format!("Failed to parse tool call arguments as JSON: {e}"))
        })
}

/// F6: the reply the engine produces for `turn` with room for `limit` tokens: unchanged when
/// its completion fits; else cut at `limit` tokens of content (reasoning first, then text; one
/// token = one byte; tool calls are dropped, the engine streams only complete calls) with
/// `finish_reason: "length"` and `completion = limit`. Returns (message, finish, completion).
pub(crate) fn generate(turn: &ScriptTurn, limit: u32) -> (oxa_proto::WireMsg, Finish, u32) {
    let msg = turn.message();
    let natural = turn.completion_tokens.unwrap_or(msg.to_json().len() as u32 + 1);
    if natural <= limit {
        return (msg, turn.finish, natural);
    }
    let mut budget = limit as usize;
    let mut cut = |s: &Option<String>| -> Option<String> {
        let t = s.as_ref()?;
        let mut end = budget.min(t.len());
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        budget -= end;
        Some(t[..end].to_string())
    };
    let reasoning = cut(&msg.reasoning);
    let text = cut(&msg.content);
    let out = oxa_proto::WireMsg {
        role: oxa_proto::Role::Assistant,
        content: text,
        reasoning,
        tool_calls: Vec::new(),
        tool_call_id: None,
    };
    (out, Finish::Length, limit)
}

/// What the fake holds after a reply (F1): the prompt plus the reply's bytes minus the last
/// generated token, which the engine never feeds.
pub(crate) fn held_after(prompt: &[u8], reply: &oxa_proto::WireMsg) -> Vec<u8> {
    // The reply's bytes are its JSON + `\n` (the next prompt repeats exactly these); the last
    // one, the `\n`, is never fed, so the next request reuses `prompt + completion - 1`.
    let mut held = prompt.to_vec();
    held.extend_from_slice(&reply.to_json());
    held
}

/// The queued comment of the unified format (F10): `: queued pos=<n> wait_s=<s>` (the
/// controller) or `: queued wait_s=<s>` (the engine).
fn queued_comment(pos: Option<u32>, wait_s: u32) -> Vec<u8> {
    match pos {
        Some(p) => format!(": queued pos={p} wait_s={wait_s}\n\n").into_bytes(),
        None => format!(": queued wait_s={wait_s}\n\n").into_bytes(),
    }
}

/// The in-stream error object (F3): the controller's `error_event` / the engine's
/// `error_json`, `{"message","type","code":status}`.
pub(crate) fn stream_error_value(status: u16, error_type: &str, prompt: usize, n_ctx: u32) -> Value {
    if error_type == "exceed_context_size_error" {
        return overflow_value(prompt, n_ctx);
    }
    json!({"error": {"code": status, "message": format!("the engine failed with HTTP {status}"), "type": error_type}})
}

fn plan_chat(sh: &Shared, req: &Request, meta: &mut (Option<usize>, Option<u32>)) -> Plan {
    let o = &sh.opts;
    let mut rec = match parse_recorded(0, &req.headers, &req.body) {
        Ok(r) => r,
        Err(e) => return json_plan(400, &error_value(e.code, &e.message)),
    };
    let mut core = lock(&sh.core);

    // H2: same key + same body replays the stored reply; no turn, no record.
    if o.hints && !rec.idem_key.is_empty() {
        if let Some(e) = core.idem.iter().find(|e| e.key == rec.idem_key) {
            if e.body == req.body {
                let len = e.response.len();
                return Plan::Stream(StreamPlan::new(e.response.clone(), len));
            }
            return json_plan(
                409,
                &error_value("idempotency_conflict", "Idempotency-Key reused with another body"),
            );
        }
    }

    let mut seen = lock(&sh.state);
    rec.seq = seen.len();
    *meta = (Some(rec.seq), Some(rec.max_tokens));
    let n_ctx = n_ctx_for(o, &rec.model);
    let prompt = cache_bytes(
        &rec.system,
        &rec.tools_json,
        &crate::render_history(&rec.messages, o.drop_reasoning),
    );
    let plen = prompt.len();
    // `Fault::Cold`: the engine holds nothing for the session when this request arrives (and no
    // shared anchor either: a server without a prefix cache).
    let cold = rec.max_tokens > 0 && matches!(core.turns.front(), Some(t) if t.fault == Some(Fault::Cold));
    if cold {
        core.cache.retain(|(s, _)| *s != rec.session);
    }
    let mut cached = crate::turn::cached_of(&core.cache, &rec.session, &prompt, cold);
    if let Some(block) = o.rewrite_resume_block.filter(|b| *b > 0) {
        let held_len = core
            .cache
            .iter()
            .find(|(s, _)| *s == rec.session)
            .map(|(_, h)| h.len());
        let sticky = core
            .sticky
            .iter()
            .position(|(s, _)| *s == rec.session)
            .map(|i| core.sticky.remove(i).1);
        if let Some(cap) = sticky {
            cached = cached.min(cap);
        } else if held_len.is_some_and(|n| cached < n) {
            cached = cached / block * block;
            core.sticky.push((rec.session.clone(), cached));
        }
    }
    // H4: `end` of the last message = the prompt end (the fake renders no generation opener).
    let mut snap = if o.hints && rec.snapshot_keep {
        Snap::Pos(plen as u32)
    } else {
        Snap::NotAsked
    };
    // `--drop-at` applies to the first chat request only.
    let global_cut = if rec.seq == 0 { o.drop_at } else { None };

    // F9: a trailing assistant message is history (a new turn is generated); only an explicit
    // in-place continuation (`add_generation_prompt: false` on a generating request) is a 400.
    let agp = serde_json::from_slice::<Value>(&req.body)
        .ok()
        .and_then(|v| v.get("add_generation_prompt").and_then(Value::as_bool));
    if crate::rejects_continuation(&rec.messages, rec.max_tokens, agp) {
        seen.push(rec);
        let body = json!({"error": {"code": 400, "type": "invalid_request_error", "message": crate::CONTINUATION_400}});
        return json_plan(400, &body);
    }
    // F13: the engine parses every re-sent tool call's arguments.
    if o.strict_tool_args {
        if let Some(msg) = bad_tool_args(&rec.messages) {
            seen.push(rec);
            let body = json!({"error": {"code": 400, "type": "invalid_request_error", "message": msg}});
            return json_plan(400, &body);
        }
    }
    // F6: like the engine, only a prompt that does not fit at all is refused; a reply that
    // would not fit is cut when the context is full (`finish_reason: "length"`).
    if plen as u64 >= n_ctx as u64 {
        seen.push(rec);
        return json_plan(400, &overflow_value(plen, n_ctx));
    }

    // H6: `max_tokens: 0` is prefill + checkpoint only. It consumes no turn. The engine's
    // stream: the role chunk, the finish chunk (`length`), the usage chunk with `timings`.
    if rec.max_tokens == 0 {
        let usage = Usage {
            prompt: plen as u32,
            completion: 0,
            cached: if o.omit_cached_tokens { 0 } else { cached as u32 },
            cached_missing: o.omit_cached_tokens,
        };
        rec.usage = Some(usage);
        core.cache.retain(|(s, _)| *s != rec.session);
        core.cache.push((rec.session.clone(), prompt.clone()));
        let mut body = format!(": prefill {plen}/{plen} 0s\n\n").into_bytes();
        sse_data(
            &mut body,
            &delta_value(json!({"role": "assistant", "content": null})),
        );
        sse_data(
            &mut body,
            &json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": rec.model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]}),
        );
        sse_data(
            &mut body,
            &json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": rec.model,
                    "choices": [], "usage": usage_value(usage, 0),
                    "timings": timings_value(plen, cached, 0, &snap)}),
        );
        let done_at = body.len();
        body.extend_from_slice(b"data: [DONE]\n\n");
        if global_cut.is_none() && o.hints && !rec.idem_key.is_empty() {
            core.idem.push(IdemEntry {
                key: rec.idem_key.clone(),
                body: req.body.clone(),
                response: body.clone(),
            });
        }
        let plan = StreamPlan {
            hold_ms: o.hold_headers_ms.max(o.hold_prefill_ms),
            comments_ms: o.comments_only_ms,
            cut: global_cut,
            done_gap_ms: o.done_gap_ms,
            ..StreamPlan::new(body, done_at)
        };
        seen.push(rec);
        return Plan::Stream(plan);
    }

    let turn = match core.turns.pop_front() {
        Some(t) => t,
        None => {
            seen.push(rec);
            return json_plan(
                500,
                &error_value(codes::INTERNAL, "script exhausted: unexpected extra request"),
            );
        }
    };
    let mut pre = Vec::new();
    match &turn.fault {
        Some(Fault::Overflow) => {
            seen.push(rec);
            return json_plan(400, &overflow_value(plen, n_ctx));
        }
        Some(Fault::Http { status, body }) => {
            seen.push(rec);
            return Plan::Json {
                status: *status,
                body: body.clone().into_bytes(),
                retry_after: None,
            };
        }
        Some(Fault::Refuse) => {
            seen.push(rec);
            return Plan::Refuse;
        }
        Some(Fault::Busy { retry_after }) => {
            seen.push(rec);
            let secs = retry_after.map_or(String::new(), |n| format!(" in {n} s"));
            let body = json!({"error": {"message": format!("Too many requests are waiting for the local model; retry{secs}."),
                                        "type": "local_backend_busy", "param": null, "code": "queue_full"}});
            return Plan::Json {
                status: 503,
                body: json_bytes(&body),
                retry_after: *retry_after,
            };
        }
        Some(Fault::StreamError {
            queued,
            status,
            error_type,
            event,
        }) => {
            seen.push(rec);
            let pre = (0..*queued)
                .map(|i| queued_comment(Some(queued - i), i))
                .collect();
            let mut body = Vec::new();
            if *event {
                body.extend_from_slice(b"event: error\n");
            }
            sse_data(&mut body, &stream_error_value(*status, error_type, plen, n_ctx));
            let len = body.len();
            return Plan::Stream(StreamPlan {
                pre,
                ..StreamPlan::new(body, len)
            });
        }
        Some(Fault::Queued { count, pos }) => {
            pre = (0..*count)
                .map(|i| queued_comment(pos.then_some(count - i), i))
                .collect();
        }
        Some(Fault::SnapNull { reason }) if !matches!(snap, Snap::NotAsked) => {
            snap = Snap::Null(reason.clone());
        }
        _ => {}
    }

    let hold_ms = match turn.fault {
        Some(Fault::HoldHeaders { ms }) => ms,
        _ => o.hold_headers_ms,
    };
    let comments_ms = match turn.fault {
        Some(Fault::CommentsOnly { ms }) => ms,
        _ => o.comments_only_ms,
    };
    let cut = match turn.fault {
        Some(Fault::DropAt { bytes }) => Some(bytes),
        _ => global_cut,
    };
    let early_close = matches!(turn.fault, Some(Fault::EarlyClose));
    let stall = match turn.fault {
        Some(Fault::Stall { bytes, ms }) => Some((bytes, ms)),
        _ => None,
    };

    // F6: the reply is bounded by `max_tokens` and by the room left in the context.
    let room = n_ctx - plen as u32;
    let (msg, finish, completion) = generate(&turn, rec.max_tokens.min(room));

    let mut body = Vec::new();
    if let Some(r) = &msg.reasoning {
        for c in split_chunks(r, turn.chunk) {
            sse_data(&mut body, &delta_value(json!({"reasoning_content": c})));
        }
    }
    if let Some(t) = &msg.content {
        for c in split_chunks(t, turn.chunk) {
            sse_data(&mut body, &delta_value(json!({"content": c})));
        }
    }
    for (index, call) in msg.tool_calls.iter().enumerate() {
        for (j, part) in split_chunks(&call.args_raw, turn.chunk).into_iter().enumerate() {
            let mut tc = json!({"index": index, "function": {"arguments": part}});
            if j == 0 {
                tc["id"] = json!(call.id);
                tc["type"] = json!("function");
                tc["function"]["name"] = json!(call.name);
            }
            sse_data(&mut body, &delta_value(json!({"tool_calls": [tc]})));
        }
    }

    let usage = Usage {
        prompt: plen as u32,
        completion,
        cached: if o.omit_cached_tokens { 0 } else { cached as u32 },
        cached_missing: o.omit_cached_tokens,
    };
    let fin = json!({
        "id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": rec.model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_value(finish)}],
        "usage": usage_value(usage, msg.reasoning.as_deref().map_or(0, str::len)),
        "timings": timings_value(plen, cached, completion, &snap),
    });
    sse_data(&mut body, &fin);
    let done_at = body.len();
    body.extend_from_slice(b"data: [DONE]\n\n");

    if cut.is_none() {
        let held = held_after(&prompt, &msg);
        rec.reply = Some(msg);
        rec.usage = Some(usage);
        core.cache.retain(|(s, _)| *s != rec.session);
        core.cache.push((rec.session.clone(), held));
    }
    let plan = StreamPlan {
        hold_ms,
        comments_ms,
        pre,
        body: body.clone(),
        done_at,
        done_gap_ms: o.done_gap_ms,
        cut,
        early_close,
        stall,
    };
    if cut.is_none() && o.hints && !rec.idem_key.is_empty() {
        core.idem.push(IdemEntry {
            key: rec.idem_key.clone(),
            body: req.body.clone(),
            response: body,
        });
    }
    seen.push(rec);
    Plan::Stream(plan)
}

fn json_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("json serialises")
}

fn stream_head(s: &mut TcpStream) -> io::Result<()> {
    s.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
          Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )
}

/// One HTTP chunk. Empty data is skipped: a zero-length chunk would end the body.
fn chunk(s: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    s.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
    s.write_all(data)?;
    s.write_all(b"\r\n")?;
    s.flush()
}

/// Write a planned stream; `Ok(true)` when the client closed the connection during a hold.
fn write_stream(s: &mut TcpStream, p: StreamPlan, sh: &Shared, id: u64) -> io::Result<bool> {
    if p.hold_ms > 0 && wait_watching(s, p.hold_ms, sh, id) {
        return Ok(true);
    }
    stream_head(s)?;
    let mut waited = 0u64;
    while waited < p.comments_ms {
        chunk(s, &queued_comment(None, (waited / 1000) as u32))?;
        let step = (p.comments_ms - waited).min(1000);
        std::thread::sleep(Duration::from_millis(step));
        waited += step;
    }
    for c in &p.pre {
        chunk(s, c)?;
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(n) = p.cut {
        // A failed reply: stop after `n` body bytes, no terminating chunk.
        let n = n.min(p.body.len());
        chunk(s, &p.body[..n])?;
        let _ = s.shutdown(Shutdown::Both);
        return Ok(false);
    }
    let mut from = 0;
    if let Some((n, ms)) = p.stall {
        // A silent stream: part of the reply, then nothing (no comment either) for `ms`.
        from = n.min(p.done_at);
        chunk(s, &p.body[..from])?;
        if wait_watching(s, ms, sh, id) {
            return Ok(true);
        }
    }
    chunk(s, &p.body[from..p.done_at])?;
    if p.done_gap_ms > 0 {
        std::thread::sleep(Duration::from_millis(p.done_gap_ms));
    }
    chunk(s, &p.body[p.done_at..])?;
    if !p.early_close {
        s.write_all(b"0\r\n\r\n")?;
    }
    s.flush().map(|_| false)
}

/// The `oxa-fake` binary: `oxa-fake --script <file> [--port N] [--record <file>]
/// [--done-gap-ms N] [--hold-headers MS] [--comments-only MS] [--drop-at BYTES] [--n-ctx N]
/// [--hold-prefill MS] [--rewrite-resume-block N] [--model-row ALIAS:CAP[:MAX_OUTPUT][:moe|flash|none]]... [--no-hints]`. Prints `listening <url>` on stdout, serves until SIGTERM / stdin close,
/// appends each request to the record file ([`crate::write_log`]). Returns the exit code.
pub fn main_from_args(args: &[String]) -> i32 {
    match run_main(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("oxa-fake: {e}");
            if e.code == codes::USAGE {
                2
            } else {
                1
            }
        }
    }
}

fn usage(msg: impl Into<String>) -> OxaError {
    OxaError::new(codes::USAGE, msg)
}

fn take<'a>(args: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str, OxaError> {
    *i += 1;
    args.get(*i)
        .map(String::as_str)
        .ok_or_else(|| usage(format!("{flag} needs a value")))
}

fn number<T: std::str::FromStr>(v: &str, flag: &str) -> Result<T, OxaError> {
    v.parse()
        .map_err(|_| usage(format!("{flag} needs a number, got `{v}`")))
}

/// `--model-row ALIAS:CAP[:MAX_OUTPUT][:moe|flash|none]` (repeatable): one `model_limits` row
/// (and `/v1/models` entry). `MAX_OUTPUT` may be empty or `-` (none); the capability block
/// defaults to `moe`.
fn model_row(v: &str) -> Result<FakeModel, OxaError> {
    let bad = || {
        usage(format!(
            "--model-row needs ALIAS:CAP[:MAX_OUTPUT][:moe|flash|none], got `{v}`"
        ))
    };
    let parts: Vec<&str> = v.split(':').collect();
    if parts.len() < 2 || parts.len() > 4 || parts[0].is_empty() {
        return Err(bad());
    }
    let cap: u32 = parts[1].parse().map_err(|_| bad())?;
    let max_output = match parts.get(2).copied() {
        None | Some("") | Some("-") => None,
        Some(n) => Some(n.parse().map_err(|_| bad())?),
    };
    let caps = match parts.get(3).copied() {
        None | Some("moe") => FakeCaps::rust_moe(),
        Some("flash") => FakeCaps::rust_flash(),
        Some("none") => FakeCaps::none(),
        Some(_) => return Err(bad()),
    };
    Ok(FakeModel {
        alias: parts[0].to_string(),
        cap,
        max_output,
        caps: Some(caps),
    })
}

fn run_main(args: &[String]) -> Result<(), OxaError> {
    let mut opts = FakeOpts::default();
    let mut script_path: Option<String> = None;
    let mut record: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--script" => script_path = Some(take(args, &mut i, flag)?.to_string()),
            "--record" => record = Some(take(args, &mut i, flag)?.to_string()),
            "--port" => opts.port = number(take(args, &mut i, flag)?, flag)?,
            "--n-ctx" => opts.n_ctx = number(take(args, &mut i, flag)?, flag)?,
            "--model" => opts.model = take(args, &mut i, flag)?.to_string(),
            "--done-gap-ms" => opts.done_gap_ms = number(take(args, &mut i, flag)?, flag)?,
            "--hold-headers" => opts.hold_headers_ms = number(take(args, &mut i, flag)?, flag)?,
            "--comments-only" => opts.comments_only_ms = number(take(args, &mut i, flag)?, flag)?,
            "--drop-at" => opts.drop_at = Some(number(take(args, &mut i, flag)?, flag)?),
            "--hold-prefill" => opts.hold_prefill_ms = number(take(args, &mut i, flag)?, flag)?,
            "--model-row" => opts.models.push(model_row(take(args, &mut i, flag)?)?),
            "--rewrite-resume-block" => {
                opts.rewrite_resume_block = Some(number(take(args, &mut i, flag)?, flag)?)
            }
            "--no-hints" => opts.hints = false,
            other => return Err(usage(format!("unknown argument `{other}`"))),
        }
        i += 1;
    }
    let script_path = script_path.ok_or_else(|| usage("--script <file> is required"))?;
    let text = std::fs::read_to_string(&script_path).map_err(|e| OxaError::io("read script", &e))?;
    let turns = crate::parse_script(&text)?;

    let mut server = FakeServer::start(turns, opts)?;
    {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        writeln!(out, "listening {}", server.url()).map_err(|e| OxaError::io("stdout", &e))?;
        out.flush().map_err(|e| OxaError::io("stdout", &e))?;
    }

    // Appends each new request to the record file while serving.
    let done = Arc::new(AtomicBool::new(false));
    let writer = record.map(|path| {
        let state = server.state.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let mut written = 0usize;
            loop {
                let finished = done.load(Ordering::SeqCst);
                let pending: Vec<Recorded> = {
                    let all = lock(&state);
                    all[written.min(all.len())..].to_vec()
                };
                if !pending.is_empty() {
                    let _ = write_log(std::path::Path::new(&path), &pending);
                    written += pending.len();
                }
                if finished {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    });

    // Serve until stdin closes.
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    done.store(true, Ordering::SeqCst);
    if let Some(w) = writer {
        let _ = w.join();
    }
    server.stop();
    Ok(())
}
