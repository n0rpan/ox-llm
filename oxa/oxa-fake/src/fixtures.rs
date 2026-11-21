//! Shared fixture events: one set of named [`AgentEvent`] sequences for every front end.
//!
//! The terminal UI and the desktop app both reduce events with `oxa_interactive::apply_event`
//! and render blocks with `oxa_interactive::render_block`. Their tests feed the **same**
//! sequences from here, so a front end that forks that logic shows different text for the same
//! events and fails its test. `oxa-rpc` uses them to check that every event it writes can be
//! read back.
//!
//! Each sequence is a whole run as `oxa-core` reports it: the stored user entry, the turn, the
//! streamed reply (deltas, then the finished message), tool execution and the settle. A test
//! that wants a mid-run state (a streaming reply, a running tool) applies a prefix of
//! [`Fixture::events`]; every prefix is a state a front end must be able to draw.
//!
//! Everything is fixed: ids, timestamps, token counts. Nothing reads a clock, the environment
//! or a file.
//!
//! | Name | What it covers |
//! |---|---|
//! | `chat` | a prompt and a streamed reply with thinking |
//! | `tool_diff` | an `edit` call whose result carries a diff, then the closing reply |
//! | `streaming_bash` | a `bash` call with live partial output, exit code and duration |
//! | `compaction` | a threshold compaction between two turns |
//! | `retry` | a request that is retried once and then answers |
//! | `error` | a reply that fails after its first words (shown, never sent) |
//! | `worker_cards` | a parallel `subagent` call with two workers |
//! | `secret_ask` | a `sudo` command that asks the owner for a password while it runs |

use oxa_proto::{
    shown_why, AgentEvent, CompactReason, CompactionResult, Entry, EntryBody, EventMessage, Level,
    MessageDelta, StopReason, ToolCall, ToolOutput, Usage, Value, WireMsg,
};
use serde_json::json;

/// The model name every fixture reply carries.
pub const MODEL: &str = "fixture-model";

/// The timestamp (Unix milliseconds) of every fixture message and entry.
pub const TS_MS: u64 = 1_700_000_000_000;

/// One named event sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixture {
    /// The name in the table of the module documentation.
    pub name: &'static str,
    /// The events of the run, in the order a front end receives them.
    pub events: Vec<AgentEvent>,
    /// Short texts that a front end must show once all events are applied, whatever its look.
    /// Each one fits on a line at 80 columns and holds no markup.
    pub shows: &'static [&'static str],
    /// Set when the owner is asked for a secret during the run.
    pub ask: Option<SecretAsk>,
}

/// A request for a secret that opens while a fixture runs.
///
/// The request itself is not an [`AgentEvent`]: a front end gets it through `Ui::secret`. The
/// fixture says when it opens and what the front end has to show. The answer is never part of
/// a fixture, and no event after the request may hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAsk {
    /// The request opens after this many of the fixture's events: `events[..after]` is the
    /// state behind the open sheet.
    pub after: usize,
    /// The exact command that asks.
    pub command: &'static str,
    /// The prompt of the asking program.
    pub prompt: &'static str,
}

/// Every fixture, in the order of the table in the module documentation.
pub fn all() -> Vec<Fixture> {
    vec![
        chat(),
        tool_diff(),
        streaming_bash(),
        compaction(),
        retry(),
        error(),
        worker_cards(),
        secret_ask(),
    ]
}

/// The fixture called `name`, or `None`.
pub fn by_name(name: &str) -> Option<Fixture> {
    all().into_iter().find(|f| f.name == name)
}

/// A prompt and a streamed reply with thinking.
pub fn chat() -> Fixture {
    let mut run = Run::start("Where is the retry delay set?");
    run.turn_start();
    run.reply(
        "The user asks about the retry delay. It is a constant in the retry module.",
        "The retry delay is a constant in the retry module.\n\nIt is 250 ms today.",
        usage(1200, 31, 0),
    );
    run.settle();
    run.fixture(
        "chat",
        &[
            "Where is the retry delay set?",
            "The retry delay is a constant in the retry module.",
            "It is 250 ms today.",
        ],
    )
}

