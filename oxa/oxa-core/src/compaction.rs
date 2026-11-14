//! C3: compaction orchestration (W5). Owner: task C3. Signatures below are frozen; bodies are yours.
// adapted from pi packages/coding-agent/src/core/agent-session.ts (compact, auto-compaction)
//
// Byte rules (W1, W5). The summary request is the whole wire plus one user instruction, sent with
// `request` (never pushed). The rebase happens only through `append_entry` of the `Compaction`
// entry: the fold of that entry in `Transcript::apply` produces exactly `oxa_compact::head_after`,
// so the stored entry's `summary` is the full wrapped user message (`prompt::summary_user_message`
// of the summary text plus file lists). A resumed session replays the same bytes.
use crate::agent_loop::persist;
use crate::exec::{request, RequestOpts};
use crate::SessionState;
use oxa_compact::prompt;
use oxa_compact::{decide, file_lists, first_kept_index, summary_request_with_target, When};
use oxa_proto::{
    codes, AgentEvent, AssistantTurn, CompactReason, CompactionResult, Entry, EntryBody, EventSink, Finish,
    HookPoint, HookReply, Level, OxaError, Profile, Role, TurnKind, Ui, Usage, Value, WireMsg,
};
use serde_json::json;
use sha2::{Digest, Sha256};

/// What compaction remembers between runs, in memory only (never persisted: a resumed session
/// starts with none). `Default` = nothing remembered.
///
/// * The pending idle summary (V020-OPTIMIZE #7, `oxa.compaction.idleSummary`): a summary made
///   ahead in idle time and not applied yet; see [`maybe_compact`].
/// * The last idle-summary attempt (its `MemoKey` and wire length): one attempt per wire.
/// * The cancelled summaries of the current compaction need (OXA-LOCAL-FIRST R4, profile
///   `local`): the second cancel in a row builds the mechanical summary.
#[derive(Debug, Default, Clone)]
pub struct CompactMemo {
    pending: Option<Pending>,
    idle_tried: Option<(MemoKey, usize)>,
    cancels: Option<(MemoKey, u32)>,
}

/// What a remembered summary depends on besides the wire messages: the session, the prefix
/// epoch, the model and the rebases of the transcript. Any rebase (compaction, leaf move,
/// context edit, extension rewrite, epoch), a model switch, a fork or a session switch changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoKey {
    session: String,
    prefix: [u8; 32],
    model: String,
    epochs: u32,
    rebases: usize,
    last_rebase: Option<[u8; 32]>,
}

/// A summary computed ahead in idle time.
#[derive(Debug, Clone)]
struct Pending {
    key: MemoKey,
    /// Wire messages the summary request covered (`wire[..covered]`) and their digest.
    covered: usize,
    digest: [u8; 32],
    /// The cut it was made for (index into the wire), as [`cut_point`] computed it then.
    cut: usize,
    /// The summary text (without file lists).
    text: String,
    usage: Usage,
}

fn memo_key(st: &SessionState) -> MemoKey {
    let rebases = st.transcript.rebases();
    MemoKey {
        session: st.sref.id.clone(),
        prefix: st.prefix.sha256,
        model: st.model.clone(),
        epochs: st.stats.epochs,
        rebases: rebases.len(),
        last_rebase: rebases.last().map(|r| r.chain),
    }
}

/// Digest of the first `n` wire messages with their entry origins.
fn wire_digest(st: &SessionState, n: usize) -> [u8; 32] {
    let mut h = Sha256::new();
    for (i, m) in st.transcript.wire()[..n].iter().enumerate() {
        h.update(m.to_json());
        h.update([0u8]);
        h.update(st.transcript.origin(i).unwrap_or("").as_bytes());
        h.update([0u8]);
    }
    h.finalize().into()
}

/// The pending summary still describes this wire: same key, the covered messages unchanged.
fn pending_valid(st: &SessionState, p: &Pending) -> bool {
    p.key == memo_key(st) && st.transcript.wire().len() >= p.covered && wire_digest(st, p.covered) == p.digest
}

/// What one compaction attempt did.
enum Outcome {
    Compacted,
    /// Nothing to summarise, cancelled by a hook, or aborted by the user. The wire is unchanged.
    Skipped,
    /// The summary failed. The wire is unchanged.
    Failed(OxaError),
}

/// A compaction applied to the transcript whose end events are still to be emitted.
struct Applied {
    /// The summary text with file lists (unwrapped).
    summary: String,
    first_kept: String,
    tokens_before: u32,
    details: Value,
    from_hook: bool,
    usage: Option<Usage>,
    head: Vec<WireMsg>,
    /// R4 (`local`): why the model summary could not be made when the mechanical summary
    /// replaced it; `None` otherwise.
    summary_error: Option<String>,
}

