//! T5 (OXA-LOCAL-FIRST): `enable_thinking` by turn kind, `oxa.thinking.policy`.
//!
//! * `fixed`: every request carries the session level (`off` = false, any other level = true).
//! * `auto` (the default): thinking on for the first reply after a user
//!   prompt; off for tool follow-ups, turns after an appended non-user instruction (an extension
//!   message, a branch summary, a repair message), compaction and branch summaries, prefills and
//!   every other background request. The level `off` always wins: the policy never turns
//!   thinking on. An explicit `RequestOpts::thinking` still wins over both (summaries send
//!   `Some(false)`). W6 holds: every request carries the explicit boolean.
//!
//! Cache safety (W1/W2) was checked against the engine's chat templates: on Ornith and
//! Qwen3.6 `enable_thinking` changes only the generation opener after the history
//! (`<think>\n` vs `<think>\n\n</think>\n\n`), and the thinking-off opener is byte-equal to how
//! an assistant turn without reasoning renders in history, so a toggle changes no byte already
//! sent. The Qwen3.8-Flash-Next template (`qwen38-flash-next.jinja`) also puts its
//! reasoning-effort sentence at the START of the system message when thinking is on, so a
//! toggle there changes the first bytes of the prompt and re-prefills everything. `auto`
//! therefore acts as `fixed` on an engine that does not keep reasoning in history
//! (`EngineCaps::preserve_thinking == Some(false)`, the Flash engine), until the engine
//! advertises a toggle-safe template itself.
//!
//!
//! ## Tool follow-ups under `auto`: adaptive escalation
//!
//! `oxa.thinking.toolFollowups` (`auto` default, `on`, `off`) decides the replies after tool
//! results. Evidence (Ornith, a long-session bug task, greedy): with thinking off on every
//! follow-up the model looped (151 identical reads, later 45-77 identical commands) and did not
//! solve the task in 900 s; with thinking on every follow-up it solved it in 392 s, but a pure
//! large-read task then hit its time cap. `auto` therefore keeps routine steps off and turns
//! thinking on while the run needs reasoning ([`Escalation`]):
//!
//! * **On** after a tool batch with a failed call (`is_error`: a non-zero exit, an edit that did
//!   not apply, any tool error), a repeat notice (`repeat.rs`), or
//!   `oxa.thinking.escalation.fruitlessCalls` calls without progress since the prompt (a
//!   successful edit or write resets the count; a `read`/`grep`/`find`/`ls` with arguments not
//!   seen since the prompt is progress and not counted). When the prompt asks to fix, debug or
//!   implement something ([`task_prompt`]), a test or build command whose output reports a
//!   failure is a failed call even when it exits 0 (`cargo test 2>&1 | tail`).
//! * **Hysteresis.** Under a fix/debug/implement prompt it stays on until an edit or write
//!   succeeds or a test or build command passes (`executed_check`: only a runner in program
//!   position counts, never a mention in a search or read; a test run also needs its pass
//!   summary). Under any other prompt it also goes back off
//!   after a batch whose calls all succeeded and were all new, so a read-heavy task stays fast.
//! * A new user prompt starts from off; the run's end clears it. The state lives in memory
//!   (`Stats::escalation`), derives only from the run's tool results and is deterministic.
//! * Visible: a footer status `thinking: auto→on` while on, and an info notice with the reason
//!   each time it turns on.
//!
//! Cache: the escalation only chooses the per-request `enable_thinking` boolean; it never
//! rewrites a sent message (W1). The toggle is cache-safe for the same reason as above, and
//! `auto` acts as `fixed` where it is not.
use crate::exec::CallResult;
use crate::SessionState;
use oxa_config::ThinkingPolicy;
use oxa_proto::{AgentEvent, EntryBody, EventSink, Level, Role, ToolCall, TurnKind, Ui, Value};
use oxa_session::Tree;