/// An `edit` call whose result carries a diff, then the closing reply.
pub fn tool_diff() -> Fixture {
    let mut run = Run::start("Double the retry delay.");
    run.turn_start();
    let call = run.call(
        "edit",
        json!({
            "path": "src/retry.rs",
            "edits": [{
                "oldText": "const DELAY_MS: u64 = 250;",
                "newText": "const DELAY_MS: u64 = 500;",
            }],
        }),
        usage(1300, 44, 1200),
    );
    let result = ToolOutput {
        text: "Successfully replaced 1 block(s) in src/retry.rs.".to_string(),
        is_error: false,
        ui: json!({
            "diff": " 11 /// Wait between two attempts.\n-12 const DELAY_MS: u64 = 250;\n+12 const DELAY_MS: u64 = 500;\n 13 \n",
            "firstChangedLine": 12,
        }),
        images: Vec::new(),
    };
    run.tool_start(&call);
    run.tool_end(&call, result, Some(12));
    run.turn_start();
    run.reply("", "The delay is 500 ms now.", usage(1400, 9, 1343));
    run.settle();
    run.fixture(
        "tool_diff",
        &[
            "Double the retry delay.",
            "src/retry.rs",
            "const DELAY_MS: u64 = 250;",
            "const DELAY_MS: u64 = 500;",
            "The delay is 500 ms now.",
        ],
    )
}

/// A `bash` call with live partial output, an exit code and a duration.
pub fn streaming_bash() -> Fixture {
    let mut run = Run::start("Run the retry tests.");
    run.turn_start();
    let call = run.call(
        "bash",
        json!({"command": "make test-retry"}),
        usage(1250, 18, 1200),
    );
    run.tool_start(&call);
    // `partial` is the complete output so far, not a delta.
    let mut partial = String::new();
    for line in [
        "running 3 tests\n",
        "test backoff_grows ... ok\n",
        "test gives_up_after_three ... ok\n",
        "test keeps_the_idempotency_key ... ok\n",
    ] {
        partial.push_str(line);
        run.tool_update(&call, &partial);
    }
    partial.push_str("\ntest result: ok. 3 passed; 0 failed\n");
    let result = ToolOutput {
        text: partial,
        is_error: false,
        ui: json!({"exit": 0, "timedOut": false}),
        images: Vec::new(),
    };
    run.tool_end(&call, result, Some(2400));
    run.turn_start();
    run.reply("", "All three retry tests pass.", usage(1350, 8, 1267));
    run.settle();
    run.fixture(
        "streaming_bash",
        &[
            "Run the retry tests.",
            "make test-retry",
            "test backoff_grows ... ok",
            "test result: ok. 3 passed; 0 failed",
            "All three retry tests pass.",
        ],
    )
}

/// A threshold compaction between two turns.
pub fn compaction() -> Fixture {
    let mut run = Run::start("Summarise what we changed.");
    run.push(AgentEvent::CompactionStart {
        reason: CompactReason::Threshold,
    });
    run.push(AgentEvent::CompactionEnd {
        reason: CompactReason::Threshold,
        result: Some(CompactionResult {
            summary: "## Goal\nMake the retry delay configurable.\n\n## Done\nThe delay moved to a constant."
                .to_string(),
            first_kept_entry_id: entry_id(0),
            tokens_before: 48_000,
            estimated_tokens_after: 9_000,
            usage: Some(usage(47_000, 600, 0)),
            details: json!({"readFiles": ["src/retry.rs"], "modifiedFiles": []}),
        }),
        aborted: false,
        will_retry: true,
        error_message: None,
    });
    run.turn_start();
    run.reply("", "We moved the retry delay to a constant.", usage(9_100, 10, 0));
    run.settle();
    run.fixture(
        "compaction",
        &[
            "Summarise what we changed.",
            "48000",
            "9000",
            "We moved the retry delay to a constant.",
        ],
    )
}