/// Decide with `oxa_compact::decide(transcript.context(), run_ended, &st.compact)` and, unless
/// `No` (or auto-compaction is off and the answer is `AfterReply`), run [`compact_now`] with
/// reason `Threshold` (`AfterReply`) or `Overflow` (`MidRun`). Returns whether it compacted.
///
/// A failed `AfterReply` summary is reported by `CompactionEnd { error_message }` and returns
/// `Ok(false)`: the reply is already stored and the next prompt still works. A failed `MidRun`
/// summary is returned, since the next request would overflow. Under profile `local` a failed
/// summary is replaced by the mechanical one instead (R4, see [`compact_now`]).
///
/// Idle summary (V020-OPTIMIZE #7, `oxa.compaction.idleSummary`): at the end of a run whose
/// context is above 80% of `soft` and not above it, the summary request a threshold compaction
/// would send is sent ahead (background, cancelled by user input) and its text is kept as the
/// pending summary, without a rebase. A later compaction without custom instructions and
/// without a model-switch fit applies it with no model request when it is still valid (same
/// session, prefix epoch, model and rebases, the covered wire messages unchanged) and its kept tail (everything from its
/// cut on, including what was appended since) is at most `2 * keep_tokens + soft / 5` tokens;
/// otherwise it is dropped and the compaction runs as usual. One attempt per wire length; a
/// cancelled or failed idle summary is silent.
pub fn maybe_compact(
    st: &mut SessionState,
    run_ended: bool,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<bool, OxaError> {
    // The runtime switch decides threshold compaction (it overrides `compaction.enabled`); the
    // hard-overflow path does not depend on it.
    let policy = oxa_compact::CompactPolicy {
        enabled: st.auto_compaction,
        ..st.compact
    };
    let reason = match decide(st.transcript.context(), run_ended, &policy) {
        When::No => {
            if run_ended {
                idle_summary(st, sink, ui);
            }
            return Ok(false);
        }
        When::AfterReply if !st.auto_compaction => return Ok(false),
        When::AfterReply => CompactReason::Threshold,
        When::MidRun => CompactReason::Overflow,
    };
    match run_compaction(st, &Spec::of(None, reason), sink, ui) {
        Outcome::Compacted => Ok(true),
        Outcome::Skipped => Ok(false),
        Outcome::Failed(e) if reason == CompactReason::Overflow => Err(e),
        Outcome::Failed(_) => Ok(false),
    }
}

/// Compact now. Sequence: emit `CompactionStart` -> hook `SessionBeforeCompact` (`Cancel` =
/// stop with `aborted`; `Summary` = use it, `from_hook`) -> otherwise one request: the whole
/// wire + `oxa_compact::summary_request`'s instruction as `RequestOpts::extra`, thinking off,
/// `background` only for `Threshold` (F12; `/compact` and overflow recovery are foreground),
/// `TurnKind::Compaction`, `max_tokens = compact.summary_max`, retried per pi's
/// summarization retry events -> `oxa_compact::head_after` -> append the `Compaction` entry
/// (details: file lists) -> `Transcript::rebase(head, compaction)` -> hook `SessionCompact`
/// -> emit `CompactionEnd` -> [`idle_prefill`]. Failure: `CompactionEnd { error_message }`,
/// hook `SessionCompactFailed`, wire unchanged.
///
/// Note: the rebase is the fold of the appended `Compaction` entry (`append_entry` applies it),
/// which logs exactly one `RebaseLog`; calling `Transcript::rebase` as well would log it twice.
/// The summary request is sent without the C2 retry wrapper (`chat_with_retry` may itself call
/// `compact_now` on overflow, so wrapping it would allow unbounded recursion); it has pi's
/// summarization retry instead (`retry::summary_with_retry`, no overflow handling).
///
/// Returns `Err` only when the summary failed; cancellation and "nothing to summarise" are `Ok`.
///
/// R4 (profile `local` only): when the summary request fails, its reply is cut or empty, or it
/// is cancelled by the user's input a second time in a row for the same compaction need, the
/// summary is built without the model (`oxa_compact::mechanical_summary`: the user messages,
/// the file lists, the count of tool results left out) and the compaction completes as usual,
/// with `details.mechanical: true` and one warning notice. The hook is `SessionCompact` (the
/// wire was compacted) with `mechanical: true` and `summaryError` (the failure's text) in its
/// payload; `SessionCompactFailed` does not fire. Under `pi` a failure is as above.
pub fn compact_now(
    st: &mut SessionState,
    instructions: Option<&str>,
    reason: CompactReason,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    match run_compaction(st, &Spec::of(instructions, reason), sink, ui) {
        Outcome::Failed(e) => Err(e),
        Outcome::Compacted | Outcome::Skipped => Ok(()),
    }
}

/// L5 (`oxa.compaction.turnStart`): compact before a prompt is sent when the context is above
/// the soft limit. Called by a prompt (and a queued run) after its hook sets accepted it and
/// before its messages are appended, so the prompt lands after the compacted head. Foreground
/// (the user waits): no background priority, no idle prefill (the prompt's own request follows
/// over the same head). A valid pending idle summary is applied with no request (see
/// [`usable_pending`]). Nothing is started when auto-compaction is off or the setting is off.
/// A failed summary is reported as `CompactionEnd { error_message }` (under `local` the
/// mechanical summary applies instead) and the prompt continues uncompacted. Returns whether it
/// compacted.
pub(crate) fn compact_at_turn_start(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> bool {
    if !st.auto_compaction || !st.settings.turn_start_compaction(st.stats.profile) {
        return false;
    }
    if context_estimate(st) <= st.compact.soft {
        return false;
    }
    let spec = Spec {
        instructions: None,
        reason: CompactReason::Threshold,
        background: false,
        prefill_after: false,
        fit: None,
        keep_last_batch: false,
    };
    matches!(run_compaction(st, &spec, sink, ui), Outcome::Compacted)
}

/// What [`compact_mid_run`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MidRun {
    /// Not attempted (setting or auto-compaction off, context at or below the soft limit, or
    /// nothing older than the last tool batch to cut): nothing emitted, no request sent.
    NotRun,
    /// The wire was compacted.
    Compacted,
    /// Attempted but the wire is unchanged: a hook cancelled, the user aborted, or the summary
    /// failed (reported by `CompactionEnd { error_message }`; under `local` the mechanical
    /// summary applies instead, so this is only a hook cancel or an abort there).
    Unchanged,
}

/// L5 mid-run (`oxa.compaction.midRun`): between two tool steps of a run (after the tool batch
/// and the delivered steering, before the next model request; never mid-stream), compact when
/// the context has passed the soft limit. Foreground (only an abort cancels it), reason
/// `Threshold`, no idle prefill after it (the run's next request follows over the new head), no
/// idle summary started; a valid pending idle summary is applied with no request.
///
/// The unanswered batch (the last assistant message with tool calls, its tool results and any
/// steering message after them) is always in the kept tail, verbatim: the cut is never after
/// the batch's assistant message and its tool results are never elided
/// ([`Spec::keep_last_batch`]). Every other message of the kept tail is treated as usual.
///
/// No thrash: the compaction runs only when the messages it would summarise (before the cut,
/// after a previous summary head) hold at least one assistant message, i.e. an earlier model
/// step older than the last batch. A run's first batch (only the prompt before it) and a wire
/// that is a summary head, maybe a steering or user message, then the last batch, are never
/// folded: the summary would replace little or nothing. Right after
/// a compaction the transcript has no usage (`context()` = 0, so [`context_estimate`] is 0)
/// until the next reply reports it, so the same wire is never compacted twice in a row; the
/// next check comes after a reply and a new batch, which makes the previous batch cuttable.
///
/// The hard-limit path ([`maybe_compact`]) runs after this one as the backstop, unchanged.
pub(crate) fn compact_mid_run(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> MidRun {
    if !st.auto_compaction || !st.settings.mid_run_compaction(st.stats.profile) {
        return MidRun::NotRun;
    }
    // Without usage (a rebase since the last reply) the estimate is 0: no compaction until a
    // reply reports the real context.
    if context_estimate(st) <= st.compact.soft {
        return MidRun::NotRun;
    }
    // No thrash: fold only when an earlier model step (an assistant message other than the
    // last batch's) lies before the cut, beyond a previous summary head.
    let Some(cut) = cut_point(st, When::AfterReply, None, true) else {
        return MidRun::NotRun;
    };
    let lower = usize::from(summary_head(st));
    if !st.transcript.wire()[lower..cut]
        .iter()
        .any(|m| m.role == Role::Assistant)
    {
        return MidRun::NotRun;
    }
    let spec = Spec {
        instructions: None,
        reason: CompactReason::Threshold,
        background: false,
        prefill_after: false,
        fit: None,
        keep_last_batch: true,
    };
    match run_compaction(st, &spec, sink, ui) {
        Outcome::Compacted => MidRun::Compacted,
        Outcome::Skipped | Outcome::Failed(_) => MidRun::Unchanged,
    }
}

/// L5 mid-run: index of the first message of the last tool batch (the last assistant message
/// that carries tool calls), or `None` when the wire has none.
fn last_batch_start(wire: &[WireMsg]) -> Option<usize> {
    wire.iter()
        .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
}

/// L5: the context size to decide a turn-start compaction with: [`Transcript::context`] (the
/// last reply's prompt + completion) plus the messages appended after that reply (an aborted
/// turn's tool result, a user message), estimated as their serialised bytes / 3 rounded up.
/// Without any usage (a fresh or just-rebased transcript) it is `context()`, i.e. 0.
pub(crate) fn context_estimate(st: &SessionState) -> u32 {
    estimate_of(&st.transcript)
}

/// [`context_estimate`] over a transcript alone.
fn estimate_of(t: &oxa_session::Transcript) -> u32 {
    let base = t.context();
    let Some(from) = t.usage_wire_len() else {
        return base;
    };
    let wire = t.wire();
    let bytes: usize = wire[from.min(wire.len())..]
        .iter()
        .map(|m| m.to_json().len())
        .sum();
    let appended = u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX);
    base.saturating_add(appended)
}

#[cfg(test)]
mod estimate_tests {
    use super::estimate_of;
    use oxa_proto::{rebase_why, Role, Usage, WireMsg};
    use oxa_session::Transcript;

    fn usage(prompt: u32, completion: u32) -> Usage {
        Usage {
            prompt,
            completion,
            cached: 0,
            cached_missing: false,
        }
    }

    fn tool_result(text: &str) -> WireMsg {
        WireMsg {
            role: Role::Tool,
            content: Some(text.to_string()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".to_string()),
        }
    }

    #[test]
    fn usage_only_is_the_reported_context() {
        let mut t = Transcript::default();
        t.push(WireMsg::user("hi"));
        t.set_usage(&usage(100, 20));
        assert_eq!(estimate_of(&t), 120);
    }

    #[test]
    fn appended_tool_result_is_estimated_at_a_third_of_its_bytes() {
        let mut t = Transcript::default();
        t.push(WireMsg::user("hi"));
        t.set_usage(&usage(100, 20));
        t.push(tool_result(&"x".repeat(3000)));
        let bytes = t.wire().last().expect("tool result").to_json().len();
        assert_eq!(estimate_of(&t), 120 + u32::try_from(bytes.div_ceil(3)).unwrap());
        assert!(estimate_of(&t) > 120 + 1000);
    }

    #[test]
    fn without_usage_there_is_no_estimate() {
        let mut t = Transcript::default();
        t.push(WireMsg::user("hi"));
        t.push(tool_result(&"x".repeat(3000)));
        assert_eq!(estimate_of(&t), 0);
    }

    #[test]
    fn a_rebase_drops_the_estimate_until_the_next_reply() {
        let mut t = Transcript::default();
        t.push(WireMsg::user("hi"));
        t.set_usage(&usage(100, 20));
        t.rebase(vec![WireMsg::user("summary")], rebase_why::COMPACTION);
        assert_eq!(estimate_of(&t), 0);
        t.push(tool_result(&"x".repeat(3000)));
        assert_eq!(estimate_of(&t), 0);
        t.set_usage(&usage(50, 10));
        t.push(tool_result(&"x".repeat(300)));
        let bytes = t.wire().last().expect("tool result").to_json().len();
        assert_eq!(estimate_of(&t), 60 + u32::try_from(bytes.div_ceil(3)).unwrap());
    }
}

/// How one compaction runs.
///
/// F12: only the threshold (after-reply) summary is a background request (`X-Oxe-Priority:
/// background`, cancelled by user input through `Gate::bg_cancel`). `/compact`, overflow
/// recovery, the pre-switch compaction of S3 and a branch summary are foreground: the user
/// waits on them, and only an abort cancels them.
pub(crate) struct Spec<'a> {
    pub instructions: Option<&'a str>,
    pub reason: CompactReason,
    pub background: bool,
    /// Idle prefill after a successful compaction (not for overflow recovery, where the
    /// retried request follows at once, nor before a model switch).
    pub prefill_after: bool,
    /// S3 / O-145: size the kept tail and the summary for another model's policy.
    pub fit: Option<Fit>,
    /// L5 mid-run: the last tool batch (see [`compact_mid_run`]) is inside the kept tail
    /// whatever its size and its tool results are never elided. `false` everywhere else.
    pub keep_last_batch: bool,
}

impl Spec<'_> {
    fn of(instructions: Option<&str>, reason: CompactReason) -> Spec<'_> {
        Spec {
            instructions,
            reason,
            background: reason == CompactReason::Threshold,
            prefill_after: reason != CompactReason::Overflow,
            fit: None,
            keep_last_batch: false,
        }
    }
}

