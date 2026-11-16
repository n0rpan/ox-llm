//! T2 (wave H): the append-only repair turn for a malformed tool call.
//!
//! The engine returns a tool call it cannot parse as plain content (lenient final parse: the
//! text from the failed call on is appended to `content`, glued to the prose before it, since
//! the content scan stops at `\n\n<tool_call>` / `\n<tool_call>` / `<tool_call>` and, for a
//! model without thinking, also at `<function=NAME>`). Without this module such a reply ends
//! the run silently with the raw block on screen.
//!
//! ## Detection rule (exact)
//! A reply is a repair candidate when all hold:
//! 1. it has **no** tool calls (a reply with real calls is never a candidate, whatever its
//!    content quotes after them);
//! 2. it did not stop at the output limit (`Finish::Length` keeps pi's behaviour: the stop
//!    reason is shown, nothing is appended);
//! 3. its `content` holds an **opener** outside code: `<tool_call>`, or `<function=` followed by
//!    a name character (`[A-Za-z0-9_.:-]`). "Outside code" = not inside a fenced block (a line
//!    whose first non-space characters are ` ``` ` or `~~~` opens or closes one) and not inside
//!    an inline code span (a backtick run on a line opens a span that the next run of the same
//!    length on that line closes). Reasoning is never scanned (the engine moves a call written
//!    in the thinking block to the content).
//!
//! The first opener outside code starts the block that is classified ([`classify`]). The cause
//! is named in one fixed user message (`prompt::repair_message`), stored as a `custom_message`
//! entry of type [`CUSTOM_TYPE`] with `details {cause, attempt}` (the session file is what a
//! digest or ledger reads), and the loop runs the next turn. At most [`MAX_IN_ROW`] repairs in a
//! row; the next malformed reply stops the run with a shown stop message (R6 shape). Nothing is
//! rewritten: the malformed reply stays on the wire exactly as streamed (W1, W2).
use crate::agent_loop::persist;
use crate::SessionState;
use oxa_proto::{
    problem_text, repair_cause, shown_why, AgentEvent, AssistantTurn, EntryBody, EventSink, Finish, Level,
    OxaError, Role, Value,
};

/// The setting that turns the repair turn off (default on).
pub const SETTING: &str = "oxa.toolRepair";
/// `custom_type` of the stored repair message.
pub const CUSTOM_TYPE: &str = "oxa.tool_repair";
/// Repairs in a row before the run stops.
pub const MAX_IN_ROW: u32 = 2;

/// Why a block could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    UnknownFunction {
        name: String,
        available: Vec<String>,
    },
    UnknownParameter {
        tool: String,
        name: String,
        valid: Vec<String>,
    },
    MissingRequired {
        tool: String,
        missing: Vec<String>,
    },
    Unclosed {
        tag: &'static str,
    },
    BadJson {
        tool: String,
        param: String,
    },
    WrongType {
        tool: String,
        param: String,
        expected: String,
    },
    Format,
}

impl Cause {
    pub fn code(&self) -> &'static str {
        match self {
            Cause::UnknownFunction { .. } => repair_cause::UNKNOWN_FUNCTION,
            Cause::UnknownParameter { .. } => repair_cause::UNKNOWN_PARAMETER,
            Cause::MissingRequired { .. } => repair_cause::MISSING_REQUIRED,
            Cause::Unclosed { .. } => repair_cause::UNCLOSED,
            Cause::BadJson { .. } => repair_cause::BAD_JSON,
            Cause::WrongType { .. } => repair_cause::WRONG_TYPE,
            Cause::Format => repair_cause::FORMAT,
        }
    }

    /// The cause sentence of the repair message (model-facing text from `prompt.rs`).
    pub fn line(&self) -> String {
        use crate::prompt as p;
        match self {
            Cause::UnknownFunction { name, available } => {
                p::repair_unknown_function(name, &available.iter().map(String::as_str).collect::<Vec<_>>())
            }
            Cause::UnknownParameter { tool, name, valid } => {
                p::repair_unknown_parameter(tool, name, &valid.iter().map(String::as_str).collect::<Vec<_>>())
            }
            Cause::MissingRequired { tool, missing } => {
                p::repair_missing_required(tool, &missing.iter().map(String::as_str).collect::<Vec<_>>())
            }
            Cause::Unclosed { tag } => p::repair_unclosed(tag),
            Cause::BadJson { tool, param } => p::repair_bad_json(tool, param),
            Cause::WrongType {
                tool,
                param,
                expected,
            } => p::repair_wrong_type(tool, param, expected),
            Cause::Format => p::REPAIR_FORMAT.to_string(),
        }
    }
}