/// `oxa.thinking.toolFollowups`.
pub const TOOL_FOLLOWUPS: &str = "oxa.thinking.toolFollowups";
/// `oxa.thinking.escalation.fruitlessCalls`.
pub const FRUITLESS_CALLS: &str = "oxa.thinking.escalation.fruitlessCalls";
/// The default of [`FRUITLESS_CALLS`].
pub const FRUITLESS_DEFAULT: u32 = 6;
/// The footer status key and text while an escalation is on.
pub const STATUS_KEY: &str = "thinking";
pub const STATUS_ON: &str = "thinking: auto→on";

/// `oxa.thinking.toolFollowups`: thinking on the replies after tool results under `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFollowups {
    /// Off for routine steps, on while the run needs reasoning (module docs).
    Auto,
    On,
    Off,
}

impl ToolFollowups {
    pub fn parse(s: &str) -> Option<ToolFollowups> {
        match s {
            "auto" => Some(ToolFollowups::Auto),
            "on" => Some(ToolFollowups::On),
            "off" => Some(ToolFollowups::Off),
            _ => None,
        }
    }
}

/// The setting in force (unset or unknown = `auto`).
pub fn tool_followups(st: &SessionState) -> ToolFollowups {
    st.settings
        .string(TOOL_FOLLOWUPS)
        .as_deref()
        .and_then(ToolFollowups::parse)
        .unwrap_or(ToolFollowups::Auto)
}

/// Why an escalation turned thinking on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A call failed (non-zero exit, an edit that did not apply, a failing test or build).
    Failure,
    /// The repeat guard answered a call with its notice.
    Repeat,
    /// `fruitlessCalls` calls without progress.
    Fruitless,
}

impl Reason {
    pub fn text(self) -> &'static str {
        match self {
            Reason::Failure => "a tool call failed",
            Reason::Repeat => "the model repeated a call",
            Reason::Fruitless => "several tool calls without progress",
        }
    }
}

/// The adaptive escalation of one run (module docs). `Default` = off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Escalation {
    /// `Some` while thinking is on for tool follow-ups.
    pub on: Option<Reason>,
    /// Calls without progress since the prompt or the last successful edit, write or check.
    pub fruitless: u32,
    /// The prompt asks to fix, debug or implement something ([`task_prompt`]).
    pub task: bool,
    /// #184: a turn of this run used the whole output budget thinking; no escalation (and no
    /// thinking on a later prompt reply) for the rest of the run.
    pub overrun: bool,
}

/// What a request answers, for the thinking policy. New kinds (a repair turn, ...) are added
/// here; [`ReplyKind::thinks`] says which ones think under `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// The first reply after a user prompt (also a steering or follow-up message the user typed).
    Prompt,
    /// A reply after tool results.
    ToolFollowUp,
    /// A reply after an appended non-user instruction (extension message, branch summary,
    /// `!cmd` output, a repair message).
    Instruction,
    /// A compaction or branch summary.
    Summary,
    /// A prefill or any other request in the background.
    Background,
}

impl ReplyKind {
    /// Whether this kind thinks under `auto` (before the level `off` is applied).
    pub fn thinks(self) -> bool {
        self == ReplyKind::Prompt
    }
}

/// The policy in force: the setting, else [`ThinkingPolicy::Auto`]; `auto` acts as `fixed`
/// where a toggle is not cache-safe (see the module docs).
pub fn policy(st: &SessionState) -> ThinkingPolicy {
    match st.settings.thinking_policy() {
        ThinkingPolicy::Auto if !toggle_safe(st) => ThinkingPolicy::Fixed,
        p => p,
    }
}

/// Whether switching `enable_thinking` between requests keeps the sent bytes (module docs).
pub fn toggle_safe(st: &SessionState) -> bool {
    st.info.caps.preserve_thinking != Some(false)
}

/// The kind of the next chat reply, from the active branch: the last entry that put something
/// on the wire decides (a user message = `Prompt`, a tool result = `ToolFollowUp`, any other
/// appended message = `Instruction`). Entries that never reach the wire and rebases
/// (compaction, a new epoch) are skipped, so a retry after an overflow compaction still answers
/// the prompt.
pub fn classify(st: &SessionState) -> ReplyKind {
    classify_tree(&st.tree)
}