/// The budget of a compaction that must fit a smaller target model (O-145): the summary is
/// at most `summary_max` tokens and the kept tail at most `tail_max` tokens, chosen so that
/// prefix + summary + tail stay within the target's soft limit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fit {
    pub summary_max: u32,
    pub keep_tokens: u32,
    pub tail_max: u32,
}

/// O-145: compact now with the SOURCE model (the session's current one), sized by `fit` for
/// the switch target, in the foreground and without idle prefill. `Ok(())` only when the wire
/// was rebased; a skipped (nothing to summarise), cancelled or failed compaction is an error
/// and leaves the wire unchanged.
pub(crate) fn compact_for_switch(
    st: &mut SessionState,
    fit: Fit,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let spec = Spec {
        instructions: None,
        reason: CompactReason::Manual,
        background: false,
        prefill_after: false,
        fit: Some(fit),
        keep_last_batch: false,
    };
    match run_compaction(st, &spec, sink, ui) {
        Outcome::Compacted => Ok(()),
        Outcome::Skipped => Err(OxaError::new(
            codes::ABORTED,
            "the compaction before the model switch was skipped or cancelled",
        )),
        Outcome::Failed(e) => Err(e),
    }
}

/// How a token count was obtained (OXA-EXACT-COUNTS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    /// `Engine::count_prompt`: the engine's own render, what a request would report.
    Exact,
    /// No exact count from the engine: [`prefix_tokens`] + [`messages_tokens`] (message content
    /// only, no template framing: slightly low).
    Estimate,
}

