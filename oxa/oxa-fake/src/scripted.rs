//! Architect-owned (W0-FROZEN, implemented): the in-process scripted engine.
use crate::{cache_bytes, Fault, Recorded, ScriptTurn};
use oxa_proto::{
    codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent, ThinkingLevel,
    Timings, Usage, WireMsg,
};
use std::collections::VecDeque;
use std::sync::Mutex;

struct Inner {
    turns: VecDeque<ScriptTurn>,
    seen: Vec<Recorded>,
    /// Per session: the bytes the engine holds (last prompt + reply minus its last token).
    cache: Vec<(String, Vec<u8>)>,
    /// EG5: every `clear_hint` call, in order.
    clears: Vec<String>,
}

/// An `Engine` that replays [`ScriptTurn`]s and records every request. See the crate docs for
/// the cache model. When the script is exhausted a chat request fails with `internal`
/// ("script exhausted"), which makes an unexpected extra request visible in tests.
pub struct ScriptedEngine {
    inner: Mutex<Inner>,
    pub n_ctx: u32,
    pub max_output: u32,
    pub model: String,
    /// B6: `capabilities.preserve_thinking` as `info` reports it (`None` = not published).
    pub preserve_thinking: Option<bool>,
    /// B6: the chat template drops the reasoning of the assistant turns at or before the last
    /// user message (a Qwen template with `preserve_thinking: false`): the cache model renders
    /// the history that way ([`crate::render_history`]), so the next user turn re-prefills
    /// from the first assistant turn after the previous user message.
    pub drop_reasoning: bool,
    /// B6: the usage leaves out `prompt_tokens_details.cached_tokens` (a server that does not
    /// report it): `Usage::cached_missing`.
    pub omit_cached: bool,
    /// L4: the compaction point the engine publishes (`EngineInfo::compact_at`); `None` = none.
    pub compact_at: Option<u32>,
}

impl ScriptedEngine {
    pub fn new(turns: Vec<ScriptTurn>) -> ScriptedEngine {
        ScriptedEngine {
            inner: Mutex::new(Inner {
                turns: turns.into(),
                seen: Vec::new(),
                cache: Vec::new(),
                clears: Vec::new(),
            }),
            n_ctx: 1 << 20,
            max_output: 4096,
            model: "fake-model".to_string(),
            preserve_thinking: None,
            drop_reasoning: false,
            omit_cached: false,
            compact_at: None,
        }
    }
    /// L4: the engine publishes `compact_at` as its compaction point.
    pub fn with_compact_at(mut self, compact_at: u32) -> ScriptedEngine {
        self.compact_at = Some(compact_at);
        self
    }
    /// B6: a model whose capabilities say `preserve_thinking` (`Some(false)` = Flash) and whose
    /// template drops the reasoning of earlier turns (`drop_reasoning`).
    pub fn with_thinking_template(
        mut self,
        preserve_thinking: Option<bool>,
        drop_reasoning: bool,
    ) -> ScriptedEngine {
        self.preserve_thinking = preserve_thinking;
        self.drop_reasoning = drop_reasoning;
        self
    }
    /// B6: the usage never reports `cached_tokens`.
    pub fn without_cached_tokens(mut self) -> ScriptedEngine {
        self.omit_cached = true;
        self
    }
    pub fn with_limits(mut self, n_ctx: u32, max_output: u32) -> ScriptedEngine {
        self.n_ctx = n_ctx;
        self.max_output = max_output;
        self
    }
    /// Add turns to the end of the script.
    pub fn push(&self, turn: ScriptTurn) {
        self.inner.lock().unwrap().turns.push_back(turn);
    }
    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.lock().unwrap().seen.clone()
    }
    /// Turns not yet consumed.
    pub fn remaining(&self) -> usize {
        self.inner.lock().unwrap().turns.len()
    }
    /// Forget all cached state (an engine restart).
    pub fn restart(&self) {
        self.inner.lock().unwrap().cache.clear();
    }
    /// EG5: the hint of every `clear_hint` call, in order.
    pub fn clears(&self) -> Vec<String> {
        self.inner.lock().unwrap().clears.clone()
    }
    /// The cache-model bytes held for `session` (`None` = nothing held).
    pub fn held(&self, session: &str) -> Option<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        g.cache.iter().find(|(s, _)| s == session).map(|(_, h)| h.clone())
    }
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