/// A request that is retried once and then answers.
pub fn retry() -> Fixture {
    let mut run = Run::start("Is the engine up?");
    run.turn_start();
    run.push(AgentEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 2_000,
        error_message: "the engine is busy (HTTP 503)".to_string(),
    });
    run.push(AgentEvent::AutoRetryEnd {
        success: true,
        attempt: 1,
        final_error: None,
    });
    run.reply("", "Yes, the engine answers again.", usage(1100, 8, 0));
    run.settle();
    run.fixture("retry", &["Is the engine up?", "Yes, the engine answers again."])
}

/// A reply that fails after its first words. The core stores what arrived as a `Shown` entry
/// (shown, never sent) and sends no `message_end` for it.
pub fn error() -> Fixture {
    let mut run = Run::start("Explain the backoff.");
    run.turn_start();
    run.push(AgentEvent::MessageStart {
        message: assistant(WireMsg::assistant(""), StopReason::Pending, None),
    });
    run.delta(MessageDelta::TextStart { index: 0 });
    for word in ["The ", "backoff ", "doubles"] {
        run.delta(MessageDelta::TextDelta {
            index: 0,
            delta: word.to_string(),
        });
    }
    run.entry(EntryBody::Shown {
        text: "The backoff doubles".to_string(),
        why: shown_why::FAILED,
    });
    run.push(AgentEvent::Notice {
        level: Level::Error,
        text: "The engine closed the stream (connection reset).".to_string(),
    });
    run.push(AgentEvent::AgentEnd {
        messages: Vec::new(),
        will_retry: false,
    });
    run.push(AgentEvent::AgentSettled { aborted: false });
    run.fixture(
        "error",
        &[
            "Explain the backoff.",
            "The backoff doubles",
            "The engine closed the stream (connection reset).",
        ],
    )
}

/// A parallel `subagent` call with two workers. `ToolOutput::ui` holds one result per worker,
/// as `oxa-core` writes it; a front end draws its worker cards from the call and that result.
pub fn worker_cards() -> Fixture {
    let mut run = Run::start("Check the retry change from two sides.");
    run.turn_start();
    let call = run.call(
        "subagent",
        json!({
            "tasks": [
                {"agent": "scout", "task": "Find every caller of the retry delay."},
                {"agent": "reviewer", "task": "Review the retry change for races."},
            ],
        }),
        usage(1500, 52, 1200),
    );
    run.tool_start(&call);
    run.tool_update(&call, "scout: running\nreviewer: running");
    run.tool_update(&call, "scout: completed\nreviewer: running");
    let result = ToolOutput {
        text: "Parallel: 2/2 succeeded\n\nscout completed: two callers, both in the engine client.\n\nreviewer completed: no race found."
            .to_string(),
        is_error: false,
        ui: json!({
            "mode": "parallel",
            "results": [
                {"agent": "scout", "status": "completed", "exitCode": 0, "sessionHint": "scout-1"},
                {"agent": "reviewer", "status": "completed", "exitCode": 0, "sessionHint": "reviewer-2"},
            ],
            "isError": false,
        }),
        images: Vec::new(),
    };
    run.tool_end(&call, result, Some(41_000));
    run.turn_start();
    run.reply(
        "",
        "Both workers are done; nothing to fix.",
        usage(1700, 11, 1552),
    );
    run.settle();
    run.fixture(
        "worker_cards",
        &[
            "Check the retry change from two sides.",
            "subagent",
            "scout completed: two callers, both in the engine client.",
            "reviewer completed: no race found.",
            "Both workers are done; nothing to fix.",
        ],
    )
}