/// The prompt tokens of a request over the session's prefix and `wire`: exact when the engine
/// can count it (`Engine::count_prompt`, one extra request: callers are off the hot path of a
/// normal turn), else the estimate. The second value says which one it is.
pub(crate) fn prompt_tokens(st: &SessionState, wire: &[WireMsg]) -> (u32, Counted) {
    let hints = oxa_proto::Hints {
        session: &st.sref.id,
        idem_key: "",
        kind: TurnKind::Chat,
        background: false,
        snapshot_keep: false,
    };
    let req = oxa_proto::ChatRequest {
        prefix: &st.prefix,
        messages: wire,
        model: &st.model,
        max_tokens: st.info.max_output,
        thinking: st.thinking.enabled(),
        preserve_thinking: st.info.caps.preserve_thinking != Some(false),
        sampling: None,
        hints,
    };
    match st.engine.count_prompt(&req) {
        Ok(Some(n)) => (u32::try_from(n).unwrap_or(u32::MAX), Counted::Exact),
        _ => (
            prefix_tokens(st).saturating_add(messages_tokens(st, wire)),
            Counted::Estimate,
        ),
    }
}

/// Tokens of the frozen prefix (system text + tool list), through the engine's tokenizer.
pub(crate) fn prefix_tokens(st: &SessionState) -> u32 {
    let count = counter(st);
    count(&format!("{}{}", st.prefix.system_text(), st.prefix.tools_json))
}

/// Tokens of `msgs` (content, reasoning, tool calls), counted in one call.
pub(crate) fn messages_tokens(st: &SessionState, msgs: &[WireMsg]) -> u32 {
    if msgs.is_empty() {
        return 0;
    }
    let count = counter(st);
    let text: Vec<String> = msgs.iter().map(message_text).collect();
    count(&text.join("\n"))
}

/// The text whose tokens are counted for one message (as `oxa_compact` counts it).
fn message_text(m: &WireMsg) -> String {
    let mut s = m.content.clone().unwrap_or_default();
    if let Some(r) = &m.reasoning {
        s.push('\n');
        s.push_str(r);
    }
    for call in &m.tool_calls {
        s.push('\n');
        s.push_str(&call.name);
        s.push('\n');
        s.push_str(&call.args_raw);
    }
    s
}

/// S4 (CMP-015 / SET-026): an entry of `compaction.modelOverrides` for the session model that
/// would send the summary to ANOTHER model (`"<id>": "<other>"` or `{"model": "<other>"}`)
/// is ignored: one warning notice per session and epoch, and the summary runs on the session
/// model (one slot: a swap would unload the model and cold-prefill the whole wire).
fn warn_model_override(st: &SessionState, sink: &mut dyn EventSink) {
    let Some(other) = crate::session::override_summary_model(&st.settings, &st.model) else {
        return;
    };
    let key = (st.sref.id.clone(), st.prefix.sha256);
    let mut warned = st
        .shared
        .gate
        .override_warned
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if warned.as_ref() == Some(&key) {
        return;
    }
    *warned = Some(key);
    sink.emit(&AgentEvent::Notice {
        level: Level::Warning,
        text: format!(
            "compaction.modelOverrides: summaries on {other} are not supported (one model is loaded at a time); the summary runs on {}",
            st.model
        ),
    });
}

/// Prefill the current wire in idle time: one request with `max_tokens: 0` (H6),
/// `X-Oxe-Priority: background` (H3), `X-Oxe-Snapshot: keep` (H4), `TurnKind::Prefill`.
/// Skipped (silently, no request) when the engine does not advertise zero-token prefill
/// (`EngineInfo::prefill_only`, from `/props` `oxe_hints`), when hook `CacheWarmingDecision`
/// cancels or when `cacheWarming` is off. Errors are reported as a `Notice` and otherwise
/// ignored: without the hints oxa is correct and only slower. Never pushes anything to the wire.
pub fn idle_prefill(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    if !st.info.prefill_only || st.settings.string("cacheWarming").as_deref() == Some("off") {
        return;
    }
    let payload = json!({ "wireMessages": st.transcript.wire().len() });
    if matches!(
        fire_chain(st, HookPoint::CacheWarmingDecision, &payload, sink, ui),
        HookReply::Cancel(_)
    ) {
        return;
    }
    // ENG-002: user input that is already queued wins; input that arrives later cancels it.
    if !st.shared.begin_background() {
        return;
    }
    let opts = RequestOpts {
        kind: TurnKind::Prefill,
        max_tokens: Some(0),
        thinking: None,
        background: true,
        // F11: the hint only when the engine keeps checkpoints.
        snapshot_keep: st.info.caps.snapshot_keep > 0,
        extra: Vec::new(),
        events: false,
        idem_key: String::new(),
    };
    let (expected, first) = (crate::agent_loop::expected_cached(st), st.stats.requests == 0);
    match request(st, &opts, sink, ui) {
        Ok(turn) => {
            crate::agent_loop::note_cache(st, expected, &turn.usage, first, sink);
            // A prefill sets the expected cache of the next chat request (the fold), so its usage
            // is recorded like any other request over this wire.
            let usage = EntryBody::Usage {
                kind: TurnKind::Prefill.as_str().to_string(),
                usage: turn.usage,
            };
            if let Err(e) = persist(st, usage, sink) {
                sink.emit(&AgentEvent::Notice {
                    level: Level::Warning,
                    text: format!("cache warming not recorded: {}", e.message),
                });
            }
        }
        // Cancelled by the user's own input (ENG-002): silently skipped.
        Err(e) if e.code == codes::ABORTED => {}
        Err(e) => sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: format!("cache warming skipped: {}", e.message),
        }),
    }
}

