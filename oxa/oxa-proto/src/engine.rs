use crate::{
    AssistantTurn, CancelToken, FrozenPrefix, OxaError, StreamEvent, ThinkingLevel, TurnKind, WireMsg,
};

/// What the engine reports about itself (`/v1/models` + `/_local/status`, M3, plus the
/// capability block `oxe_hints` of `/props`). See `oxa_engine::parse_info` for the mapping of
/// the controller's status fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    pub base_url: String,
    /// All models the endpoint can serve.
    pub models: Vec<String>,
    /// The loaded model (the selected one when nothing is loaded).
    pub model: String,
    /// X3: the model that `n_ctx`, `max_output` and `compact_at` describe. The requested target
    /// of [`Engine::info`] when the engine publishes limits for it (the controller's per-model
    /// `model_limits`), even when that model is not loaded; else `model`.
    pub limits_for: String,
    pub n_ctx: u32,
    pub max_output: u32,
    /// Requests waiting; 0 when the engine does not report it.
    pub queue: u32,
    /// The controller's recommended compaction point (prompt tokens) for the selected context
    /// mode (`context.modes[mode].compact_at`); `None` when it publishes none.
    pub compact_at: Option<u32>,
    /// H6: the engine advertises zero-token prefill (`/props` `oxe_hints.prefill_only`). Idle
    /// prefill requests are sent only when `true`.
    pub prefill_only: bool,
    /// The other capabilities of `limits_for` (F4 / EG2).
    pub caps: EngineCaps,
    /// Whether the model thinks (reasoning). `None` = the controller published no thinking field
    /// (a plain OpenAI server), so a client must not assume it does.
    pub thinking: Option<bool>,
    /// Whether the model accepts images as input. `None` = no input-modality field was published.
    pub images: Option<bool>,
}

/// What the engine lets a client use with a model, known before the model loads: the
/// controller's `/_local/status` `model_limits[alias].capabilities` (EG2), else the loaded
/// engine's `/props` `oxe_hints`. `Default` = nothing advertised (a plain OpenAI server).
/// `EngineInfo::prefill_only` (H6) is the fourth capability; it predates this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EngineCaps {
    /// H4: checkpoints kept per session for `X-Oxe-Snapshot: keep`; 0 = the hint is not sent.
    pub snapshot_keep: u32,
    /// EG5: the hint-scoped clear (`{"scope": "hint"}`) exists; [`Engine::clear_hint`] is only
    /// called when `true`.
    pub hint_clear: bool,
    /// F8: whether the model keeps reasoning in the replayed history. `Some(false)` (Flash):
    /// the request must not carry `preserve_thinking` at all; `None` = not published, the W6
    /// default (`true`) is sent.
    pub preserve_thinking: Option<bool>,
    /// Local request-level reasoning-token cap is supported; unknown/remote defaults false.
    pub thinking_budget: bool,
    /// `park == "disk"`: a parked session of this model is restored from disk only (Flash).
    pub park_disk_only: bool,
    /// Wave D D4: this model runs at a cloud provider through the controller. Local-only
    /// features (prefix hints, cache warming, engine-side state and the X-Oxe-* headers) are
    /// not sent to it.
    pub remote: bool,
    /// Cloud endpoint explicitly advertises JSON Schema response formatting.
    pub response_format: bool,
    /// The provider name of a remote model (`openrouter`, `anthropic`); `None` when local.
    pub remote_provider: Option<&'static str>,
    /// The engine echoes a previous reply (CACHE-PERFECT ECHO); local only.
    pub echo: bool,
    /// Prefix/cache hints are useful (snapshot, hint clear, cache warming); local only.
    pub cache_hints: bool,
    /// The client may cancel a running request (the controller/engine closes the socket).
    pub cancel: bool,
    /// Zero-token prefill (`max_tokens: 0`) is available; local Rust engines only.
    pub prefill: bool,
    /// The model's context window when the engine publishes it as a capability.
    pub context_window: Option<u32>,
}

/// The capability string an engine puts in the `oxe_hints.hints` array of its `/props` when it
/// accepts a deliberate-cancel notice. The notice is a PROPOSAL recorded in the shared
/// `cancel-contract.md` section 7 ("Proposal: explicit cancel notice, capability-gated"). The
/// client sends it only when the server advertises this capability; until a server advertises
/// it nothing is sent, so the code is inert by design. The socket close stays the primary cancel
/// signal. Rename together with [`CANCEL_PATH`] if the proposal changes.
pub const CANCEL_HINT: &str = "cancel";

/// PROVISIONAL, capability-gated proposal (cancel-contract.md section 7): `POST` path of the
/// deliberate-cancel notice, below the base path of the engine URL. The notice is a PROPOSAL
/// recorded in the shared `cancel-contract.md` section 7 ("Proposal:
/// explicit cancel notice, capability-gated"). Body `{"idempotency_key": <key of the cancelled
/// request>, "session": <session id>}`; sent at most once per cancelled request,
/// fire-and-forget, only when [`CANCEL_HINT`] is advertised. Until a server advertises
/// [`CANCEL_HINT`] nothing is sent, so the code is inert by design; the socket close stays the
/// primary cancel signal.
pub const CANCEL_PATH: &str = "/_local/cancel";

/// The optional, client-neutral hints of plan sec. 3. A hint never changes generated bytes and
/// is ignored by a server that lacks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hints<'a> {
    /// H1 `X-Oxe-Session` (also sent as `X-Session-Id`).
    pub session: &'a str,
    /// H2 `Idempotency-Key`; omitted when empty. A retry reuses the same key.
    pub idem_key: &'a str,
    /// H5 `X-Oxe-Turn-Kind`.
    pub kind: TurnKind,
    /// H3 `X-Oxe-Priority: background`.
    pub background: bool,
    /// H4 `X-Oxe-Snapshot: keep`.
    pub snapshot_keep: bool,
}