/// [`classify`] over a session tree (its active leaf).
pub fn classify_tree(tree: &Tree) -> ReplyKind {
    let mut id = tree.leaf().to_string();
    while let Some(e) = tree.get(&id) {
        match &e.body {
            EntryBody::Msg { wire, .. } => {
                return match wire.role {
                    Role::User => ReplyKind::Prompt,
                    Role::Tool => ReplyKind::ToolFollowUp,
                    _ => ReplyKind::Instruction,
                }
            }
            EntryBody::CustomMessage { .. } | EntryBody::BranchSummary { .. } => {
                return ReplyKind::Instruction
            }
            EntryBody::BashExecution { excluded: false, .. } => return ReplyKind::Instruction,
            _ => {}
        }
        match &e.parent {
            Some(p) => id = p.clone(),
            None => break,
        }
    }
    ReplyKind::Prompt
}

/// `enable_thinking` for a reply of `kind` under the policy in force. Under `auto` a tool
/// follow-up follows `oxa.thinking.toolFollowups` and the run's [`Escalation`].
pub fn thinking_for(st: &SessionState, kind: ReplyKind) -> bool {
    let level = st.thinking.enabled();
    match policy(st) {
        ThinkingPolicy::Fixed => level,
        ThinkingPolicy::Auto if kind == ReplyKind::ToolFollowUp => {
            level
                && match tool_followups(st) {
                    ToolFollowups::On => true,
                    ToolFollowups::Off => false,
                    ToolFollowups::Auto => st.stats.escalation.on.is_some(),
                }
        }
        ThinkingPolicy::Auto => level && kind.thinks() && !st.stats.escalation.overrun,
    }
}

/// The notice shown when a thinking-only overrun is retried without thinking.
pub const OVERRUN_NOTICE: &str = "thinking used the whole output budget; answered without thinking";

/// A reply cut at the output limit with nothing to show or run.
pub(crate) fn empty_length(turn: &oxa_proto::AssistantTurn) -> bool {
    turn.finish == oxa_proto::Finish::Length
        && turn.msg.tool_calls.is_empty()
        && turn.msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
}

/// #184: a local turn that thought (`thinking_on`) up to the output limit and produced neither
/// text nor a tool call. Never for a remote model.
pub(crate) fn reasoning_only_overrun(
    st: &SessionState,
    turn: &oxa_proto::AssistantTurn,
    thinking_on: bool,
) -> bool {
    thinking_on
        && !st.info.caps.remote
        && empty_length(turn)
        && turn
            .msg
            .reasoning
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty())
}

/// Remember the overrun for the rest of the run: thinking is no longer escalated.
pub(crate) fn record_overrun(st: &mut SessionState) {
    st.stats.escalation.overrun = true;
    st.stats.escalation.on = None;
}

/// Whether the escalation can change anything: policy `auto`, `toolFollowups: auto`, a level
/// above `off`.
fn escalation_applies(st: &SessionState) -> bool {
    policy(st) == ThinkingPolicy::Auto && tool_followups(st) == ToolFollowups::Auto && st.thinking.enabled()
}

/// Before each chat request of a run: a reply to a user prompt (also a steering message)
/// starts the escalation from off. The fix/debug/implement intent of the run is kept.
pub(crate) fn before_reply(st: &mut SessionState, ui: &mut dyn Ui) {
    if classify(st) != ReplyKind::Prompt {
        return;
    }
    let latest = st
        .transcript
        .wire()
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    let task = st.stats.escalation.task || task_prompt(latest);
    set(
        st,
        Escalation {
            task,
            overrun: st.stats.escalation.overrun,
            ..Escalation::default()
        },
        None,
        ui,
    );
}

/// At the end of a run: back to off, the footer status cleared.
pub(crate) fn end_run(st: &mut SessionState, ui: &mut dyn Ui) {
    set(st, Escalation::default(), None, ui);
}

