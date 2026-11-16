//! B3: one request and one tool batch.
// ported from pi packages/agent/src/agent-loop.ts (executeToolCalls) and stream-fn.ts
use crate::agent_loop::persist;
use crate::host::{fire_hook, pumped, serve_level, Serve};
use crate::SessionState;
use oxa_proto::{
    codes, rebase_why, tool_errors, wire_is_prefix, Access, AgentEvent, Answer, AssistantTurn, ChatRequest,
    Decision, EntryBody, EventMessage, EventSink, Hints, HookPoint, HookReply, Level, MessageDelta, OxaError,
    StopReason, StreamEvent, Tool, ToolCall, ToolClass, ToolCtx, ToolOutput, TurnKind, Ui, Usage, Value,
    WireMsg,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Options of one engine request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOpts {
    pub kind: TurnKind,
    /// `None` = the model's max output; `Some(0)` = prefill only (H6).
    pub max_tokens: Option<u32>,
    /// Override of the session thinking level (`Some(false)` for summaries). `None` = the
    /// T5 policy (`think.rs`): the level under `fixed`, by turn kind under `auto`.
    pub thinking: Option<bool>,
    pub background: bool,
    pub snapshot_keep: bool,
    /// Messages sent after the wire for this request only (the summary instruction). They are
    /// not pushed to the transcript.
    pub extra: Vec<WireMsg>,
    /// Emit `Message*` events for this request (false for summaries and prefill).
    pub events: bool,
    /// Reuse this idempotency key (a retry); empty = make a new one.
    pub idem_key: String,
}

/// Build the `ChatRequest` from the state (`prefix`, `transcript.wire()` + `extra`, model,
/// thinking, hints with the session id) and call `Engine::chat` once, translating stream
/// events into `MessageUpdate` / `Prefill` events. Fires `Context` / `BeforeProviderRequest`
/// through the W-guard first: a `Rebase` reply that has the wire as a prefix is treated as an
/// append; any other is refused (`Notice` + `guard_refused` logged) unless the hook set
/// declared `rebase: true` (then `Transcript::rebase(.., extension)`). Does not push the reply.
///
/// S1 (ENG-001): the request gate of the instance (`Shared::gate`) is held from the hooks to
/// the end of the reply; a second request meanwhile (re-entrancy through a host request served
/// during the hooks) is refused with `busy` and nothing is sent. ENG-002: a `background`
/// request is cancelled by `Gate::bg_cancel` (user input, abort), every other one by
/// `Shared::cancel` (abort) only. F11: after a request that carried H4, `timings.snap_pos` is
/// recorded in `Stats::last_snap_pos`; a reply without a checkpoint gives one `Info` notice.
pub fn request(
    st: &mut SessionState,
    opts: &RequestOpts,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<AssistantTurn, OxaError> {
    // OXA-LOCAL-FIRST F1: a tool-result prefill still running (a host request served while a
    // tool runs, e.g. the park before a subagent) ends first: one request in flight at a time.
    // Nothing is sent while it is still registered (deferred, then `engine_busy`).
    await_tool_prefill(st, opts.background, sink)?;
    if st.info.caps.remote && oxa_proto::offline() {
        return Err(oxa_proto::offline_error(&format!("the cloud model {}", st.model)));
    }
    let _gate = st.shared.enter_request()?;
    guard_hooks(st, HookPoint::Context, sink, ui)?;
    guard_hooks(st, HookPoint::ContextWithSystem, sink, ui)?;
    guard_hooks(st, HookPoint::BeforeProviderRequest, sink, ui)?;

    let mut msgs: Vec<WireMsg> = st.transcript.wire().to_vec();
    msgs.extend(opts.extra.iter().cloned());
    let idem = if opts.idem_key.is_empty() {
        new_idem_key(st)
    } else {
        opts.idem_key.clone()
    };
    let max_tokens = opts.max_tokens.unwrap_or(st.info.max_output);
    let thinking = opts
        .thinking
        .unwrap_or_else(|| crate::think::request_thinking(st, opts.kind));
    let session = st.sref.id.clone();
    let engine = st.engine.clone();
    let cancel = if opts.background {
        st.shared.gate.bg_cancel.clone()
    } else {
        st.shared.cancel.clone()
    };
    let hints = Hints {
        session: &session,
        idem_key: &idem,
        kind: opts.kind,
        background: opts.background,
        snapshot_keep: opts.snapshot_keep,
    };
    // Owned, so that the provider hooks can fire (with the state) while the reply streams.
    let (prefix, model) = (st.prefix.clone(), st.model.clone());
    let req = ChatRequest {
        prefix: &prefix,
        messages: &msgs,
        model: &model,
        max_tokens,
        thinking,
        thinking_budget: crate::think::request_budget(st, thinking),
        // F8: never forced on a model that does not keep reasoning (Flash).
        preserve_thinking: st.info.caps.preserve_thinking != Some(false),
        sampling: match opts.kind {
            TurnKind::Chat | TurnKind::Subagent => crate::sampling::for_request(st, thinking),
            _ => None,
        },
        hints,
    };
    let mut stream = StreamState::default();
    let mut hooks = crate::fire::StreamHooks::new(st, opts.events);
    let mut on = |ev: StreamEvent| {
        hooks.raw(st, &ev, sink, ui);
        stream.handle(ev, &mut hooks.sink(st, sink, ui), opts.events)
    };
    let turn = engine.chat(&req, &cancel, &mut on)?;
    if opts.events {
        // The closing `*_end` records go through the same sink, so `message_update` sees them.
        stream.finish(&turn, &mut hooks.sink(st, sink, ui));
    }
    hooks.done(st, sink, ui);
    if opts.snapshot_keep {
        note_snapshot(st, &turn, opts.kind, sink);
    }
    Ok(turn)
}

/// F11: what the engine did with an H4 hint. A kept checkpoint is recorded
/// (`Stats::last_snap_pos`); `snap_pos: null` (the engine knows H4 but kept none) is one
/// `Info` notice, never an error: the next request after a park or a leaf move may only
/// prefill more.
fn note_snapshot(
    st: &mut SessionState,
    turn: &AssistantTurn,
    kind: oxa_proto::TurnKind,
    sink: &mut dyn EventSink,
) {
    let Some(t) = turn.timings else {
        return;
    };
    match t.snap_pos {
        Some(pos) => st.stats.last_snap_pos = Some(pos),
        None if t.snap_asked => sink.emit(&AgentEvent::Notice {
            level: Level::Info,
            text: format!(
                "the engine kept no checkpoint for this {} request; a later restore may prefill more",
                kind.as_str()
            ),
        }),
        None => {}
    }
}

/// Execute the tool calls of one assistant message and return one `tool` message per call, in
/// call order. Per call: find the tool in `st.active` (unknown = `tool_errors::UNKNOWN_TOOL`);
/// parse `args_raw` as a JSON object (else `BAD_JSON`); hook `ToolCall` (block / replace
/// args); `Tool::access`; `Policy::decide` per access (`Ask` -> `Ui::ask_access`; `Session` ->
/// `Policy::remember`; denial = fixed text); `Tool::run` (or `run_with_ui`, serially, when
/// `Tool::needs_ui`); hook `ToolResult` (once); `shape`
/// with `settings.tool_output_cap()` (`keep_tail` for `ToolClass::Exec`). Consecutive
/// `ToolClass::Read` calls may run in parallel threads; writes to the same path are
/// serialised by `MutationQueue`. Emits `ToolExecutionStart/Update/End`. A cancel yields
/// `tool_errors::ABORTED` results for the calls not finished. The caller appends the messages.
pub fn execute_calls(
    st: &mut SessionState,
    calls: &[ToolCall],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Vec<WireMsg> {
    run_calls(st, calls, sink, ui)
        .into_iter()
        .map(|r| r.msg)
        .collect()
}

/// One finished tool call with what the loop needs for its events.
pub(crate) struct CallResult {
    pub msg: WireMsg,
    pub tool_name: String,
    pub is_error: bool,
    /// `ToolOutput::ui`, stored for display on resume.
    pub ui: Value,
    /// The repeat guard asks to end the run after this batch (`repeat.rs`).
    pub stop: bool,
}

/// What became of a call before anything ran.
enum Plan {
    /// Finished without running: unknown tool, bad arguments, a hook block, a denial.
    Done(ToolOutput),
    /// Allowed; `run` decides the rest.
    Run { tool: Arc<dyn Tool> },
}

struct Prepared {
    id: String,
    name: String,
    args: Value,
    exec: bool,
    plan: Plan,
    /// The lock keys of the call's `Access::Write` paths (`MutationQueue::keys`: canonical,
    /// sorted, unique): held in [`mutations`] while it runs.
    writes: Vec<std::path::PathBuf>,
    /// #518: the real locations the permission check judged (`ToolCtx::checked`).
    checked: Vec<(std::path::PathBuf, std::path::PathBuf)>,
}

/// The process-wide mutation queue: write-class calls on the same path never run at once, also
/// across sessions in one process (C7). Calls on different paths do not wait for each other.
fn mutations() -> &'static crate::MutationQueue {
    static QUEUE: std::sync::OnceLock<crate::MutationQueue> = std::sync::OnceLock::new();
    QUEUE.get_or_init(crate::MutationQueue::new)
}

/// Run `f` holding every lock key of `keys` in [`mutations`]. The keys are canonical (aliases of
/// one file are one key, X4) and taken in sorted order, so two calls with overlapping path sets
/// cannot deadlock and one call never waits for itself.
fn with_writes<R>(keys: &[std::path::PathBuf], f: impl FnOnce() -> R) -> R {
    mutations().with_keys(keys, f)
}

/// Fail every call of a reply cut by the output limit without running it (pi's
/// `failToolCallsFromTruncatedMessage`): a start and an end event per call and one error
/// result `tool_errors::TRUNCATED`, in call order. No hook, policy or tool is consulted.
pub(crate) fn fail_truncated_calls(
    st: &mut SessionState,
    calls: &[ToolCall],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Vec<CallResult> {
    calls
        .iter()
        .map(|call| {
            let args = parse_args(&call.args_raw).unwrap_or(Value::Null);
            crate::fire::tool_start(st, &call.id, &call.name, &args, sink, ui);
            let output = ToolOutput::error(tool_errors::TRUNCATED.replace("{name}", &call.name));
            crate::fire::tool_end(st, &call.id, &call.name, &output, Some(0), sink, ui);
            CallResult {
                msg: WireMsg::tool_result(call.id.clone(), output.text),
                tool_name: call.name.clone(),
                is_error: true,
                ui: Value::Null,
                stop: false,
            }
        })
        .collect()
}

impl Prepared {
    fn parallel_read(&self) -> bool {
        match &self.plan {
            Plan::Run { tool } => tool.spec().class == ToolClass::Read && !tool.needs_ui(),
            Plan::Done(_) => false,
        }
    }
}

/// The batch engine behind [`execute_calls`]; keeps the tool names and error flags.
pub(crate) fn run_calls(
    st: &mut SessionState,
    calls: &[ToolCall],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Vec<CallResult> {
    // pi: the session variables are resolved when each command starts (ENV-003).
    crate::session::refresh_tool_env(st);
    // Phase 1, in call order: lookup, arguments, hooks, access and policy.
    let mut prepared = Vec::with_capacity(calls.len());
    for call in calls {
        let p = prepare(st, call, sink, ui);
        if let Plan::Done(_) = &p.plan {
            // Finished before running: its start is emitted now, in call order.
            crate::fire::tool_start(st, &p.id, &p.name, &p.args, sink, ui);
        }
        prepared.push(p);
    }

    // Phase 2: run. Consecutive parallel-safe reads share a thread scope; everything else runs
    // alone, so writes and exec calls are serial.
    let n = prepared.len();
    let mut outputs: Vec<Option<(ToolOutput, Duration)>> = (0..n).map(|_| None).collect();
    // F1: results shaped ahead for the tool-result prefill (reused in phase 3 when unchanged).
    let mut early: Vec<Option<Early>> = (0..n).map(|_| None).collect();
    let mut prefilled = false;
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j < n && prepared[j].parallel_read() {
            j += 1;
        }
        if j - i > 1 {
            let batch = run_parallel(st, &prepared[i..j], sink, ui);
            for (k, res) in batch.into_iter().enumerate() {
                outputs[i + k] = Some(res);
            }
            i = j;
        } else {
            let warming = !prefilled && start_tool_prefill(st, &prepared, &outputs, i, &mut early);
            // Held while the call runs: an unwind ends the prefill too.
            let guard = warming.then(|| PrefillGuard(st.shared.clone()));
            outputs[i] = Some(run_one(st, &prepared[i], sink, ui));
            if let Some(guard) = guard {
                prefilled = finish_tool_prefill(guard, sink);
            }
            i += 1;
        }
    }

    // Phase 3, in call order: the ToolResult hook, shape, end events.
    let mut out = Vec::with_capacity(n);
    // The repeat guard (`repeat.rs`): this batch's earlier calls and final result texts, in order.
    let mut batch: Vec<(String, String, String)> = Vec::with_capacity(n);
    for (((p, res), ahead), call) in prepared.iter().zip(outputs).zip(early).zip(calls) {
        let (mut output, dur) = res.unwrap_or((ToolOutput::error(tool_errors::ABORTED), Duration::ZERO));
        output = apply_result_hook(st, p, output, sink, ui);
        let cap = crate::tune::output_cap(st);
        output.text = match ahead {
            // F1: the same pure function of the same inputs, computed before (byte-identical).
            Some(e) if e.raw == output.text && e.cap == cap => e.shaped,
            _ => oxa_tools::shape(output.text.as_bytes(), cap, p.exec, &st.spill_dir),
        };
        // The repeat guard: a call that ran and returned the same result LIMIT times in a row is
        // answered with a notice (the call ran; only its result text changes).
        let mut stop = false;
        let ran = matches!(p.plan, Plan::Run { .. });
        if ran {
            let earlier: Vec<(&str, &str, &str)> = batch
                .iter()
                .map(|(n, a, t)| (n.as_str(), a.as_str(), t.as_str()))
                .collect();
            match crate::repeat::judge(st, &p.name, &call.args_raw, &output.text, &earlier) {
                crate::repeat::Verdict::Pass => {}
                crate::repeat::Verdict::Notice(text) => {
                    sink.emit(&AgentEvent::Notice {
                        level: Level::Warning,
                        text: format!(
                            "{} returned the same result again; the model was told to change approach",
                            p.name
                        ),
                    });
                    output = ToolOutput::error(text);
                }
                crate::repeat::Verdict::Stop(text) => {
                    output = ToolOutput::error(text);
                    stop = true;
                }
            }
        }
        batch.push((p.name.clone(), call.args_raw.clone(), output.text.clone()));
        let ms = Some(dur.as_millis() as u64);
        crate::fire::tool_end(st, &p.id, &p.name, &output, ms, sink, ui);
        out.push(CallResult {
            msg: WireMsg::tool_result(p.id.clone(), output.text.clone()),
            tool_name: p.name.clone(),
            is_error: output.is_error,
            ui: output.ui.clone(),
            stop,
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// OXA-LOCAL-FIRST F1: prefill while tools run (`oxa.prefill.tools`)
//
// Results are pushed in call order, so while a slow call runs, the results of the calls before it
// are final. One background zero-token request (H6, H3, H4 as the idle prefill sends them,
// `TurnKind::Prefill`) over `wire + those results` puts them in the engine's cache, so the
// follow-up request finds most of its prompt cached. Timing only: nothing is pushed, nothing is
// recorded, the cache expectation is unchanged (the follow-up reports more `cached` than
// expected, which is silent).
//
// Design. The request runs on a helper thread that owns everything it sends (a clone of the
// engine handle, the prefix, the messages, its own `CancelToken`) and calls `Engine::chat`
// directly: it bypasses `request` on purpose (no hooks, no events, no gate), because the
// session thread is busy with the tool. It is started only for a call that runs alone (the
// serial path of `run_calls`, with or without a host) and waits [`tool_prefill_after`] before it
// sends, so a call that finishes sooner costs nothing. The session thread cancels and joins it
// (bounded, [`TOOL_PREFILL_JOIN`]) as soon as the call returns, and every `request` of the
// session does the same first (a park served while a subagent tool runs) and sends nothing
// while it is still registered ([`await_tool_prefill`]), so two requests of one instance are
// never in flight. A panic or an early return drops [`PrefillGuard`], which
// does the same. Not covered: a slow read inside a parallel batch (the batch has no point at
// which earlier results are final while a later one runs).

/// F1: how long the running call must have run before the prefill is sent.
pub const TOOL_PREFILL_AFTER: Duration = Duration::from_secs(2);
/// F1: the least total bytes of the finished leading results (as they will be pushed).
pub const TOOL_PREFILL_MIN_BYTES: usize = 2048;
/// F1: how long the session thread waits for a cancelled prefill to end before it goes on
/// without it (the engine client polls its cancel token every 100 ms, also while writing).
const TOOL_PREFILL_JOIN: Duration = Duration::from_secs(3);
/// F1: how much longer a `request` waits (abortable) for a cancelled prefill that outlived
/// [`TOOL_PREFILL_JOIN`] before it refuses to send (`engine_busy`, retryable).
const TOOL_PREFILL_DEFER: Duration = Duration::from_secs(30);

static TOOL_PREFILL_AFTER_MS: AtomicU64 = AtomicU64::new(TOOL_PREFILL_AFTER.as_millis() as u64);

/// F1: the delay in force (the const, unless a test changed it).
fn tool_prefill_after() -> Duration {
    Duration::from_millis(TOOL_PREFILL_AFTER_MS.load(Ordering::Relaxed))
}

/// Tests only: replace [`TOOL_PREFILL_AFTER`] in this process.
#[doc(hidden)]
pub fn set_tool_prefill_after_for_tests(after: Duration) {
    TOOL_PREFILL_AFTER_MS.store(after.as_millis() as u64, Ordering::Relaxed);
}

/// A result shaped ahead: reused in phase 3 only for the same raw text and cap.
struct Early {
    raw: String,
    cap: usize,
    shaped: String,
}

/// What the helper reports: `Ok(true)` = the prefill completed, `Ok(false)` = cancelled before
/// anything was sent, `Err` = the request failed or was cancelled while in flight.
type PrefillOutcome = Result<bool, OxaError>;

struct PrefillCell {
    cancel: oxa_proto::CancelToken,
    handle: std::sync::Mutex<Option<std::thread::JoinHandle<PrefillOutcome>>>,
}

/// The running tool-result prefill of each session (keyed by its `Shared`), so that any
/// request of that session ends it first. At most one per session.
type Active = std::sync::Mutex<Vec<(usize, Arc<PrefillCell>)>>;

fn active() -> &'static Active {
    static ACTIVE: std::sync::OnceLock<Active> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

fn shared_key(shared: &Arc<crate::Shared>) -> usize {
    Arc::as_ptr(shared) as usize
}

/// Cancel and join the session's tool-result prefill, if one runs. `None` when there was none
/// or it did not end within [`TOOL_PREFILL_JOIN`]: it then STAYS registered, so the next
/// `request` of the session (and the next batch) settles it again and no new helper starts
/// meanwhile; its request is cancelled and sends nothing more. The engine client sees the
/// cancel within one poll (100 ms) while connecting, writing and reading; the bound only
/// covers an `Engine` that ignores the token, so the UI never hangs on it.
fn settle_tool_prefill(shared: &Arc<crate::Shared>) -> Option<PrefillOutcome> {
    settle_tool_prefill_within(shared, TOOL_PREFILL_JOIN, &|| false)
}

/// [`settle_tool_prefill`] with its own bound; `stop` ends the wait early (`None`).
fn settle_tool_prefill_within(
    shared: &Arc<crate::Shared>,
    bound: Duration,
    stop: &dyn Fn() -> bool,
) -> Option<PrefillOutcome> {
    let key = shared_key(shared);
    let cell = {
        let list = active().lock().unwrap_or_else(|p| p.into_inner());
        list.iter().find(|(k, _)| *k == key)?.1.clone()
    };
    cell.cancel.cancel();
    let deadline = Instant::now() + bound;
    loop {
        let finished = cell
            .handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_none_or(|h| h.is_finished());
        if finished {
            break;
        }
        if Instant::now() >= deadline || stop() {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    active()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|(_, c)| !Arc::ptr_eq(c, &cell));
    let handle = cell.handle.lock().unwrap_or_else(|p| p.into_inner()).take()?;
    handle.join().ok()
}

/// Before a `request` sends: settle the session's tool-result prefill and make sure it is no
/// longer registered, so the two are never in flight together. One that outlived
/// [`TOOL_PREFILL_JOIN`] (an `Engine` ignoring its cancel) is waited for up to
/// [`TOOL_PREFILL_DEFER`] more, ended early by the request's own cancel (`aborted`); then the
/// request is refused with the retryable `engine_busy` and nothing is sent (the transcript is
/// unchanged, so the retry or the next prompt sends the same request).
fn await_tool_prefill(st: &SessionState, background: bool, sink: &mut dyn EventSink) -> Result<(), OxaError> {
    settle_tool_prefill(&st.shared);
    if !tool_prefill_registered(&st.shared) {
        return Ok(());
    }
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text: "waiting for the cancelled tool-result prefill to end before sending".to_string(),
    });
    let user = st.shared.cancel.clone();
    let bg = st.shared.gate.bg_cancel.clone();
    let stop = || user.is_cancelled() || (background && bg.is_cancelled());
    settle_tool_prefill_within(&st.shared, TOOL_PREFILL_DEFER, &stop);
    if !tool_prefill_registered(&st.shared) {
        return Ok(());
    }
    if stop() {
        return Err(OxaError::new(codes::ABORTED, "aborted"));
    }
    Err(OxaError::new(
        codes::ENGINE_BUSY,
        format!(
            "the cancelled tool-result prefill has not ended after {} s; nothing was sent (retry)",
            (TOOL_PREFILL_JOIN + TOOL_PREFILL_DEFER).as_secs()
        ),
    ))
}

/// A tool-result prefill of this session is still registered (not joined yet).
fn tool_prefill_registered(shared: &Arc<crate::Shared>) -> bool {
    let key = shared_key(shared);
    active()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|(k, _)| *k == key)
}

/// Ends the session's tool-result prefill when dropped (a panic or an early return).
struct PrefillGuard(Arc<crate::Shared>);

impl Drop for PrefillGuard {
    fn drop(&mut self) {
        settle_tool_prefill(&self.0);
    }
}

/// F1: start the prefill for call `i`, which is about to run alone, when every rule holds.
/// Returns whether a helper was started (the caller then calls [`finish_tool_prefill`]).
fn start_tool_prefill(
    st: &SessionState,
    prepared: &[Prepared],
    outputs: &[Option<(ToolOutput, Duration)>],
    i: usize,
    early: &mut [Option<Early>],
) -> bool {
    if i == 0
        || !matches!(prepared[i].plan, Plan::Run { .. })
        || !st.settings.prefill_tools()
        || !st.info.prefill_only
        || st.settings.string("cacheWarming").as_deref() == Some("off")
        || st.shared.cancel.is_cancelled()
    {
        return false;
    }
    // A hook that can replace a result or rewrite the request makes the final bytes unknown
    // now; an extension that decides cache warming is not overridden.
    let hooked = [
        HookPoint::ToolResult,
        HookPoint::Context,
        HookPoint::ContextWithSystem,
        HookPoint::BeforeProviderRequest,
        HookPoint::CacheWarmingDecision,
    ]
    .iter()
    .any(|p| crate::fire::wants(st, *p));
    if hooked {
        return false;
    }
    // Only inside the agent loop: the wire ends with the assistant message of these calls.
    let wire = st.transcript.wire();
    let ours = wire.last().is_some_and(|m| {
        m.role == oxa_proto::Role::Assistant
            && m.tool_calls.len() == prepared.len()
            && m.tool_calls.iter().zip(prepared).all(|(c, p)| c.id == p.id)
    });
    if !ours {
        return false;
    }
    let cap = crate::tune::output_cap(st);
    let mut msgs: Vec<WireMsg> = wire.to_vec();
    let mut bytes = 0usize;
    for k in 0..i {
        let Some((out, _)) = &outputs[k] else {
            return false;
        };
        if early[k]
            .as_ref()
            .is_none_or(|e| e.raw != out.text || e.cap != cap)
        {
            let shaped = oxa_tools::shape(out.text.as_bytes(), cap, prepared[k].exec, &st.spill_dir);
            early[k] = Some(Early {
                raw: out.text.clone(),
                cap,
                shaped,
            });
        }
        let shaped = &early[k].as_ref().expect("shaped above").shaped;
        bytes += shaped.len();
        msgs.push(WireMsg::tool_result(prepared[k].id.clone(), shaped.clone()));
    }
    if bytes < TOOL_PREFILL_MIN_BYTES {
        return false;
    }
    // A previous helper of this session (none expected) ends first; one that does not end
    // in time blocks a new one.
    settle_tool_prefill(&st.shared);
    if tool_prefill_registered(&st.shared) {
        return false;
    }
    let cancel = oxa_proto::CancelToken::new();
    let cell = Arc::new(PrefillCell {
        cancel: cancel.clone(),
        handle: std::sync::Mutex::new(None),
    });
    let engine = st.engine.clone();
    let prefix = st.prefix.clone();
    let model = st.model.clone();
    let session = st.sref.id.clone();
    let idem = new_idem_key(st);
    let thinking = crate::think::request_thinking(st, TurnKind::Prefill);
    let preserve_thinking = st.info.caps.preserve_thinking != Some(false);
    // F11: the hint whenever the engine keeps checkpoints, as the idle prefill sends it.
    let snapshot_keep = st.info.caps.snapshot_keep > 0;
    let user_abort = st.shared.cancel.clone();
    let after = tool_prefill_after();
    let spawned = std::thread::Builder::new()
        .name("oxa-tool-prefill".to_string())
        .spawn(move || -> PrefillOutcome {
            let deadline = Instant::now() + after;
            loop {
                if cancel.is_cancelled() || user_abort.is_cancelled() {
                    return Ok(false);
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
            }
            let req = ChatRequest {
                prefix: &prefix,
                messages: &msgs,
                model: &model,
                max_tokens: 0,
                thinking,
                thinking_budget: None,
                preserve_thinking,
                sampling: None,
                hints: Hints {
                    session: &session,
                    idem_key: &idem,
                    kind: TurnKind::Prefill,
                    background: true,
                    snapshot_keep,
                },
            };
            // The user's abort ends the request at once, not only when the tool returns.
            let done = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| {
                let _ = std::thread::Builder::new()
                    .name("oxa-tool-prefill-watch".to_string())
                    .spawn_scoped(scope, || {
                        while !done.load(Ordering::SeqCst) {
                            if user_abort.is_cancelled() {
                                cancel.cancel();
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    });
                let res = engine.chat(&req, &cancel, &mut |_| {}).map(|_| true);
                done.store(true, Ordering::SeqCst);
                res
            })
        });
    let Ok(handle) = spawned else {
        return false;
    };
    *cell.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    active()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((shared_key(&st.shared), cell));
    true
}

/// F1: the running call returned: end the prefill (cancel, bounded join). Returns whether a
/// request was sent for this batch (then no other is sent). A failure other than the cancel is
/// one warning notice; nothing is recorded either way.
fn finish_tool_prefill(guard: PrefillGuard, sink: &mut dyn EventSink) -> bool {
    let outcome = settle_tool_prefill(&guard.0);
    drop(guard);
    match outcome {
        Some(Ok(sent)) => sent,
        Some(Err(e)) => {
            if e.code != codes::ABORTED {
                sink.emit(&AgentEvent::Notice {
                    level: Level::Warning,
                    text: format!("tool-result prefill skipped: {}", e.message),
                });
            }
            true
        }
        // Not ended in time (or the helper panicked): treat as sent, never start another.
        None => true,
    }
}

fn prepare(st: &mut SessionState, call: &ToolCall, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Prepared {
    let mk = |args: Value, exec: bool, plan: Plan| Prepared {
        id: call.id.clone(),
        name: call.name.clone(),
        args,
        exec,
        plan,
        writes: Vec::new(),
        checked: Vec::new(),
    };
    let Some(tool) = st.active.get(&call.name).cloned() else {
        let text = tool_errors::UNKNOWN_TOOL.replace("{name}", &call.name);
        return mk(Value::Null, false, Plan::Done(ToolOutput::error(text)));
    };
    let exec = tool.spec().class == ToolClass::Exec;
    let Some(mut args) = parse_args(&call.args_raw) else {
        return mk(
            Value::Null,
            exec,
            Plan::Done(ToolOutput::error(bad_args_text(call))),
        );
    };

    // Hook `ToolCall`: block, or replace the arguments that execution sees (the wire keeps the
    // original `args_raw`, W2).
    for h in st.hooks.clone() {
        let payload = serde_json::json!({"toolName": call.name, "toolCallId": call.id, "input": args});
        match fire_hook(st, &h, HookPoint::ToolCall, &payload, sink, ui) {
            HookReply::Continue => {}
            HookReply::ReplaceArgs(s) => match parse_args(&s) {
                Some(v) => args = v,
                None => return mk(args, exec, Plan::Done(ToolOutput::error(tool_errors::BAD_JSON))),
            },
            HookReply::Block(reason) => return mk(args, exec, Plan::Done(ToolOutput::error(reason))),
            _ => break,
        }
    }

    let accesses = {
        let cx = tool_ctx(st);
        match tool.access(&args, &cx) {
            Ok(a) => a,
            Err(msg) => return mk(args, exec, Plan::Done(ToolOutput::error(msg))),
        }
    };
    let mut checked = Vec::new();
    for a in &accesses {
        let (decision, pin) = st.policy.decide_pinned(st.mode, a, &st.cwd);
        if let (Access::Read(p) | Access::Write(p), Some(real)) = (a, pin) {
            checked.push((p.clone(), real));
        }
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                let text = format!("{}{}", tool_errors::DENIED_PREFIX, reason);
                return mk(args, exec, Plan::Done(ToolOutput::error(text)));
            }
            Decision::Ask => match crate::fire::ask_access(st, a, sink, ui) {
                Answer::Once => {}
                Answer::Session => st.policy.remember(a),
                Answer::Deny => {
                    // #554: with no one to ask, nobody declined; say what was refused and why.
                    let text = if ui.interactive() {
                        tool_errors::DENIED_BY_USER.to_string()
                    } else {
                        tool_errors::DENIED_UNATTENDED.replace("{what}", &crate::fire::access_title(a))
                    };
                    return mk(args, exec, Plan::Done(ToolOutput::error(text)));
                }
            },
        }
    }
    // X4: canonicalise BEFORE sorting and deduplicating, so the lock order is the order of the
    // identities actually locked.
    let writes = crate::MutationQueue::keys(accesses.iter().filter_map(|a| match a {
        Access::Write(p) => Some(p.as_path()),
        _ => None,
    }));
    Prepared {
        writes,
        checked,
        ..mk(args, exec, Plan::Run { tool })
    }
}

/// The name of the environment variable that carries the id of the running tool call to the
/// tool (`ToolCtx::env`; extension tools pass it to `execute` as `toolCallId`).
pub const TOOL_CALL_ID_ENV: &str = oxa_proto::names::ENV_TOOL_CALL_ID;

/// What a tool run needs from the state, owned, so that the run can leave the session thread.
struct OwnedCtx {
    cwd: std::path::PathBuf,
    spill_dir: std::path::PathBuf,
    sandbox: Arc<dyn oxa_proto::Sandbox>,
    mode: oxa_proto::Mode,
    cancel: oxa_proto::CancelToken,
    env: Vec<(String, String)>,
    tune: oxa_proto::ToolTune,
    checked: Vec<(std::path::PathBuf, std::path::PathBuf)>,
    hidden: Option<Arc<dyn oxa_proto::PathFilter>>,
}

impl OwnedCtx {
    fn of(
        st: &SessionState,
        call_id: &str,
        checked: &[(std::path::PathBuf, std::path::PathBuf)],
    ) -> OwnedCtx {
        let mut env = st.tool_env.clone();
        if !call_id.is_empty() {
            env.push((TOOL_CALL_ID_ENV.to_string(), call_id.to_string()));
        }
        OwnedCtx {
            cwd: st.cwd.clone(),
            spill_dir: st.spill_dir.clone(),
            sandbox: st.sandbox.clone(),
            mode: st.mode,
            cancel: st.shared.cancel.clone(),
            env,
            tune: st.tune,
            checked: checked.to_vec(),
            hidden: st.policy.path_filter(),
        }
    }

    fn cx(&self) -> ToolCtx<'_> {
        ToolCtx {
            checked: &self.checked,
            cwd: &self.cwd,
            spill_dir: &self.spill_dir,
            sandbox: self.sandbox.as_ref(),
            mode: self.mode,
            cancel: &self.cancel,
            env: &self.env,
            tune: self.tune,
            hidden: self.hidden.clone(),
        }
    }
}

/// Run one tool (`run_with_ui` when it `needs_ui`). With a host, the tool runs on a helper
/// thread while the session thread serves host requests (E-INT 2: an extension tool or a
/// codemode script may call back into the session); its partial output reaches `live` on the
/// session thread. Without a host it runs inline. `call_id` (may be empty) is passed to the
/// tool as [`TOOL_CALL_ID_ENV`]; `checked` is what the permission check pinned (#518).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_tool(
    st: &mut SessionState,
    tool: &Arc<dyn Tool>,
    args: &Value,
    call_id: &str,
    checked: &[(std::path::PathBuf, std::path::PathBuf)],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    live: &mut dyn FnMut(&mut dyn EventSink, &str),
) -> ToolOutput {
    let owned = OwnedCtx::of(st, call_id, checked);
    if st.host_rx.is_none() {
        let cx = owned.cx();
        let mut direct = |partial: &str| live(&mut *sink, partial);
        return contained_tool(tool, || {
            if tool.needs_ui() {
                tool.run_with_ui(args, &cx, &mut direct, ui)
            } else {
                tool.run(args, &cx, &mut direct)
            }
        });
    }
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let owned = &owned;
    let mut tick = |sink: &mut dyn EventSink| {
        while let Ok(partial) = rx.try_recv() {
            live(sink, &partial);
        }
    };
    pumped(st, Serve::Run, sink, ui, &mut tick, move |ui| {
        let cx = owned.cx();
        let mut forward = |partial: &str| {
            let _ = tx.send(partial.to_string());
        };
        contained_tool(tool, || {
            if tool.needs_ui() {
                tool.run_with_ui(args, &cx, &mut forward, ui)
            } else {
                tool.run(args, &cx, &mut forward)
            }
        })
    })
}

/// A1/A2: run one tool call; a panic in the tool becomes an error result
/// ([`tool_errors::PANICKED`]) and the turn goes on. Contained on the thread that runs the tool,
/// so the panic hook stays quiet and the terminal is left alone.
pub(crate) fn contained_tool(tool: &Arc<dyn Tool>, run: impl FnOnce() -> ToolOutput) -> ToolOutput {
    oxa_proto::panic::contain(run).unwrap_or_else(|error| {
        ToolOutput::error(
            tool_errors::PANICKED
                .replace("{name}", &tool.spec().name)
                .replace("{error}", &error),
        )
    })
}

/// Run one call (serial path; `run_with_ui` for tools that open dialogs).
fn run_one(
    st: &mut SessionState,
    p: &Prepared,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> (ToolOutput, Duration) {
    let Plan::Run { tool } = &p.plan else {
        return (plan_output(p), Duration::ZERO);
    };
    crate::fire::tool_start(st, &p.id, &p.name, &p.args, sink, ui);
    if st.shared.cancel.is_cancelled() {
        return (ToolOutput::error(tool_errors::ABORTED), Duration::ZERO);
    }
    let started = Instant::now();
    let hook_updates = crate::fire::wants(st, HookPoint::ToolExecutionUpdate);
    let mut live = |sink: &mut dyn EventSink, partial: &str| {
        sink.emit(&AgentEvent::ToolExecutionUpdate {
            tool_call_id: p.id.clone(),
            tool_name: p.name.clone(),
            args: p.args.clone(),
            partial: partial.to_string(),
        });
        if hook_updates {
            let payload = crate::fire::tool_update_payload(&p.id, &p.name, &p.args, partial);
            crate::fire::defer(HookPoint::ToolExecutionUpdate, payload);
        }
    };
    // C7: a call that writes holds its paths in the mutation queue while it runs.
    let out = with_writes(&p.writes, || {
        run_tool(st, tool, &p.args, &p.id, &p.checked, sink, ui, &mut live)
    });
    crate::fire::drain(st, sink, ui);
    (out, started.elapsed())
}

/// Run a batch of parallel-safe reads on scoped threads. Results come back in batch order.
/// While the batch runs, the session thread serves host requests (a read-class tool may still
/// call back into the session).
fn run_parallel(
    st: &mut SessionState,
    batch: &[Prepared],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Vec<(ToolOutput, Duration)> {
    for p in batch {
        crate::fire::tool_start(st, &p.id, &p.name, &p.args, sink, ui);
    }
    let owned: Vec<OwnedCtx> = batch
        .iter()
        .map(|p| OwnedCtx::of(st, &p.id, &p.checked))
        .collect();
    let cancelled = st.shared.cancel.is_cancelled();
    std::thread::scope(|scope| {
        let handles: Vec<_> = batch
            .iter()
            .zip(&owned)
            .map(|(p, owned)| {
                let Plan::Run { tool } = &p.plan else {
                    unreachable!("a parallel batch holds runnable calls only")
                };
                let tool = Arc::clone(tool);
                scope.spawn(move || {
                    if cancelled {
                        return (ToolOutput::error(tool_errors::ABORTED), Duration::ZERO);
                    }
                    let started = Instant::now();
                    let mut none = |_: &str| {};
                    let out = contained_tool(&tool, || tool.run(&p.args, &owned.cx(), &mut none));
                    (out, started.elapsed())
                })
            })
            .collect();
        if st.host_rx.is_some() {
            let mut pause = Duration::from_micros(200);
            while handles.iter().any(|h| !h.is_finished()) {
                let served = serve_level(st, Serve::Run, sink, ui);
                pause = if served {
                    Duration::from_micros(200)
                } else {
                    (pause * 2).min(Duration::from_millis(5))
                };
                std::thread::sleep(pause);
            }
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

/// The output of a call that finished before running.
fn plan_output(p: &Prepared) -> ToolOutput {
    match &p.plan {
        Plan::Done(o) => o.clone(),
        Plan::Run { .. } => ToolOutput::error(tool_errors::ABORTED),
    }
}

/// Hook `ToolResult`: the first `ReplaceResult` wins; any other reply ends the chain.
fn apply_result_hook(
    st: &mut SessionState,
    p: &Prepared,
    output: ToolOutput,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> ToolOutput {
    let payload = serde_json::json!({
        "toolName": p.name,
        "toolCallId": p.id,
        "input": p.args,
        "content": output.text,
        "isError": output.is_error,
        "details": output.ui,
    });
    for h in st.hooks.clone() {
        match fire_hook(st, &h, HookPoint::ToolResult, &payload, sink, ui) {
            HookReply::Continue => {}
            HookReply::ReplaceResult(o) => return o,
            _ => break,
        }
    }
    output
}

fn tool_ctx(st: &SessionState) -> ToolCtx<'_> {
    ToolCtx {
        checked: &[],
        cwd: &st.cwd,
        spill_dir: &st.spill_dir,
        sandbox: st.sandbox.as_ref(),
        mode: st.mode,
        cancel: &st.shared.cancel,
        env: &st.tool_env,
        tune: st.tune,
        hidden: st.policy.path_filter(),
    }
}

/// The error text for arguments that are not a JSON object; an empty string says so, so the
/// model (and the progress line) can tell why and retry (#483).
fn bad_args_text(call: &ToolCall) -> String {
    if call.args_raw.trim().is_empty() {
        format!(
            "{} (empty arguments: call `{}` again with its arguments as a JSON object)",
            tool_errors::BAD_JSON,
            call.name
        )
    } else {
        tool_errors::BAD_JSON.to_string()
    }
}

/// A JSON object or nothing (`BAD_JSON`).
fn parse_args(raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw).ok().filter(Value::is_object)
}

// ---------------------------------------------------------------------------------------------
// Streaming translation

/// Turns the engine's stream increments into `MessageUpdate` events. Indices follow pi's
/// `contentIndex`: the thinking block first, then text, then tool calls in stream order.
/// Every block that starts also ends (pi's `*_end` with the block's complete content): a block
/// ends when the next one starts, and the blocks still open when the reply is complete end
/// with the reply's authoritative content ([`StreamState::finish`]).
#[derive(Default)]
struct StreamState {
    thinking: bool,
    text: bool,
    tools: Vec<bool>,
    /// Content of the open thinking / text block (`None` = not open).
    open_thinking: Option<String>,
    open_text: Option<(usize, String)>,
    /// The open tool-call block: (call index, content index, id, name, argument bytes).
    open_tool: Option<(usize, usize, String, String, String)>,
}

impl StreamState {
    /// End the open thinking block, if any.
    fn end_thinking(&mut self, sink: &mut dyn EventSink, events: bool) {
        if let Some(content) = self.open_thinking.take() {
            if events {
                update(sink, MessageDelta::ThinkingEnd { index: 0, content });
            }
        }
    }

    /// End the open text block, if any.
    fn end_text(&mut self, sink: &mut dyn EventSink, events: bool) {
        if let Some((index, content)) = self.open_text.take() {
            if events {
                update(sink, MessageDelta::TextEnd { index, content });
            }
        }
    }

    /// End the open tool-call block, if any, with the call as streamed.
    fn end_tool(&mut self, sink: &mut dyn EventSink, events: bool) {
        if let Some((_, index, id, name, args_raw)) = self.open_tool.take() {
            if events {
                let call = ToolCall { id, name, args_raw };
                update(sink, MessageDelta::ToolcallEnd { index, call });
            }
        }
    }

    /// The reply is complete: end the blocks still open with its authoritative content.
    fn finish(&mut self, turn: &AssistantTurn, sink: &mut dyn EventSink) {
        if let Some(content) = self.open_thinking.as_mut() {
            if let Some(r) = &turn.msg.reasoning {
                *content = r.clone();
            }
        }
        self.end_thinking(sink, true);
        if let Some((_, content)) = self.open_text.as_mut() {
            if let Some(t) = &turn.msg.content {
                *content = t.clone();
            }
        }
        self.end_text(sink, true);
        if let Some((n, _, id, name, args_raw)) = self.open_tool.as_mut() {
            if let Some(call) = turn.msg.tool_calls.get(*n) {
                id.clone_from(&call.id);
                name.clone_from(&call.name);
                args_raw.clone_from(&call.args_raw);
            }
        }
        self.end_tool(sink, true);
    }

    fn handle(&mut self, ev: StreamEvent, sink: &mut dyn EventSink, events: bool) {
        match ev {
            StreamEvent::Prefill { done, total, secs } => {
                sink.emit(&AgentEvent::Prefill { done, total, secs });
            }
            StreamEvent::Queued { pos, wait_s } => {
                sink.emit(&AgentEvent::EngineQueue { pos, wait_s });
            }
            StreamEvent::Reasoning(delta) => {
                if !self.thinking {
                    self.thinking = true;
                    self.open_thinking = Some(String::new());
                    if events {
                        update(sink, MessageDelta::ThinkingStart { index: 0 });
                    }
                }
                if let Some(content) = self.open_thinking.as_mut() {
                    content.push_str(&delta);
                }
                if events {
                    update(sink, MessageDelta::ThinkingDelta { index: 0, delta });
                }
            }
            StreamEvent::Text(delta) => {
                let index = self.thinking as usize;
                if !self.text {
                    self.end_thinking(sink, events);
                    self.text = true;
                    self.open_text = Some((index, String::new()));
                    if events {
                        update(sink, MessageDelta::TextStart { index });
                    }
                }
                if let Some((_, content)) = self.open_text.as_mut() {
                    content.push_str(&delta);
                }
                if events {
                    update(sink, MessageDelta::TextDelta { index, delta });
                }
            }
            StreamEvent::ToolArgs {
                index,
                id,
                name,
                delta,
            } => {
                if self.tools.len() <= index {
                    self.tools.resize(index + 1, false);
                }
                let at = self.thinking as usize + self.text as usize + index;
                if !self.tools[index] {
                    self.end_thinking(sink, events);
                    self.end_text(sink, events);
                    self.end_tool(sink, events);
                    self.tools[index] = true;
                    let (id, name) = (id.unwrap_or_default(), name.unwrap_or_default());
                    self.open_tool = Some((index, at, id.clone(), name.clone(), String::new()));
                    if events {
                        update(sink, MessageDelta::ToolcallStart { index: at, id, name });
                    }
                }
                if let Some((n, _, _, _, args)) = self.open_tool.as_mut() {
                    if *n == index {
                        args.push_str(&delta);
                    }
                }
                if events {
                    update(sink, MessageDelta::ToolcallDelta { index: at, delta });
                }
            }
        }
    }
}

fn update(sink: &mut dyn EventSink, delta: MessageDelta) {
    sink.emit(&AgentEvent::MessageUpdate {
        usage: Usage::default(),
        delta,
    });
}

// ---------------------------------------------------------------------------------------------
// W-guard

/// Fire one prefix-checked hook point through the hook chain.
///
/// * `Rebase(list)`: accepted only when the current wire is a prefix of `list` (an append; only
///   plain user messages, so the fold replays them exactly). Anything else is refused with a
///   `guard_refused` notice and the request goes out unchanged (W1). This is the path of every
///   source that did not declare a rebase.
/// * `DeclaredRebase { messages, origin }` (E-INT 3): the origin flag of the reply path. The
///   wire is replaced with `rebase_why::EXTENSION` and the rebase is logged; the cache miss of
///   the request that follows has the cause `extension`.
fn guard_hooks(
    st: &mut SessionState,
    point: HookPoint,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    if !crate::fire::wants(st, point) {
        return Ok(());
    }
    // pi's `context_with_system` carries the full transcript with the system message first.
    // The system text is the frozen prefix (W3): a reply keeps it byte for byte (it is
    // stripped before the guard path below) or the reply is refused.
    let system = (point == HookPoint::ContextWithSystem).then(|| WireMsg::system(st.prefix.system_text()));
    let payload = match &system {
        Some(sys) => {
            let mut full = vec![sys.clone()];
            full.extend(st.transcript.wire().iter().cloned());
            context_payload(&full)
        }
        None => context_payload(st.transcript.wire()),
    };
    let strip = |list: Vec<WireMsg>, sink: &mut dyn EventSink| -> Option<Vec<WireMsg>> {
        let Some(sys) = &system else {
            return Some(list);
        };
        if list.first() == Some(sys) {
            return Some(list[1..].to_vec());
        }
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: format!(
                "{}: a context_with_system hook changed the system prompt (frozen prefix, W3); the request is sent unchanged",
                codes::GUARD_REFUSED
            ),
        });
        None
    };
    for h in st.hooks.clone() {
        match fire_hook(st, &h, point, &payload, sink, ui) {
            HookReply::Continue => {}
            HookReply::Rebase(list) => {
                if let Some(list) = strip(list, sink) {
                    apply_rebase(st, list, None, point, sink)?;
                }
                break;
            }
            HookReply::DeclaredRebase { messages, origin } => {
                if let Some(list) = strip(messages, sink) {
                    apply_rebase(st, list, Some(&origin), point, sink)?;
                }
                break;
            }
            _ => break,
        }
    }
    Ok(())
}

/// Apply a message list returned at a prefix-checked point. `declared_by` is the origin of a
/// declared rebase (`None` = not declared: appends only).
fn apply_rebase(
    st: &mut SessionState,
    list: Vec<WireMsg>,
    declared_by: Option<&str>,
    point: HookPoint,
    sink: &mut dyn EventSink,
) -> Result<(), OxaError> {
    let wire = st.transcript.wire();
    if list.as_slice() == wire {
        return Ok(());
    }
    if wire_is_prefix(wire, &list) {
        let extra = &list[wire.len()..];
        let plain: Option<Vec<String>> = extra.iter().map(as_plain_user).collect();
        if let Some(texts) = plain {
            for content in texts {
                persist(
                    st,
                    EntryBody::CustomMessage {
                        custom_type: "context".to_string(),
                        content,
                        display: false,
                        details: Value::Null,
                    },
                    sink,
                )?;
            }
            return Ok(());
        }
    }
    let Some(origin) = declared_by else {
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: format!(
                "{}: a hook rewrote the wire without extending it; the request is sent unchanged",
                codes::GUARD_REFUSED
            ),
        });
        return Ok(());
    };
    // A declared rebase: the one door through which an extension may change the wire. It is
    // logged three ways: the transcript's rebase log (cause `extension`, which names the cache
    // miss that follows), a `custom` entry in the session file and a notice for the user.
    // The entry carries the rebased wire and the rebase IS the fold of that entry
    // (`append_entry` folds it, as compaction does), so a resumed session replays exactly the
    // wire that was sent.
    let before = wire.len();
    let after = list.len();
    persist(
        st,
        EntryBody::Custom {
            custom_type: EXTENSION_REBASE_ENTRY.to_string(),
            data: serde_json::json!({
                "extension": origin,
                "event": point.as_str(),
                "why": rebase_why::EXTENSION,
                "before": before,
                "after": after,
                "messages": oxa_session::Transcript::encode_rebase(&list),
            }),
        },
        sink,
    )?;
    debug_assert_eq!(st.transcript.wire(), list.as_slice());
    sink.emit(&AgentEvent::Notice {
        level: Level::Warning,
        text: format!(
            "extension {origin} rebased the context at \"{}\" (declared rebase, {before} -> {after} messages); \
             the next request misses the cache (cause: {})",
            point.as_str(),
            oxa_proto::miss_cause::EXTENSION
        ),
    });
    Ok(())
}

/// `customType` of the session entry that records a declared extension rebase.
pub const EXTENSION_REBASE_ENTRY: &str = oxa_session::Transcript::EXTENSION_REBASE_ENTRY;

/// The text of a message that is exactly `WireMsg::user(text)`.
fn as_plain_user(m: &WireMsg) -> Option<String> {
    let c = m.content.clone()?;
    (*m == WireMsg::user(c.clone())).then_some(c)
}

fn context_payload(wire: &[WireMsg]) -> Value {
    // Every wire field is carried (`reasoning`, `toolCallId`, `toolCalls` with the argument
    // string as received), so a handler that returns the list unchanged, or with messages
    // appended, yields a list the W-guard sees as an extension of the wire. Absent fields are
    // left out.
    let msgs: Vec<Value> = wire
        .iter()
        .map(|m| {
            let mut o = serde_json::Map::new();
            o.insert("role".to_string(), Value::String(m.role.as_str().to_string()));
            o.insert(
                "content".to_string(),
                m.content.clone().map(Value::String).unwrap_or(Value::Null),
            );
            if let Some(r) = &m.reasoning {
                o.insert("reasoning".to_string(), Value::String(r.clone()));
            }
            if let Some(id) = &m.tool_call_id {
                o.insert("toolCallId".to_string(), Value::String(id.clone()));
            }
            if !m.tool_calls.is_empty() {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| serde_json::json!({"id": c.id, "name": c.name, "args": c.args_raw}))
                    .collect();
                o.insert("toolCalls".to_string(), Value::Array(calls));
            }
            Value::Object(o)
        })
        .collect();
    serde_json::json!({ "messages": msgs })
}

/// A fresh idempotency key (H2), unique per request within the process.
fn new_idem_key(st: &SessionState) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(st.sref.id.as_bytes());
    h.update(nanos.to_le_bytes());
    h.update(n.to_le_bytes());
    let digest: [u8; 32] = h.finalize().into();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the assistant `EventMessage` of a finished reply (used by the loop).
pub(crate) fn reply_event(turn: &AssistantTurn, ts_ms: u64) -> EventMessage {
    EventMessage {
        msg: turn.msg.clone(),
        ts_ms,
        stop: Some(StopReason::from(turn.finish)),
        usage: Some(turn.usage),
        model: Some(turn.model.clone()),
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

#[cfg(test)]
mod bad_args_tests {
    use super::*;

    fn call(raw: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "read".into(),
            args_raw: raw.into(),
        }
    }

    #[test]
    fn empty_arguments_get_a_retry_hint() {
        for raw in ["", "  \n"] {
            let t = bad_args_text(&call(raw));
            assert!(t.starts_with(tool_errors::BAD_JSON), "{t}");
            assert!(t.contains("empty arguments") && t.contains("`read`"), "{t}");
        }
        assert_eq!(bad_args_text(&call("[1]")), tool_errors::BAD_JSON);
    }
}