/// A `sudo` command that asks the owner for a password while it runs ([`Fixture::ask`]).
pub fn secret_ask() -> Fixture {
    const COMMAND: &str = "sudo -A install -m 644 retry.conf /etc/oxa/retry.conf";
    let mut run = Run::start("Install the retry config.");
    run.turn_start();
    let call = run.call("bash", json!({"command": COMMAND}), usage(1280, 27, 1200));
    run.tool_start(&call);
    let ask = SecretAsk {
        after: run.events.len(),
        command: COMMAND,
        prompt: "[sudo] password for owner:",
    };
    let result = ToolOutput {
        text: "installed /etc/oxa/retry.conf\n".to_string(),
        is_error: false,
        ui: json!({"exit": 0, "timedOut": false}),
        images: Vec::new(),
    };
    run.tool_end(&call, result, Some(6_300));
    run.turn_start();
    run.reply("", "The retry config is installed.", usage(1380, 8, 1307));
    run.settle();
    let mut fixture = run.fixture(
        "secret_ask",
        &[
            "Install the retry config.",
            "sudo -A install -m 644 retry.conf /etc/oxa/retry.conf",
            "installed /etc/oxa/retry.conf",
            "The retry config is installed.",
        ],
    );
    fixture.ask = Some(ask);
    fixture
}

// ---------------------------------------------------------------------------------------------
// Builder

/// A tool call of a fixture reply.
struct Call {
    id: String,
    name: &'static str,
    args: Value,
}

/// Builds one run in the order `oxa-core` reports it.
struct Run {
    events: Vec<AgentEvent>,
    /// Number of entries stored so far (the next entry id).
    entries: u32,
    /// Number of tool calls so far (the next call id).
    calls: u32,
    /// The assistant message and the tool results of the open turn, for `turn_end`.
    turn: Option<(EventMessage, Vec<EventMessage>)>,
}

impl Run {
    /// A run that starts with the owner's prompt: the stored entry, then `agent_start`.
    fn start(prompt: &str) -> Run {
        let mut run = Run {
            events: Vec::new(),
            entries: 0,
            calls: 0,
            turn: None,
        };
        run.entry(EntryBody::Msg {
            wire: WireMsg::user(prompt),
            parts: Vec::new(),
            tool: None,
        });
        run.push(AgentEvent::AgentStart);
        // pi's `message_start` for the prompt: the second report of the same message.
        run.push(AgentEvent::MessageStart {
            message: message(WireMsg::user(prompt)),
        });
        run
    }

    fn push(&mut self, event: AgentEvent) {
        self.events.push(event);
    }

    fn delta(&mut self, delta: MessageDelta) {
        self.push(AgentEvent::MessageUpdate {
            usage: Usage::default(),
            delta,
        });
    }

    /// Store an entry; each one is the child of the one before.
    fn entry(&mut self, body: EntryBody) {
        let parent = self.entries.checked_sub(1).map(entry_id);
        let entry = Entry {
            id: entry_id(self.entries),
            parent,
            ts_ms: TS_MS,
            body,
        };
        self.entries += 1;
        self.push(AgentEvent::EntryAppended { entry });
    }

    /// Close the turn before (if one is open) and start the next one.
    fn turn_start(&mut self) {
        self.turn_end();
        self.push(AgentEvent::TurnStart);
    }

    fn turn_end(&mut self) {
        if let Some((message, tool_results)) = self.turn.take() {
            self.push(AgentEvent::TurnEnd {
                message,
                tool_results,
            });
        }
    }

    /// A streamed text reply: optional thinking, then the text word by word, then the message.
    fn reply(&mut self, thinking: &str, text: &str, usage: Usage) {
        self.push(AgentEvent::MessageStart {
            message: assistant(WireMsg::assistant(""), StopReason::Pending, None),
        });
        let mut index = 0;
        if !thinking.is_empty() {
            self.delta(MessageDelta::ThinkingStart { index });
            for piece in thinking.split_inclusive(' ') {
                self.delta(MessageDelta::ThinkingDelta {
                    index,
                    delta: piece.to_string(),
                });
            }
            self.delta(MessageDelta::ThinkingEnd {
                index,
                content: thinking.to_string(),
            });
            index += 1;
        }
        self.delta(MessageDelta::TextStart { index });
        for piece in text.split_inclusive(' ') {
            self.delta(MessageDelta::TextDelta {
                index,
                delta: piece.to_string(),
            });
        }
        self.delta(MessageDelta::TextEnd {
            index,
            content: text.to_string(),
        });
        let mut wire = WireMsg::assistant(text);
        if !thinking.is_empty() {
            wire.reasoning = Some(thinking.to_string());
        }
        let done = assistant(wire, StopReason::Stop, Some(usage));
        self.push(AgentEvent::MessageEnd {
            message: done.clone(),
        });
        self.turn = Some((done, Vec::new()));
    }