/// V020-OPTIMIZE #13: prefill the user's draft while it is being composed. One zero-token
/// request (H6) over `wire + [draft]` (`RequestOpts::extra`: nothing is pushed or stored),
/// `X-Oxe-Priority: background` (H3), never `X-Oxe-Snapshot: keep` (H4: the draft is
/// speculative), `TurnKind::Prefill`, so that the prompt sent on Enter finds the draft in the
/// engine's cache.
/// The caller ([`crate::Session::prefill_draft`]) has claimed the session and built `draft` with
/// the prompt's own functions.
///
/// Like the boot preload, nothing is recorded: no usage entry, no cache expectation (a lost or
/// partial prefill is never a cache miss: the next chat request only reports more `cached`
/// than expected, which is silent), and the checkpoint the engine reports (past the wire) is not
/// kept as the session's last one. Hook `CacheWarmingDecision` may cancel it. User input
/// cancels it (ENG-002, `bg_cancel`); `stale` is asked once more after the background cancel is
/// armed, so an edit made before that moment is never missed. Returns whether it completed.
pub(crate) fn draft_prefill(
    st: &mut SessionState,
    draft: WireMsg,
    stale: &dyn Fn() -> bool,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> bool {
    let payload = json!({ "wireMessages": st.transcript.wire().len() });
    if matches!(
        fire_chain(st, HookPoint::CacheWarmingDecision, &payload, sink, ui),
        HookReply::Cancel(_)
    ) {
        return false;
    }
    if !st.shared.begin_background() || stale() {
        return false;
    }
    let opts = RequestOpts {
        kind: TurnKind::Prefill,
        max_tokens: Some(0),
        thinking: None,
        background: true,
        // A draft is speculative (it may be edited after the pause): its checkpoint must not
        // evict a kept one of the real conversation on an engine with few slots. No H4.
        snapshot_keep: false,
        extra: vec![draft],
        events: false,
        idem_key: String::new(),
    };
    let snap = st.stats.last_snap_pos;
    let result = request(st, &opts, sink, ui);
    st.stats.last_snap_pos = snap;
    match result {
        Ok(_) => true,
        Err(e) if e.code == codes::ABORTED => false,
        Err(e) => {
            sink.emit(&AgentEvent::Notice {
                level: Level::Warning,
                text: format!("draft prefill skipped: {}", e.message),
            });
            false
        }
    }
}

/// O-152: the setting that turns the boot preload off (`oxa.bootPreload: false`).
pub const SETTING_BOOT_PRELOAD: &str = "oxa.bootPreload";

/// O-152: the boot preload. While a new session waits for its first prompt, prefill its frozen
/// prefix (tools, preamble, identity, skills index, then the project's context and `cwd`) with
/// one zero-token request (H6), `X-Oxe-Priority: background` (H3), `X-Oxe-Snapshot: keep` when
/// advertised (H4), `TurnKind::Prefill`, so that
/// turn 1 finds the prefix in the engine's cache. Generic: no client-specific bytes, the same
/// request an idle prefill sends over an empty wire.
///
/// Sent only when the wire is empty, the engine advertises H6 (`EngineInfo::prefill_only`),
/// `cacheWarming` is not `off` and `oxa.bootPreload` is not `false`. User input cancels it
/// (ENG-002). Nothing is recorded: the first chat request still expects a cold start, so a
/// lost preload is never reported as a cache miss. Errors are one warning notice. Returns
/// whether a request completed.
pub fn boot_preload(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> bool {
    if !st.transcript.wire().is_empty()
        || !st.info.prefill_only
        || st.settings.string("cacheWarming").as_deref() == Some("off")
        || !st.settings.bool(SETTING_BOOT_PRELOAD, true)
    {
        return false;
    }
    if !st.shared.begin_background() {
        return false;
    }
    let opts = RequestOpts {
        kind: TurnKind::Prefill,
        max_tokens: Some(0),
        thinking: None,
        background: true,
        // H4 / ENG-006: like the idle prefill, the hint whenever the engine keeps checkpoints.
        snapshot_keep: st.info.caps.snapshot_keep > 0,
        extra: Vec::new(),
        events: false,
        idem_key: String::new(),
    };
    let (expected, first) = (crate::agent_loop::expected_cached(st), st.stats.requests == 0);
    match request(st, &opts, sink, ui) {
        Ok(turn) => {
            crate::agent_loop::note_cache(st, expected, &turn.usage, first, sink);
            // The first prompt then expects the preloaded prefix (in memory: the session file
            // holds no entry for a request over an empty wire).
            st.transcript.set_usage(&turn.usage);
            true
        }
        Err(e) if e.code == codes::ABORTED => false,
        Err(e) => {
            sink.emit(&AgentEvent::Notice {
                level: Level::Warning,
                text: format!("prefix preload skipped: {}", e.message),
            });
            false
        }
    }
}

/// E11: park the session's head before a child session takes the engine's one slot
/// (`HostAction::ParkSession`): one zero-token request over the current wire (H6) with
/// `X-Oxe-Snapshot: keep` (H4) in the foreground (the user waits on the tool call). The wire
/// may end with the assistant's tool-call message. Its usage is recorded like a prefill, so
/// the session's next request expects `cached` = this request's `prompt_tokens`. No request
/// when the engine does not advertise H6. -> `{parked, promptTokens, snapPos}`.
pub(crate) fn park(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Value, OxaError> {
    if !st.info.prefill_only {
        return Ok(json!({ "parked": false }));
    }
    let opts = RequestOpts {
        kind: TurnKind::Prefill,
        max_tokens: Some(0),
        thinking: None,
        background: false,
        snapshot_keep: st.info.caps.snapshot_keep > 0,
        extra: Vec::new(),
        events: false,
        idem_key: String::new(),
    };
    let (expected, first) = (crate::agent_loop::expected_cached(st), st.stats.requests == 0);
    let turn = request(st, &opts, sink, ui)?;
    crate::agent_loop::note_cache(st, expected, &turn.usage, first, sink);
    persist(
        st,
        EntryBody::Usage {
            kind: TurnKind::Prefill.as_str().to_string(),
            usage: turn.usage,
        },
        sink,
    )?;
    Ok(json!({
        "parked": true,
        "promptTokens": turn.usage.prompt,
        "snapPos": turn.timings.and_then(|t| t.snap_pos),
    }))
}

/// The whole compaction with its start and end events. Idle prefill follows a successful one.
fn run_compaction(st: &mut SessionState, spec: &Spec, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Outcome {
    let reason = spec.reason;
    // Overflow recovery keeps the current prompt; every other reason is a plain cut (W5).
    let when = match reason {
        CompactReason::Overflow => When::MidRun,
        CompactReason::Manual | CompactReason::Threshold => When::AfterReply,
    };
    let mut pending = if spec.instructions.is_none() && spec.fit.is_none() {
        usable_pending(st, when)
    } else {
        None
    };
    if spec.keep_last_batch {
        // A pending cut is made before this run's batches; checked anyway (never cut into it).
        let start = last_batch_start(st.transcript.wire());
        pending = pending.filter(|p| start.is_some_and(|b| p.cut <= b));
    }
    let cut = match &pending {
        Some(p) => p.cut,
        None => match cut_point(st, when, spec.fit.as_ref(), spec.keep_last_batch) {
            Some(cut) => cut,
            None => return Outcome::Skipped,
        },
    };
    warn_model_override(st, sink);
    sink.emit(&AgentEvent::CompactionStart { reason });
    let inner = compact_inner(st, spec, when, cut, pending.as_ref(), sink, ui);
    if matches!(inner, Ok(None)) {
        // Nothing changed (a hook or the user cancelled): the pending summary stays valid.
        st.compact_memo.pending = pending;
    }
    match inner {
        Ok(Some(a)) => {
            st.compact_memo = CompactMemo::default();
            let mut payload = json!({
                "reason": reason.as_str(),
                "summary": a.summary,
                "firstKeptEntryId": a.first_kept,
                "tokensBefore": a.tokens_before,
                "fromHook": a.from_hook,
                "fromExtension": a.from_hook,
                "willRetry": false,
            });
            // R4: the compaction happened, so `session_compact` fires (not
            // `session_compact_failed`, which means the wire is unchanged); the payload says the
            // model summary failed and why.
            if let Some(err) = &a.summary_error {
                payload["mechanical"] = json!(true);
                payload["summaryError"] = json!(err);
            }
            observe(st, HookPoint::SessionCompact, &payload, sink, ui);
            st.stats.compactions += 1;
            // Rebase clears transcript usage. Count the full request (including the
            // frozen system/tools prefix), not just the surviving message content.
            // The local estimate over-counts (about 2.5x on a small cloud session), so an
            // estimated count never exceeds what the context was before compaction.
            let (counted, how) = prompt_tokens(st, &a.head);
            let estimated = if how == Counted::Exact {
                counted
            } else {
                counted.min(a.tokens_before)
            };
            st.stats.context_tokens = estimated;
            sink.emit(&AgentEvent::CompactionEnd {
                reason,
                result: Some(CompactionResult {
                    summary: a.summary,
                    first_kept_entry_id: a.first_kept,
                    tokens_before: a.tokens_before,
                    estimated_tokens_after: estimated,
                    usage: a.usage,
                    details: a.details,
                }),
                aborted: false,
                will_retry: false,
                error_message: None,
            });
            // Overflow recovery is mid-run: the retried chat request follows at once and
            // fills the cache itself, so a prefill here would be a second pass over the head.
            // Before a model switch the head is prefilled by the target, not the source.
            if spec.prefill_after {
                idle_prefill(st, sink, ui);
            }
            Outcome::Compacted
        }
        Ok(None) => {
            sink.emit(&AgentEvent::CompactionEnd {
                reason,
                result: None,
                aborted: true,
                will_retry: false,
                error_message: None,
            });
            Outcome::Skipped
        }
        Err(e) => {
            sink.emit(&AgentEvent::CompactionEnd {
                reason,
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some(e.message.clone()),
            });
            observe(
                st,
                HookPoint::SessionCompactFailed,
                &json!({
                    "reason": reason.as_str(),
                    "error": e.message,
                    "errorMessage": e.message,
                    "aborted": false,
                    "willRetry": false,
                    "fromExtension": false,
                }),
                sink,
                ui,
            );
            Outcome::Failed(e)
        }
    }
}

/// Steps between the start and end events. `Ok(None)` = cancelled by a hook or the user; the
/// wire is unchanged in that case and in every `Err`.
fn compact_inner(
    st: &mut SessionState,
    spec: &Spec,
    _when: When,
    cut: usize,
    pending: Option<&Pending>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Option<Applied>, OxaError> {
    let (instructions, reason) = (spec.instructions, spec.reason);
    // OXA-REBASE-NONE-ORIGIN: a cut on a message without an entry origin (rewritten by a
    // declared extension rebase) keeps its tail by value (`details.keptMessages`).
    let wire_len = st.transcript.wire().len();
    let by_value = cut < wire_len && st.transcript.origin(cut).is_none();
    let first_kept = if by_value {
        String::new()
    } else {
        kept_origin(st, cut)?
    };
    // A1: oversized tool results of the kept tail are elided (the summary request below still
    // sends the whole wire).
    let keep_tokens = match spec.fit {
        Some(f) => f.keep_tokens.min(f.tail_max),
        None => st.compact.keep_tokens,
    };
    // L5 mid-run: the last tool batch is exempt (the model has not answered it yet).
    let elide_end = if spec.keep_last_batch {
        last_batch_start(st.transcript.wire()).map_or(wire_len, |b| b.max(cut).min(wire_len))
    } else {
        wire_len
    };
    let elided = {
        let count = counter(st);
        oxa_compact::tail_elisions(
            &st.transcript.wire()[cut.min(wire_len)..elide_end],
            keep_tokens,
            &count,
        )
    };
    let tokens_before = st.transcript.context();
    let payload = json!({
        "reason": reason.as_str(),
        "instructions": instructions,
        "customInstructions": instructions,
        "tokensBefore": tokens_before,
        "firstKeptEntryId": first_kept,
        "willRetry": false,
        "preparation": { "firstKeptEntryId": first_kept, "tokensBefore": tokens_before },
    });

    let mut summary_error = None;
    let (summary, details, from_hook, usage) = match fire_chain(
        st,
        HookPoint::SessionBeforeCompact,
        &payload,
        sink,
        ui,
    ) {
        HookReply::Cancel(_) => return Ok(None),
        // A hook summary lands as the head as given: no file lists are added (pi).
        HookReply::Summary { text, details } => (text, details, true, None),
        _ => {
            let (read, modified) = {
                let summarised = summarised(st, &first_kept);
                file_lists(&summarised)
            };
            let mut details = json!({ "readFiles": &read, "modifiedFiles": &modified });
            let lists = prompt::file_lists_block(&read, &modified);
            if let Some(p) = pending {
                // The idle summary: made for this cut over the same messages, no request.
                details["precomputed"] = json!(true);
                (format!("{}{lists}", p.text), details, false, Some(p.usage))
            } else {
                let summary_max = match spec.fit {
                    Some(f) => f.summary_max.min(st.compact.summary_max),
                    None => st.compact.summary_max,
                };
                let made = match summary_turn(st, spec, summary_max, sink, ui) {
                    Ok(Some(turn)) => {
                        persist(
                            st,
                            EntryBody::Usage {
                                kind: TurnKind::Compaction.as_str().to_string(),
                                usage: turn.usage,
                            },
                            sink,
                        )?;
                        summary_text(&turn).map(|text| (text, turn.usage))
                    }
                    Ok(None) if !cancelled_twice(st) => return Ok(None),
                    Ok(None) => Err(OxaError::new(
                        codes::ABORTED,
                        "the summary was cancelled twice in a row",
                    )),
                    Err(e) => Err(e),
                };
                match made {
                    Ok((text, usage)) => (format!("{text}{lists}"), details, false, Some(usage)),
                    // R4: under `local` a summary that could not be made is replaced by
                    // the mechanical one, so the session never blocks on it.
                    Err(e) if st.stats.profile == Profile::Local => {
                        // Self-feedback: say how much memory this cost (the assistant and
                        // tool turns are what the record leaves out) and that it is degraded.
                        let dropped = st.transcript.wire()[..cut]
                            .iter()
                            .filter(|m| matches!(m.role, Role::Assistant | Role::Tool))
                            .count();
                        sink.emit(&AgentEvent::Notice {
                            level: Level::Warning,
                            text: format!(
                                "context compacted without a model summary: {dropped} assistant/tool turns were dropped from memory ({}); an automatic record of the user's messages and files is used instead",
                                e.message
                            ),
                        });
                        details["droppedTurns"] = json!(dropped);
                        details["mechanical"] = json!(true);
                        summary_error = Some(e.message.clone());
                        let summary =
                            oxa_compact::mechanical_summary(&st.transcript.wire()[..cut], &read, &modified);
                        (summary, details, false, None)
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    };

    // The head is the fold of the entry below: the summary, then the wire from the first kept
    // message on (nothing when no tail is kept), oversized tool results elided.
    let mut head = vec![WireMsg::user(prompt::summary_user_message(&summary))];
    let mut tail: Vec<WireMsg> = if first_kept.is_empty() && !by_value {
        Vec::new()
    } else {
        st.transcript.wire()[cut..].to_vec()
    };
    let mut details = details;
    if by_value || !elided.is_empty() {
        if !details.is_object() {
            details = json!({ "hook": details });
        }
        if by_value {
            details["keptMessages"] = oxa_session::Transcript::encode_rebase(&tail);
        }
        if !elided.is_empty() {
            details["elided"] = elided
                .iter()
                .map(|(at, text)| json!({ "at": at, "text": text }))
                .collect();
        }
    }
    for (at, text) in &elided {
        if let Some(m) = tail.get_mut(*at) {
            m.content = Some(text.clone());
        }
    }
    head.extend(tail);
    let entry = EntryBody::Compaction {
        // The fold replays this text verbatim, so it is the wrapped user message itself.
        summary: prompt::summary_user_message(&summary),
        first_kept: first_kept.clone(),
        tokens_before,
        details: details.clone(),
        from_hook,
    };
    persist(st, entry, sink)?;
    debug_assert!(
        st.transcript.wire() == head.as_slice(),
        "the fold of the Compaction entry must equal head_after"
    );
    Ok(Some(Applied {
        summary,
        first_kept,
        tokens_before,
        details,
        from_hook,
        usage,
        head,
        summary_error,
    }))
}

/// The cut of the kept tail as an index into the wire, or `None` when nothing before it can be
/// summarised (the summary head itself is never summarised twice).
///
/// With `fit` (O-145) the kept tail is cut with the target's `keep_tokens` (at most
/// `tail_max`); when the tail found that way is still above `tail_max`, nothing is kept (the
/// summary alone).
///
/// `keep_last_batch` (L5 mid-run): the cut is never after the first message of the last tool
/// batch (an assistant message, a valid cut point), so the batch is kept whatever its size;
/// `None` when the wire has no tool batch.
fn cut_point(st: &SessionState, when: When, fit: Option<&Fit>, keep_last_batch: bool) -> Option<usize> {
    let count = counter(st);
    let cut = match fit {
        None => first_kept_index(&st.transcript, when, &st.compact, &count),
        Some(f) => {
            let policy = oxa_compact::CompactPolicy {
                keep_tokens: f.keep_tokens.min(f.tail_max),
                ..st.compact
            };
            let wire = st.transcript.wire();
            let cut = first_kept_index(&st.transcript, when, &policy, &count);
            if cut < wire.len() && messages_tokens(st, &wire[cut..]) > f.tail_max {
                wire.len()
            } else {
                cut
            }
        }
    };
    let cut = if keep_last_batch {
        cut.min(last_batch_start(st.transcript.wire())?)
    } else {
        cut
    };
    (cut > usize::from(summary_head(st))).then_some(cut)
}

/// The entry id of the first kept message. Empty when nothing is kept, which the fold reads as
/// "keep no tail".
fn kept_origin(st: &SessionState, cut: usize) -> Result<String, OxaError> {
    match st.transcript.origin(cut) {
        Some(id) => Ok(id.to_string()),
        None if cut >= st.transcript.wire().len() => Ok(String::new()),
        None => Err(OxaError::new(
            codes::INTERNAL,
            "compaction cut has no entry origin",
        )),
    }
}

/// The entries of the current branch that the compaction summarises: everything before the
/// first kept entry (all of them when nothing is kept).
fn summarised<'a>(st: &'a SessionState, first_kept: &str) -> Vec<&'a Entry> {
    let branch = st.tree.branch(st.tree.leaf());
    if first_kept.is_empty() {
        return branch;
    }
    branch.into_iter().take_while(|e| e.id != first_kept).collect()
}

/// R4 (profile `local`): count a summary cancelled by the user's input (not an abort) for the
/// current compaction need (the same [`MemoKey`]); `true` on the second one in a row.
fn cancelled_twice(st: &mut SessionState) -> bool {
    if st.stats.profile != Profile::Local || st.shared.cancel.is_cancelled() {
        return false;
    }
    let key = memo_key(st);
    let n = match &st.compact_memo.cancels {
        Some((k, n)) if *k == key => n + 1,
        _ => 1,
    };
    if n >= 2 {
        st.compact_memo.cancels = None;
        true
    } else {
        st.compact_memo.cancels = Some((key, n));
        false
    }
}

/// The options of a summary request: the whole wire plus the instruction (with the soft length
/// target of `oxa.compaction.summaryTarget`, V020-OPTIMIZE #7) as `extra`, thinking off.
fn summary_opts(
    st: &SessionState,
    instructions: Option<&str>,
    background: bool,
    summary_max: u32,
) -> RequestOpts {
    let wire_len = st.transcript.wire().len();
    let words = prompt::summary_target_words(st.settings.summary_target(st.stats.profile));
    let mut messages = summary_request_with_target(&st.transcript, instructions, words);
    let extra = messages.split_off(wire_len);
    RequestOpts {
        kind: TurnKind::Compaction,
        max_tokens: Some(summary_max),
        thinking: Some(false),
        background,
        snapshot_keep: false,
        extra,
        events: false,
        idem_key: String::new(),
    }
}

/// One summary request: the whole wire plus the instruction as `extra`. `Ok(None)` when aborted.
///
/// `background` (the threshold case only, F12): sent with `X-Oxe-Priority: background` and
/// cancelled by the user's own input (ENG-002); queued input skips it before it is sent.
fn summary_turn(
    st: &mut SessionState,
    spec: &Spec,
    summary_max: u32,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Option<AssistantTurn>, OxaError> {
    let (instructions, background) = (spec.instructions, spec.background);
    if background && !st.shared.begin_background() {
        return Ok(None);
    }
    let opts = summary_opts(st, instructions, background, summary_max);
    // pi's summarization retry (CMP-017): transient failures are retried with the
    // `summarization_retry_*` events.
    let source = crate::retry::SummarySource::Compaction(spec.reason);
    match crate::retry::summary_with_retry(st, &opts, source, sink, ui) {
        Ok(turn) => Ok(Some(turn)),
        Err(e) if e.code == codes::ABORTED => Ok(None),
        Err(e) => Err(e),
    }
}

/// V020-OPTIMIZE #7: the idle summary (see [`maybe_compact`]). Called at the end of a run
/// that needs no compaction. Silent: nothing is emitted when it is skipped, cancelled or fails.
/// A completed one records its usage (a `compaction` usage entry, which does not set the
/// expected cache: the next chat request still expects the last chat request's tokens, which
/// the summary request, an append to the same wire, leaves in the engine) and is followed by
/// the idle prefill of the unchanged wire, so the engine's state ends at the wire again.
fn idle_summary(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    if !st.auto_compaction || !st.settings.idle_summary(st.stats.profile) {
        return;
    }
    let (context, soft) = (st.transcript.context(), st.compact.soft);
    if context > soft || u64::from(context) * 5 <= u64::from(soft) * 4 {
        return;
    }
    if let Some(p) = &st.compact_memo.pending {
        if pending_valid(st, p) {
            return;
        }
    }
    st.compact_memo.pending = None;
    let key = memo_key(st);
    let len = st.transcript.wire().len();
    if st.compact_memo.idle_tried.as_ref() == Some(&(key.clone(), len)) {
        return;
    }
    let Some(cut) = cut_point(st, When::AfterReply, None, false) else {
        return;
    };
    // A cut without an entry origin keeps its tail by value: not worth precomputing.
    if cut < len && st.transcript.origin(cut).is_none() {
        return;
    }
    st.compact_memo.idle_tried = Some((key.clone(), len));
    if !st.shared.begin_background() {
        return;
    }
    let opts = summary_opts(st, None, true, st.compact.summary_max);
    let Ok(turn) = request(st, &opts, sink, ui) else {
        return;
    };
    let usage = EntryBody::Usage {
        kind: TurnKind::Compaction.as_str().to_string(),
        usage: turn.usage,
    };
    if persist(st, usage, sink).is_err() {
        return;
    }
    let Ok(text) = summary_text(&turn) else {
        return;
    };
    st.compact_memo.pending = Some(Pending {
        key,
        covered: len,
        digest: wire_digest(st, len),
        cut,
        text,
        usage: turn.usage,
    });
    idle_prefill(st, sink, ui);
}

/// The pending idle summary when a compaction (`when`) can use it: still valid, its cut has an
/// entry origin (or keeps nothing), a mid-run cut keeps the current prompt, and its kept tail
/// (from its cut to the end of the wire) is at most `2 * keep_tokens + soft / 5` tokens. Taken
/// out of the memo either way: an unusable one is dropped.
fn usable_pending(st: &mut SessionState, when: When) -> Option<Pending> {
    let p = st.compact_memo.pending.take()?;
    if !pending_valid(st, &p) {
        return None;
    }
    let wire = st.transcript.wire();
    if p.cut < wire.len() && st.transcript.origin(p.cut).is_none() {
        return None;
    }
    if when == When::MidRun {
        if let Some(last_user) = wire.iter().rposition(|m| m.role == Role::User) {
            if p.cut > last_user {
                return None;
            }
        }
    }
    let tail = messages_tokens(st, &wire[p.cut..]);
    let limit = st
        .compact
        .keep_tokens
        .saturating_mul(2)
        .saturating_add(st.compact.soft / 5);
    (tail <= limit).then_some(p)
}

/// The text of a summary reply. A cut-off or empty summary is a failure (pi).
fn summary_text(turn: &AssistantTurn) -> Result<String, OxaError> {
    if turn.finish == Finish::Length {
        return Err(OxaError::new(
            codes::INTERNAL,
            prompt::summary_hit_length("Compaction"),
        ));
    }
    let text = turn.msg.content.as_deref().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return Err(OxaError::new(
            codes::INTERNAL,
            prompt::summary_failed("Compaction", Some("empty summary")),
        ));
    }
    Ok(text)
}

/// `true` when the wire starts with a compaction summary message.
fn summary_head(st: &SessionState) -> bool {
    st.transcript.wire().first().is_some_and(|m| {
        m.role == Role::User
            && m.content.as_deref().is_some_and(|c| {
                c.starts_with(prompt::COMPACTION_SUMMARY_PREFIX)
                    && c.ends_with(prompt::COMPACTION_SUMMARY_SUFFIX)
            })
    })
}

/// Token counts through the engine's tokenizer; a failed count falls back to the byte length.
fn counter(st: &SessionState) -> impl Fn(&str) -> u32 {
    let engine = st.engine.clone();
    move |s: &str| {
        engine
            .count_tokens(s)
            .unwrap_or_else(|_| u32::try_from(s.len()).unwrap_or(u32::MAX))
    }
}

/// Fire a point through the hook sets in order: `Continue` moves on, any other reply ends the
/// chain and is returned.
fn fire_chain(
    st: &mut SessionState,
    point: HookPoint,
    payload: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> HookReply {
    crate::host::first_reply(st, point, payload, sink, ui)
}

/// Fire an observer point on every hook set; replies are ignored.
fn observe(
    st: &mut SessionState,
    point: HookPoint,
    payload: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    crate::host::observe_hooks(st, point, payload, sink, ui)
}