impl Engine for ScriptedEngine {
    fn info(&self, _target: Option<&str>) -> Result<EngineInfo, OxaError> {
        Ok(EngineInfo {
            base_url: "scripted:".to_string(),
            models: vec![self.model.clone()],
            model: self.model.clone(),
            limits_for: self.model.clone(),
            n_ctx: self.n_ctx,
            max_output: self.max_output,
            queue: 0,
            compact_at: self.compact_at,
            prefill_only: true,
            caps: oxa_proto::EngineCaps {
                snapshot_keep: 2,
                hint_clear: true,
                preserve_thinking: self.preserve_thinking,
                park_disk_only: false,
            },
        })
    }

    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        Ok(text.len() as u32)
    }

    /// The fake's template: [`cache_bytes`], one token per byte (what a request reports).
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        Ok(Some(
            cache_bytes(
                &r.prefix.system_text(),
                &r.prefix.tools_json,
                &crate::render_history(r.messages, self.drop_reasoning),
            )
            .len() as u64,
        ))
    }

    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let mut g = self.inner.lock().unwrap();
        let system = r.prefix.system_text();
        let rendered = crate::render_history(r.messages, self.drop_reasoning);
        let prompt_bytes = cache_bytes(&system, &r.prefix.tools_json, &rendered);
        let plen = prompt_bytes.len();
        let session = r.hints.session.to_string();
        // `Fault::Cold`: the engine holds nothing for the session when the request arrives.
        let cold = r.max_tokens > 0 && matches!(g.turns.front(), Some(t) if t.fault == Some(Fault::Cold));
        if cold {
            g.cache.retain(|(s, _)| *s != session);
        }
        let cached = crate::turn::cached_of(&g.cache, &session, &prompt_bytes, cold);
        let seq = g.seen.len();
        let mut rec = Recorded {
            seq,
            system,
            tools_json: r.prefix.tools_json.clone(),
            messages: r.messages.to_vec(),
            model: r.model.to_string(),
            max_tokens: r.max_tokens,
            thinking: r.thinking,
            session: session.clone(),
            idem_key: r.hints.idem_key.to_string(),
            kind: r.hints.kind.as_str().to_string(),
            background: r.hints.background,
            snapshot_keep: r.hints.snapshot_keep,
            preserve_thinking: r.preserve_thinking.then_some(true),
            headers: Vec::new(),
            body: Vec::new(),
            reply: None,
            usage: None,
        };
        let level = if r.thinking {
            ThinkingLevel::Medium
        } else {
            ThinkingLevel::Off
        };
        // H4: `snap_pos` only with the hint (the end of the last message = the prompt end in
        // the fake's model); `Fault::SnapNull` makes it null.
        let snap_null = matches!(g.turns.front(), Some(t) if matches!(t.fault, Some(Fault::SnapNull { .. })))
            && r.max_tokens > 0;
        let timings = |predicted: u32| Timings {
            cache_n: cached as u32,
            prompt_n: (plen - cached) as u32,
            prompt_ms: 0.0,
            predicted_n: predicted,
            predicted_ms: 0.0,
            snap_pos: if r.hints.snapshot_keep && !snap_null {
                Some(plen as u32)
            } else {
                None
            },
            snap_asked: r.hints.snapshot_keep,
        };
        if cancel.is_cancelled() {
            g.seen.push(rec);
            return Err(OxaError::new(codes::ABORTED, "aborted"));
        }
        // F9: a trailing assistant message is history; the reply is a new turn (no refusal: a
        // `ChatRequest` cannot ask for an in-place continuation).
        // F6: only a prompt that does not fit at all is refused.
        if plen as u64 >= self.n_ctx as u64 {
            g.seen.push(rec);
            return Err(OxaError::new(
                codes::CONTEXT_OVERFLOW,
                "exceed_context_size_error",
            ));
        }

        // H6: prefill only (F7: `finish_reason: "length"`, nothing generated or held beyond the
        // prompt).
        if r.max_tokens == 0 {
            on(StreamEvent::Prefill {
                done: plen as u32,
                total: plen as u32,
                secs: 0,
            });
            let usage = Usage {
                prompt: plen as u32,
                completion: 0,
                cached: if self.omit_cached { 0 } else { cached as u32 },
                cached_missing: self.omit_cached,
            };
            rec.usage = Some(usage);
            g.seen.push(rec);
            g.cache.retain(|(s, _)| *s != session);
            g.cache.push((session, prompt_bytes.clone()));
            return Ok(AssistantTurn {
                msg: WireMsg {
                    role: oxa_proto::Role::Assistant,
                    content: None,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                },
                finish: oxa_proto::Finish::Length,
                usage,
                timings: Some(timings(0)),
                model: r.model.to_string(),
                level,
            });
        }

        let turn = match g.turns.pop_front() {
            Some(t) => t,
            None => {
                g.seen.push(rec);
                return Err(OxaError::new(
                    codes::INTERNAL,
                    "script exhausted: unexpected extra request",
                ));
            }
        };
        let mut drop_at = None;
        match &turn.fault {
            Some(Fault::Overflow) => {
                g.seen.push(rec);
                return Err(OxaError::new(
                    codes::CONTEXT_OVERFLOW,
                    "exceed_context_size_error",
                ));
            }
            Some(Fault::Http { status, body }) => {
                g.seen.push(rec);
                return Err(status_error(*status, body, None));
            }
            Some(Fault::Busy { retry_after }) => {
                g.seen.push(rec);
                return Err(status_error(
                    503,
                    "Too many requests are waiting for the local model.",
                    *retry_after,
                ));
            }
            Some(Fault::StreamError {
                queued,
                status,
                error_type,
                ..
            }) => {
                g.seen.push(rec);
                for i in 0..*queued {
                    on(StreamEvent::Queued {
                        pos: Some(queued - i),
                        wait_s: i,
                    });
                }
                if error_type == "exceed_context_size_error" {
                    return Err(OxaError::new(
                        codes::CONTEXT_OVERFLOW,
                        "exceed_context_size_error",
                    ));
                }
                return Err(status_error(
                    *status,
                    &format!("the engine failed with HTTP {status}"),
                    None,
                ));
            }
            Some(Fault::Queued { count, pos }) => {
                for i in 0..*count {
                    on(StreamEvent::Queued {
                        pos: pos.then_some(count - i),
                        wait_s: i,
                    });
                }
            }
            Some(Fault::Refuse) => {
                g.seen.push(rec);
                return Err(
                    OxaError::new(codes::ENGINE_DOWN, "connection refused").with_hint("start the engine")
                );
            }
            Some(Fault::DropAt { bytes }) => drop_at = Some(*bytes),
            _ => {}
        }

        // F6: bounded by `max_tokens` and by the room left in the context.
        let room = self.n_ctx - plen as u32;
        let (msg, finish, completion) = crate::server::generate(&turn, r.max_tokens.min(room));

        let mut sent = 0usize;
        let mut emit = |ev: StreamEvent, n: usize, sent: &mut usize| -> bool {
            if let Some(limit) = drop_at {
                if *sent + n > limit {
                    return false;
                }
            }
            *sent += n;
            on(ev);
            true
        };
        let mut ok = true;
        if let Some(reasoning) = &msg.reasoning {
            for c in split_chunks(reasoning, turn.chunk) {
                ok = ok && emit(StreamEvent::Reasoning(c.to_string()), c.len(), &mut sent);
            }
        }
        if let Some(text) = &msg.content {
            for c in split_chunks(text, turn.chunk) {
                ok = ok && emit(StreamEvent::Text(c.to_string()), c.len(), &mut sent);
            }
        }
        for (index, call) in msg.tool_calls.iter().enumerate() {
            let parts = split_chunks(&call.args_raw, turn.chunk);
            for (i, c) in parts.iter().enumerate() {
                let first = i == 0;
                ok = ok
                    && emit(
                        StreamEvent::ToolArgs {
                            index,
                            id: if first { Some(call.id.clone()) } else { None },
                            name: if first { Some(call.name.clone()) } else { None },
                            delta: c.to_string(),
                        },
                        c.len(),
                        &mut sent,
                    );
            }
        }
        if !ok || drop_at.is_some() {
            g.seen.push(rec);
            return Err(OxaError::new(codes::STREAM_BROKEN, "stream closed before [DONE]"));
        }
        if cancel.is_cancelled() {
            g.seen.push(rec);
            return Err(OxaError::new(codes::ABORTED, "aborted"));
        }

        let usage = Usage {
            prompt: plen as u32,
            completion,
            cached: if self.omit_cached { 0 } else { cached as u32 },
            cached_missing: self.omit_cached,
        };
        rec.reply = Some(msg.clone());
        rec.usage = Some(usage);
        g.seen.push(rec);
        // F1: the last generated token is never fed.
        let held = crate::server::held_after(&prompt_bytes, &msg);
        g.cache.retain(|(s, _)| *s != session);
        g.cache.push((session, held));
        Ok(AssistantTurn {
            msg,
            finish,
            usage,
            timings: Some(timings(completion)),
            model: r.model.to_string(),
            level,
        })
    }

    /// EG5: drops what the engine holds for session `hint` alone and records the call
    /// ([`ScriptedEngine::clears`]).
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        let mut g = self.inner.lock().unwrap();
        g.cache.retain(|(s, _)| s != hint);
        g.clears.push(hint.to_string());
        Ok(())
    }
}

/// A status reply as `oxa_engine::HttpEngine` maps it: 503 -> `engine_busy` (with `retry after
/// <n> s`), other statuses -> `engine_http` naming `HTTP <status>`.
fn status_error(status: u16, detail: &str, retry_after: Option<u32>) -> OxaError {
    let msg = format!("the engine answered HTTP {status}: {detail}");
    if status == 503 {
        let msg = match retry_after {
            Some(n) => format!("{msg} (retry after {n} s)"),
            None => msg,
        };
        return OxaError::new(codes::ENGINE_BUSY, msg);
    }
    OxaError::new(codes::ENGINE_HTTP, msg)
}