/// Store `next`; on a change from off to on emit the notice and the status, from on to off
/// clear the status.
fn set(st: &mut SessionState, next: Escalation, sink: Option<&mut dyn EventSink>, ui: &mut dyn Ui) {
    let was = st.stats.escalation.on;
    st.stats.escalation = next;
    match (was, next.on) {
        (None, Some(reason)) => {
            ui.set_status(STATUS_KEY, Some(STATUS_ON));
            if let Some(sink) = sink {
                sink.emit(&AgentEvent::Notice {
                    level: Level::Info,
                    text: format!("{STATUS_ON}: {}; the next reply thinks", reason.text()),
                });
            }
        }
        (Some(_), None) => ui.set_status(STATUS_KEY, None),
        _ => {}
    }
}

/// After a tool batch is on the wire: update the escalation from the batch's results, in call
/// order (rules in the module docs).
pub(crate) fn observe(
    st: &mut SessionState,
    calls: &[ToolCall],
    results: &[CallResult],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    if !escalation_applies(st) || st.stats.escalation.overrun {
        return;
    }
    let limit = st.settings.u32(FRUITLESS_CALLS, FRUITLESS_DEFAULT);
    let mut esc = st.stats.escalation;
    let was_on = esc.on.is_some();
    // The calls since the latest user message, before this batch.
    let mut seen = earlier_calls(st.transcript.wire(), calls);
    let mut clean = true;
    let mut all_new = true;
    for call in calls {
        let Some(r) = results
            .iter()
            .find(|r| r.msg.tool_call_id.as_deref() == Some(call.id.as_str()))
        else {
            continue;
        };
        let text = r.msg.content.as_deref().unwrap_or("");
        let class = CallClass::of(&call.name);
        let args = parse_args(&call.args_raw);
        let new = !seen.iter().any(|(n, a)| *n == call.name && *a == args);
        seen.push((call.name.clone(), args.clone()));
        all_new &= new;
        if text.starts_with(crate::prompt::REPEAT_NOTICE_HEAD) {
            esc.on = Some(Reason::Repeat);
            clean = false;
            continue;
        }
        let check = executed_check(&call.name, &args);
        let failed = r.is_error || (esc.task && check.is_some() && reports_failure(text));
        if failed {
            esc.on.get_or_insert(Reason::Failure);
            clean = false;
            continue;
        }
        let passed = match check {
            Some(Check::Test) => reports_pass(text) && !reports_failure(text),
            Some(Check::Build) => !reports_failure(text),
            None => false,
        };
        if class == CallClass::Write || passed {
            // Progress: an edit or write that applied, a test or build that passed.
            esc.on = None;
            esc.fruitless = 0;
            continue;
        }
        if !(class == CallClass::Read && new) {
            esc.fruitless += 1;
        }
        if limit > 0 && esc.fruitless >= limit && esc.on.is_none() {
            esc.on = Some(Reason::Fruitless);
        }
    }
    // Outside a fix/debug/implement task, a clean batch of new calls releases it.
    if was_on && !esc.task && clean && all_new && esc.on.is_some() {
        esc.on = None;
        esc.fruitless = 0;
    }
    set(st, esc, Some(sink), ui);
}

/// What a tool does, for the escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallClass {
    /// Changes a file: `edit`, `write`.
    Write,
    /// Reads without side effects: `read`, `grep`, `find`, `ls`.
    Read,
    Other,
}

impl CallClass {
    fn of(name: &str) -> CallClass {
        match name {
            "edit" | "write" => CallClass::Write,
            "read" | "grep" | "find" | "ls" => CallClass::Read,
            _ => CallClass::Other,
        }
    }
}