/// One chat request. The body is a pure function of this value
/// (`oxa_engine::build_body`); W1-W3 and W6 are tested on those bytes.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub prefix: &'a FrozenPrefix,
    pub messages: &'a [WireMsg],
    /// JSON schema, only populated for a cloud model advertising response_format.
    pub output_schema: Option<&'a crate::Value>,
    pub model: &'a str,
    /// `0` = prefill + checkpoint only (H6): usage and timings, no decode.
    pub max_tokens: u32,
    /// W6 `enable_thinking`.
    pub thinking: bool,
    /// The level behind `thinking` (ignored while `thinking` is false). A cloud model gets it as
    /// `reasoning_effort`; `None` is `Medium`. Local engines only see the boolean.
    pub thinking_level: Option<ThinkingLevel>,
    /// Optional local reasoning-token cap; omitted unless thinking is on.
    pub thinking_budget: Option<u64>,
    /// W6 / F8: send `chat_template_kwargs.preserve_thinking: true`. `false` omits the key (the
    /// server's own default applies; never forced on a model without the capability).
    pub preserve_thinking: bool,
    /// Stochastic sampling for this request. `None` = W6 greedy: `"temperature":0` and no other
    /// sampling key (the engine's MTP speculation runs on greedy requests only).
    pub sampling: Option<Sampling>,
    pub hints: Hints<'a>,
}

impl Default for Hints<'_> {
    /// No session, no idempotency key, an ordinary foreground chat turn.
    fn default() -> Self {
        Hints {
            session: "",
            idem_key: "",
            kind: TurnKind::Chat,
            background: false,
            snapshot_keep: false,
        }
    }
}

impl ChatRequest<'_> {
    /// The level this request thinks at: `Off` unless `thinking`, else the requested level.
    pub fn effective_level(&self) -> ThinkingLevel {
        match (self.thinking, self.thinking_level) {
            (false, _) => ThinkingLevel::Off,
            // Thinking forced on while the session level is off: the default level.
            (true, Some(ThinkingLevel::Off) | None) => ThinkingLevel::Medium,
            (true, Some(level)) => level,
        }
    }
}

impl<'a> ChatRequest<'a> {
    /// A request with the required parts and defaults for everything else (1024 output tokens,
    /// thinking off, greedy, default hints). Build with `ChatRequest { max_tokens: 64,
    /// ..ChatRequest::new(..) }` so a field added later does not break the caller.
    pub fn new(prefix: &'a FrozenPrefix, messages: &'a [WireMsg], model: &'a str) -> Self {
        ChatRequest {
            prefix,
            messages,
            model,
            output_schema: None,
            max_tokens: 1024,
            thinking: false,
            thinking_level: None,
            thinking_budget: None,
            preserve_thinking: false,
            sampling: None,
            hints: Hints::default(),
        }
    }
}

/// Sampling parameters sent with a request (`oxa.sampling.mode = model`): the model card's
/// recommended values. The engine reads `presence_penalty` per reply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub min_p: f32,
    pub presence_penalty: f32,
    /// A fixed seed (tests, reproducible runs); `None` = the engine's own.
    pub seed: Option<u64>,
}

/// The engine as the core sees it. `oxa-engine` implements it over HTTP; tests script it.
pub trait Engine: Send + Sync {
    /// The engine's models and the limits of `target` (X3). `target = None` (or a target the
    /// engine publishes nothing for) describes the loaded model; otherwise the limits are the
    /// target's, read before its first request even when it is not loaded
    /// (`EngineInfo::limits_for` says which). Never loads a model.
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError>;
    /// One warning line when the server advertises a newer protocol than this oxa understands,
    /// or lacks a capability oxa expects, as read by the last [`Engine::info`] (no request of
    /// its own). `None` = nothing to say, or an engine that does not check.
    fn handshake_warning(&self) -> Option<String> {
        None
    }
    /// Exact token count of `text` (`POST /tokenize`).
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError>;
    /// Run one request to completion, calling `on` for each stream increment in order.
    ///
    /// * `Ok` only for a complete reply (a `finish_reason` and `[DONE]` were received); `msg`
    ///   holds exactly the streamed bytes (W2).
    /// * Cancel -> `Err(aborted)`. A stream that ends or stalls early -> `Err(stream_broken)`.
    ///   400 `exceed_context_size_error` -> `Err(context_overflow)`. Refused connection ->
    ///   `Err(engine_down)` with a hint.
    /// * Retries happen only before the first data byte and reuse the idempotency key.
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError>;
    /// EG5: drop everything the engine holds for the session hint `hint` alone (a finished
    /// child session `parent.sN`): `POST /_local/cache/clear {"scope": "hint", "hint": ..}`.
    /// State another hint also owns (the parent's prefix, shared anchors) stays. Idempotent.
    /// The engine refuses while a request runs (409 -> `busy`); the caller may retry. The
    /// default does nothing, for engines without the clear.
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        let _ = hint;
        Ok(())
    }
    /// OXA-EXACT-COUNTS: the exact prompt tokens of `req` as the engine renders it (template
    /// framing and generation opener included): what the reply's `usage.prompt_tokens` would
    /// be. `Ok(None)` = the engine cannot count exactly (no `/apply-template` with
    /// `return_tokens`); callers then fall back to an estimate. Never generates, never touches
    /// the session's state; one extra request, so never on the hot path of a normal turn.
    fn count_prompt(&self, req: &ChatRequest) -> Result<Option<u64>, OxaError> {
        let _ = req;
        Ok(None)
    }
}