/// A declared tool as the detector sees it: its name and parameter schema JSON.
pub struct Declared<'a> {
    pub name: &'a str,
    pub schema_json: &'a str,
}

/// Rule 3 of the module docs: the byte offset of the first opener outside code.
pub fn find_opener(content: &str) -> Option<usize> {
    let mut fenced = false;
    let mut line_start = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            line_start += line.len();
            continue;
        }
        if !fenced {
            if let Some(off) = opener_in_line(line) {
                return Some(line_start + off);
            }
        }
        line_start += line.len();
    }
    None
}

/// The first opener of one line that is not inside an inline code span.
fn opener_in_line(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = 0usize;
    let mut span: Option<usize> = None; // length of the backtick run that opened a span
    while i < b.len() {
        if b[i] == b'`' {
            let mut j = i;
            while j < b.len() && b[j] == b'`' {
                j += 1;
            }
            let run = j - i;
            span = match span {
                None => Some(run),
                Some(open) if open == run => None,
                other => other,
            };
            i = j;
            continue;
        }
        if span.is_none() && b[i] == b'<' {
            let rest = &line[i..];
            if rest.starts_with("<tool_call>") {
                return Some(i);
            }
            if let Some(after) = rest.strip_prefix("<function=") {
                if after.chars().next().is_some_and(name_char) {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

/// The candidate check of the module docs, then [`classify`]. `None` = not a malformed call.
pub fn detect(turn: &AssistantTurn, tools: &[Declared<'_>]) -> Option<Cause> {
    if !turn.msg.tool_calls.is_empty() || turn.finish == Finish::Length || turn.msg.role != Role::Assistant {
        return None;
    }
    let content = turn.msg.content.as_deref()?;
    let at = find_opener(content)?;
    Some(classify(&content[at..], tools))
}

/// Classify the block that starts at an opener (`<tool_call>` or `<function=`). Checks in
/// order: the outer `<tool_call>` form, the function name, each `<parameter=..>` in order
/// (closed, declared, value type), the required set, the closing tags. A block that passes
/// every check (the engine refused it for a finer reason, e.g. a missing newline) is `Format`.
pub fn classify(block: &str, tools: &[Declared<'_>]) -> Cause {
    let mut s = block;
    let wrapped = s.starts_with("<tool_call>");
    if wrapped {
        s = s["<tool_call>".len()..].trim_start();
        if s.is_empty() {
            return Cause::Unclosed { tag: "</tool_call>" };
        }
        if !s.starts_with("<function=") {
            return Cause::Format;
        }
    }
    let after = &s["<function=".len()..];
    let Some(gt) = after.find('>') else {
        return Cause::Unclosed { tag: ">" };
    };
    let name = after[..gt].trim();
    let Some(tool) = tools.iter().find(|t| t.name == name) else {
        let mut available: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        available.sort();
        return Cause::UnknownFunction {
            name: name.to_string(),
            available,
        };
    };
    let schema: Value = serde_json::from_str(tool.schema_json).unwrap_or(Value::Null);
    let props = schema.get("properties").and_then(Value::as_object);
    let valid: Vec<String> = props.map(|p| p.keys().cloned().collect()).unwrap_or_default();
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let tool_name = tool.name.to_string();

    let full = &after[gt + 1..];
    let region_end = full.find("</function>").unwrap_or(full.len());
    let mut body = &full[..region_end];
    let mut seen: Vec<String> = Vec::new();
    while let Some(p) = body.find("<parameter=") {
        let rest = &body[p + "<parameter=".len()..];
        let Some(pgt) = rest.find('>') else {
            return Cause::Unclosed { tag: ">" };
        };
        let pname = rest[..pgt].trim().to_string();
        let value_and_more = &rest[pgt + 1..];
        let Some(close) = value_and_more.find("</parameter>") else {
            return Cause::Unclosed { tag: "</parameter>" };
        };
        if !valid.contains(&pname) {
            return Cause::UnknownParameter {
                tool: tool_name,
                name: pname,
                valid,
            };
        }
        let raw = value_and_more[..close].trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r'));
        let ptype = props
            .and_then(|m| m.get(&pname))
            .map(declared_types)
            .unwrap_or_default();
        if let Some(cause) = check_value(&tool_name, &pname, raw, &ptype) {
            return cause;
        }
        seen.push(pname);
        body = &value_and_more[close + "</parameter>".len()..];
    }
    let missing: Vec<String> = required.iter().filter(|r| !seen.contains(r)).cloned().collect();
    if !missing.is_empty() {
        return Cause::MissingRequired {
            tool: tool_name,
            missing,
        };
    }
    if region_end == full.len() {
        return Cause::Unclosed { tag: "</function>" };
    }
    if wrapped && !full[region_end..].contains("</tool_call>") {
        return Cause::Unclosed { tag: "</tool_call>" };
    }
    Cause::Format
}

/// The JSON Schema `type` names of a property (`type` string or list; none = any).
fn declared_types(prop: &Value) -> Vec<String> {
    match prop.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// A value of a parameter that cannot be a string must be JSON of a declared type. The
/// engine's own lenient coercion is mirrored: one level of JSON-string quoting, Python
/// `True`/`False`/`None`, an integral float for an integer.
fn check_value(tool: &str, param: &str, raw: &str, types: &[String]) -> Option<Cause> {
    if types.is_empty() || types.iter().any(|t| t == "string") {
        return None;
    }
    let bad_json = || Cause::BadJson {
        tool: tool.to_string(),
        param: param.to_string(),
    };
    let wrong = || Cause::WrongType {
        tool: tool.to_string(),
        param: param.to_string(),
        expected: types.join(" or "),
    };
    let fits = |v: &Value| types.iter().any(|t| type_fits(t, v));
    let parsed: Option<Value> = serde_json::from_str(raw).ok();
    match parsed {
        Some(v) if fits(&v) => None,
        Some(Value::String(inner)) => match serde_json::from_str::<Value>(inner.trim()) {
            Ok(v) if fits(&v) => None,
            _ => Some(wrong()),
        },
        Some(_) => Some(wrong()),
        None => {
            let python = match raw {
                r if r.eq_ignore_ascii_case("true") => Some(Value::Bool(true)),
                r if r.eq_ignore_ascii_case("false") => Some(Value::Bool(false)),
                "None" => Some(Value::Null),
                _ => None,
            };
            match python {
                Some(v) if fits(&v) => None,
                _ => Some(bad_json()),
            }
        }
    }
}

fn type_fits(t: &str, v: &Value) -> bool {
    match t {
        "number" => v.is_number(),
        "integer" => v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// Repairs in a row that end at the leaf: walking back over the branch, each repair message
/// that is followed only by assistant replies without tool calls (and bookkeeping entries)
/// counts; anything else on the wire ends the row.
fn repairs_in_row(st: &SessionState) -> u32 {
    let leaf = st.tree.leaf().to_string();
    let mut n = 0;
    for e in st.tree.branch(&leaf).into_iter().rev() {
        match &e.body {
            EntryBody::CustomMessage { custom_type, .. } if custom_type == CUSTOM_TYPE => n += 1,
            EntryBody::Msg { wire, .. } if wire.role == Role::Assistant && wire.tool_calls.is_empty() => {}
            EntryBody::Usage { .. }
            | EntryBody::Shown { .. }
            | EntryBody::Label { .. }
            | EntryBody::Custom { .. } => {}
            _ => break,
        }
    }
    n
}

/// Called by the loop after a reply without tool calls was pushed. `true` = a repair message
/// was appended and the loop runs the next turn; `false` = the run ends (no malformed call,
/// repair off, or the cap was reached: then a stop message is shown).
pub(crate) fn repair_turn(
    st: &mut SessionState,
    turn: &AssistantTurn,
    sink: &mut dyn EventSink,
) -> Result<bool, OxaError> {
    if !st.settings.bool(SETTING, true) {
        return Ok(false);
    }
    let cause = {
        let specs: Vec<_> = st.active.direct.iter().map(|t| t.spec().clone()).collect();
        let declared: Vec<Declared<'_>> = specs
            .iter()
            .map(|s| Declared {
                name: &s.name,
                schema_json: &s.schema_json,
            })
            .collect();
        match detect(turn, &declared) {
            Some(c) => c,
            None => return Ok(false),
        }
    };
    let in_row = repairs_in_row(st);
    if in_row >= MAX_IN_ROW {
        let text = stop_text(&cause);
        persist(
            st,
            EntryBody::Shown {
                text: text.clone(),
                why: shown_why::NOTICE,
            },
            sink,
        )?;
        sink.emit(&AgentEvent::ToolRepair {
            attempt: in_row,
            cause: cause.code(),
            total: st.stats.tool_repairs,
            stopped: true,
        });
        sink.emit(&AgentEvent::Notice {
            level: Level::Error,
            text,
        });
        return Ok(false);
    }
    let attempt = in_row + 1;
    persist(
        st,
        EntryBody::CustomMessage {
            custom_type: CUSTOM_TYPE.to_string(),
            content: crate::prompt::repair_message(&cause.line()),
            display: false,
            details: serde_json::json!({ "cause": cause.code(), "attempt": attempt }),
        },
        sink,
    )?;
    st.stats.tool_repairs += 1;
    sink.emit(&AgentEvent::ToolRepair {
        attempt,
        cause: cause.code(),
        total: st.stats.tool_repairs,
        stopped: false,
    });
    sink.emit(&AgentEvent::Notice {
        level: Level::Warning,
        text: notice_text(&cause, attempt),
    });
    Ok(true)
}

/// Append-only final-answer repair, independent of malformed-tool repair settings/counters.
/// The budget belongs to the whole run, so tools or steering cannot reset it.
pub(crate) fn schema_repair_turn(
    st: &mut SessionState,
    turn: &AssistantTurn,
    repaired: &mut bool,
    sink: &mut dyn EventSink,
) -> Result<bool, OxaError> {
    let Some(schema) = &st.output_schema else {
        return Ok(false);
    };
    if schema.matches(turn.msg.content.as_deref().unwrap_or_default()) {
        return Ok(false);
    }
    if *repaired {
        return Err(OxaError::new(
            oxa_proto::codes::SCHEMA_MISMATCH,
            "Final answer did not match the output schema after one repair turn",
        ));
    }
    let content = format!(
        "Your final answer did not match the output schema. Return only valid JSON matching this schema:\n{}",
        schema.value()
    );
    persist(
        st,
        EntryBody::CustomMessage {
            custom_type: "oxa.schema_repair".into(),
            content,
            display: false,
            details: serde_json::json!({"cause":"schema_mismatch", "attempt":1}),
        },
        sink,
    )?;
    *repaired = true;
    Ok(true)
}

/// The notice of one repair (R6 shape).
pub fn notice_text(cause: &Cause, attempt: u32) -> String {
    problem_text(
        &format!("Malformed tool call, repair {attempt} of {MAX_IN_ROW}"),
        &cause_reason(cause),
        "none needed: the model was asked to write the call again",
    )
}

/// The stop message at the cap (R6 shape).
pub fn stop_text(cause: &Cause) -> String {
    problem_text(
        &format!(
            "Run stopped: the model wrote a malformed tool call {} times in a row",
            MAX_IN_ROW + 1
        ),
        &cause_reason(cause),
        "send the request again, rephrase it, or switch model with /model",
    )
}

/// The user-facing reason for a cause (the code plus its sentence).
fn cause_reason(cause: &Cause) -> String {
    format!("{} ({})", cause.code().replace('_', " "), cause.line())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opener_outside_and_inside_code() {
        assert_eq!(find_opener("ok<tool_call>\n<function=read>"), Some(2));
        assert_eq!(find_opener("use `<tool_call>` tags"), None);
        assert_eq!(find_opener("use ``a ` <tool_call>`` tags"), None);
        assert_eq!(find_opener("```\n<tool_call>\n```\n"), None);
        assert_eq!(find_opener("```\nx\n```\n<function=read>\n"), Some(10));
        assert_eq!(find_opener("<function= read>"), None);
        assert_eq!(find_opener("a <function=x"), Some(2));
        assert_eq!(find_opener("plain text"), None);
    }
}