fn parse_args(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The calls of the assistant messages after the latest user message, except the message that
/// holds `batch` (it is already on the wire).
fn earlier_calls(wire: &[oxa_proto::WireMsg], batch: &[ToolCall]) -> Vec<(String, Value)> {
    let start = wire
        .iter()
        .rposition(|m| m.role == Role::User)
        .map_or(0, |i| i + 1);
    let ids: Vec<&str> = batch.iter().map(|c| c.id.as_str()).collect();
    wire[start..]
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter(|m| !m.tool_calls.iter().any(|c| ids.contains(&c.id.as_str())))
        .flat_map(|m| m.tool_calls.iter())
        .map(|c| (c.name.clone(), parse_args(&c.args_raw)))
        .collect()
}

/// What an executed check command is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// A test runner (`cargo test`, `pytest`, `go test`, `npm test`, `make test`, ...): it
    /// passes only with exit 0, no failure marker and a recognised pass summary.
    Test,
    /// A build or type check (`cargo build|check|clippy`, `make`, `tsc`, `go build|vet`,
    /// `mypy`, ...): it passes with exit 0 and no failure marker (many print nothing).
    Build,
}

/// The check a `bash` call actually EXECUTES, if any. Only the program position of each
/// simple command counts (after `;`, `&&`, `||`, `|`, a newline, leading `VAR=x` assignments
/// and wrappers such as `time`, `env`, `timeout 60`); a word inside arguments or quotes never
/// does, so `grep -rn "cargo test" .`, `rg test_`, `echo pytest` or `ls tests/` are not checks.
/// A test runner wins over a build in the same command.
fn executed_check(name: &str, args: &Value) -> Option<Check> {
    if name != "bash" {
        return None;
    }
    let cmd = args.get("command").and_then(Value::as_str)?;
    let mut found = None;
    for words in simple_commands(cmd) {
        let w: Vec<String> = program_words(&words)
            .iter()
            .map(|x| x.to_ascii_lowercase())
            .collect();
        let Some(prog) = w.first() else { continue };
        let prog = prog.rsplit('/').next().unwrap_or(prog).to_string();
        let sub = w.get(1).map(String::as_str).unwrap_or("");
        let kind = match prog.as_str() {
            "cargo" => match sub {
                "test" | "nextest" => Some(Check::Test),
                "build" | "check" | "clippy" => Some(Check::Build),
                _ => None,
            },
            "go" | "zig" | "dotnet" => match sub {
                "test" => Some(Check::Test),
                "build" | "vet" => Some(Check::Build),
                _ => None,
            },
            "npm" | "yarn" | "pnpm" | "bun" => match (sub, w.get(2).map(String::as_str)) {
                ("test", _) | ("run", Some("test")) => Some(Check::Test),
                ("build", _) | ("run", Some("build")) => Some(Check::Build),
                _ => None,
            },
            "make" | "ninja" => {
                if w[1..]
                    .iter()
                    .any(|a| matches!(a.as_str(), "test" | "tests" | "check"))
                {
                    Some(Check::Test)
                } else {
                    Some(Check::Build)
                }
            }
            "python" | "python3" if sub == "-m" => match w.get(2).map(String::as_str) {
                Some("pytest" | "unittest") => Some(Check::Test),
                Some("mypy") => Some(Check::Build),
                _ => None,
            },
            "pytest" | "ctest" | "jest" | "vitest" | "rspec" => Some(Check::Test),
            "mvn" | "gradle" | "./gradlew" | "gradlew" => {
                if w[1..].iter().any(|a| a == "test") {
                    Some(Check::Test)
                } else {
                    Some(Check::Build)
                }
            }
            "tsc" | "mypy" => Some(Check::Build),
            _ => None,
        };
        match kind {
            Some(Check::Test) => return Some(Check::Test),
            Some(Check::Build) => found = Some(Check::Build),
            None => {}
        }
    }
    found
}

/// The words of `cmd` split into simple commands at unquoted `;`, `&`, `|` and newlines, with
/// quotes removed (a small shell tokenizer: enough to find program positions).
fn simple_commands(cmd: &str) -> Vec<Vec<String>> {
    let mut out = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(n) = chars.next() {
                    word.push(n);
                }
            }
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        word.push(n);
                        in_word = true;
                    }
                }
                ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' => {
                    if in_word {
                        out.last_mut().unwrap().push(std::mem::take(&mut word));
                        in_word = false;
                    }
                    if !out.last().unwrap().is_empty() {
                        out.push(Vec::new());
                    }
                }
                c if c.is_whitespace() => {
                    if in_word {
                        out.last_mut().unwrap().push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    word.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        out.last_mut().unwrap().push(word);
    }
    out.retain(|c| !c.is_empty());
    out
}