    /// A reply that is one tool call: the call streams in, then the message ends with it.
    fn call(&mut self, name: &'static str, args: Value, usage: Usage) -> Call {
        self.calls += 1;
        let call = Call {
            id: format!("call_{}", self.calls),
            name,
            args,
        };
        let args_raw = call.args.to_string();
        self.push(AgentEvent::MessageStart {
            message: assistant(WireMsg::assistant(""), StopReason::Pending, None),
        });
        self.delta(MessageDelta::ToolcallStart {
            index: 0,
            id: call.id.clone(),
            name: name.to_string(),
        });
        // Two pieces, so a front end sees arguments that do not parse yet.
        let cut = (0..=args_raw.len() / 2)
            .rev()
            .find(|i| args_raw.is_char_boundary(*i))
            .unwrap_or(0);
        for piece in [&args_raw[..cut], &args_raw[cut..]] {
            self.delta(MessageDelta::ToolcallDelta {
                index: 0,
                delta: piece.to_string(),
            });
        }
        let tool_call = ToolCall {
            id: call.id.clone(),
            name: name.to_string(),
            args_raw,
        };
        self.delta(MessageDelta::ToolcallEnd {
            index: 0,
            call: tool_call.clone(),
        });
        let mut wire = WireMsg::assistant("");
        wire.tool_calls = vec![tool_call];
        let done = assistant(wire, StopReason::ToolUse, Some(usage));
        self.push(AgentEvent::MessageEnd {
            message: done.clone(),
        });
        self.turn = Some((done, Vec::new()));
        call
    }

    fn tool_start(&mut self, call: &Call) {
        self.push(AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.to_string(),
            args: call.args.clone(),
        });
    }

    fn tool_update(&mut self, call: &Call, partial: &str) {
        self.push(AgentEvent::ToolExecutionUpdate {
            tool_call_id: call.id.clone(),
            tool_name: call.name.to_string(),
            args: call.args.clone(),
            partial: partial.to_string(),
        });
    }

    fn tool_end(&mut self, call: &Call, result: ToolOutput, duration_ms: Option<u64>) {
        let mut answer = message(WireMsg::tool_result(call.id.clone(), result.text.clone()));
        answer.tool_name = Some(call.name.to_string());
        answer.is_error = result.is_error;
        if let Some((_, tool_results)) = self.turn.as_mut() {
            tool_results.push(answer);
        }
        self.push(AgentEvent::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.to_string(),
            result,
            duration_ms,
        });
    }

    /// End the run: the open turn, `agent_end` with the last reply, then the settle.
    fn settle(&mut self) {
        let last = self.turn.as_ref().map(|(message, _)| message.clone());
        self.turn_end();
        self.push(AgentEvent::AgentEnd {
            messages: last.into_iter().collect(),
            will_retry: false,
        });
        self.push(AgentEvent::AgentSettled { aborted: false });
    }

    fn fixture(self, name: &'static str, shows: &'static [&'static str]) -> Fixture {
        Fixture {
            name,
            events: self.events,
            shows,
            ask: None,
        }
    }
}

/// The id of the `n`th stored entry: 8 lowercase hex characters, as in a session file.
fn entry_id(n: u32) -> String {
    format!("{:08x}", n + 1)
}

fn usage(prompt: u32, completion: u32, cached: u32) -> Usage {
    Usage {
        prompt,
        completion,
        cached,
        ..Usage::default()
    }
}

/// A user or tool message as events carry it.
fn message(msg: WireMsg) -> EventMessage {
    EventMessage {
        msg,
        ts_ms: TS_MS,
        stop: None,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

/// An assistant message as events carry it.
fn assistant(msg: WireMsg, stop: StopReason, usage: Option<Usage>) -> EventMessage {
    EventMessage {
        stop: Some(stop),
        usage,
        model: Some(MODEL.to_string()),
        ..message(msg)
    }
}