/// The words from the program on: leading `VAR=x` assignments, redirections and wrappers
/// (`time`, `env`, `nice`, `command`, `exec`, `sudo`, `timeout <d>`) are skipped.
fn program_words(words: &[String]) -> &[String] {
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        let assignment = w
            .split_once('=')
            .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        if assignment || matches!(w, "time" | "env" | "nice" | "command" | "exec" | "sudo" | "!") {
            i += 1;
        } else if w == "timeout" {
            i += 2;
        } else {
            break;
        }
    }
    &words[i.min(words.len())..]
}

/// A test runner's output that reports a pass (the runner's own summary line).
fn reports_pass(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "test result: ok",
        " passed",
        "PASSED",
        "\nok ",
        "ok  \t",
        "\nPASS",
        "PASS ",
        "\nOK",
        "Tests:",
        "All tests passed",
    ];
    let t = format!("\n{text}");
    MARKERS.iter().any(|m| t.contains(m))
}

/// Output text of a test or build that reports a failure.
fn reports_failure(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "test result: FAILED",
        "error: could not compile",
        "error[E",
        ": error:",
        "--- FAIL",
        "FAILED ",
        "FAIL:",
        "AssertionError",
        "Traceback (most recent call last)",
        "npm ERR!",
        "panicked at",
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

/// Whether a prompt asks to fix, debug or implement something (whole words, any case).
pub fn task_prompt(text: &str) -> bool {
    const WORDS: &[&str] = &[
        "fix",
        "fixes",
        "fixing",
        "debug",
        "debugging",
        "implement",
        "implementing",
        "bug",
        "bugs",
        "failing",
        "fails",
        "broken",
        "repair",
        "solve",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .any(|w| WORDS.iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// The value `exec::request` sends when the caller did not set one: chat (and subagent) turns
/// by [`classify`], summaries and prefills as background work.
pub(crate) fn request_thinking(st: &SessionState, kind: TurnKind) -> bool {
    let reply = match kind {
        TurnKind::Chat | TurnKind::Subagent => classify(st),
        TurnKind::Compaction | TurnKind::BranchSummary => ReplyKind::Summary,
        TurnKind::Prefill => ReplyKind::Background,
    };
    thinking_for(st, reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::{Entry, FrozenPrefix, Usage, Value, WireMsg};

    fn tree(bodies: Vec<EntryBody>) -> Tree {
        let mut t = Tree::new(Vec::new());
        for (i, body) in bodies.into_iter().enumerate() {
            t.push(Entry {
                id: format!("{i:08x}"),
                parent: (i > 0).then(|| format!("{:08x}", i - 1)),
                ts_ms: 0,
                body,
            });
        }
        t
    }
    fn msg(w: WireMsg) -> EntryBody {
        EntryBody::Msg {
            wire: w,
            parts: Vec::new(),
        }
    }
    fn prefix() -> EntryBody {
        EntryBody::Prefix(FrozenPrefix {
            sections: Vec::new(),
            tools_json: String::from("[]"),
            sha256: [0; 32],
        })
    }
    fn usage() -> EntryBody {
        EntryBody::Usage {
            kind: "chat".to_string(),
            usage: Usage::default(),
        }
    }

    #[test]
    fn the_last_wire_entry_decides() {
        assert_eq!(classify_tree(&tree(vec![])), ReplyKind::Prompt);
        assert_eq!(
            classify_tree(&tree(vec![prefix(), msg(WireMsg::user("hi"))])),
            ReplyKind::Prompt
        );
        let after_tools = vec![
            prefix(),
            msg(WireMsg::user("hi")),
            msg(WireMsg::assistant("")),
            usage(),
            msg(WireMsg::tool_result("c1", "ok")),
        ];
        assert_eq!(classify_tree(&tree(after_tools.clone())), ReplyKind::ToolFollowUp);
        // An extension message appended after the results is an instruction, not a prompt.
        let mut instr = after_tools.clone();
        instr.push(EntryBody::CustomMessage {
            custom_type: "x".to_string(),
            content: "do more".to_string(),
            display: true,
            details: Value::Null,
        });
        assert_eq!(classify_tree(&tree(instr)), ReplyKind::Instruction);
        // A steering message the user typed is a prompt.
        let mut steer = after_tools.clone();
        steer.push(msg(WireMsg::user("also this")));
        assert_eq!(classify_tree(&tree(steer)), ReplyKind::Prompt);
        // Entries off the wire and rebases are skipped: a retry after an overflow compaction
        // still answers the prompt.
        let retry = vec![
            prefix(),
            msg(WireMsg::user("hi")),
            EntryBody::Compaction {
                summary: "s".to_string(),
                first_kept: "00000001".to_string(),
                tokens_before: 1,
                details: Value::Null,
                from_hook: false,
            },
            EntryBody::Custom {
                custom_type: "y".to_string(),
                data: Value::Null,
            },
        ];
        assert_eq!(classify_tree(&tree(retry)), ReplyKind::Prompt);
    }

    #[test]
    fn task_prompts_and_checks() {
        assert!(task_prompt("Find and fix the bug in gguf"));
        assert!(task_prompt("please DEBUG this"));
        assert!(task_prompt("Implement the parser"));
        assert!(!task_prompt("explain how the prefix cache works"));
        assert!(!task_prompt("prefix of the file"));
        let cmd = |c: &str| serde_json::json!({ "command": c });
        let check = |c: &str| executed_check("bash", &cmd(c));
        assert_eq!(check("cargo test -p gguf 2>&1 | tail -20"), Some(Check::Test));
        assert_eq!(check("cd x && python -m pytest -q"), Some(Check::Test));
        assert_eq!(
            check("RUST_BACKTRACE=1 timeout 600 cargo test"),
            Some(Check::Test)
        );
        assert_eq!(check("cargo build && cargo test"), Some(Check::Test));
        assert_eq!(check("make -j8"), Some(Check::Build));
        assert_eq!(check("make test"), Some(Check::Test));
        assert_eq!(check("npm run test"), Some(Check::Test));
        assert_eq!(check("go test ./..."), Some(Check::Test));
        // Mentions in arguments, quotes or output are never an executed check.
        for c in [
            "grep -rn cargo src",
            "grep -rn \"cargo test\" .",
            "rg 'cargo test' README.md",
            "grep -n pytest README.md",
            "rg test_",
            "echo pytest",
            "ls tests/",
            "ls makefiles",
            "cat tests/test_parser.py",
            "grep -n 'a; cargo test' notes.txt",
            "find . -name '*test*'",
            "git log --grep='make test'",
        ] {
            assert_eq!(check(c), None, "{c}");
        }
        assert_eq!(executed_check("read", &cmd("cargo test")), None);
        assert!(reports_pass(
            "running 11 tests\ntest result: ok. 11 passed; 0 failed"
        ));
        assert!(reports_pass("===== 5 passed in 0.12s ====="));
        assert!(!reports_pass("(no output)"));
        assert!(reports_failure("test result: FAILED. 1 passed; 1 failed"));
        assert!(!reports_failure("test result: ok. 11 passed; 0 failed"));
    }

    #[test]
    fn only_the_prompt_kind_thinks() {
        let all = [
            ReplyKind::Prompt,
            ReplyKind::ToolFollowUp,
            ReplyKind::Instruction,
            ReplyKind::Summary,
            ReplyKind::Background,
        ];
        let thinks: Vec<bool> = all.iter().map(|k| k.thinks()).collect();
        assert_eq!(thinks, vec![true, false, false, false, false]);
    }
}
