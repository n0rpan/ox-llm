//! One read-only status view for a session's work.
//!
//! The interactive `/status`, the headless `/status` built-in and bare `oxa status` all render
//! this view: the same rows, in the same order, in the same sections. A surface supplies the
//! facts it can read from a typed API and leaves the rest alone; a row without a fact is shown
//! as unavailable with a fixed reason, never as a zero or an empty value. A fact that is not
//! known is therefore visible as such, and every row gains a value only when a surface really
//! has one.
//!
//! Reading is side-effect free: this module formats strings and owns no file, engine, process
//! or clock handle.

use std::collections::BTreeMap;

/// One row of the status view. The order of [`Row::ALL`] is the order of the view; the row's
/// section starts whenever the section of a row differs from the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Row {
    SessionFile,
    SessionId,
    SessionName,
    Messages,
    Tokens,
    Context,
    LastCache,
    Turn,
    Model,
    Steering,
    FollowUp,
    EngineQueue,
    EngineEndpoint,
    EngineModel,
    Jobs,
    Children,
    Goal,
    Lead,
    Cost,
}

impl Row {
    /// Every row, in view order.
    pub const ALL: [Row; 19] = [
        Row::SessionFile,
        Row::SessionId,
        Row::SessionName,
        Row::Messages,
        Row::Tokens,
        Row::Context,
        Row::LastCache,
        Row::Turn,
        Row::Model,
        Row::Steering,
        Row::FollowUp,
        Row::EngineQueue,
        Row::EngineEndpoint,
        Row::EngineModel,
        Row::Jobs,
        Row::Children,
        Row::Goal,
        Row::Lead,
        Row::Cost,
    ];

    /// The section heading the row belongs to.
    pub fn section(self) -> &'static str {
        match self {
            Row::SessionFile
            | Row::SessionId
            | Row::SessionName
            | Row::Messages
            | Row::Tokens
            | Row::Context
            | Row::LastCache => "Session",
            Row::Turn | Row::Model => "Turn",
            Row::Steering | Row::FollowUp | Row::EngineQueue => "Queues",
            Row::EngineEndpoint | Row::EngineModel => "Engine",
            Row::Jobs | Row::Children | Row::Goal | Row::Lead => "Work",
            Row::Cost => "Budget",
        }
    }

    /// The row label as the view prints it.
    pub fn label(self) -> &'static str {
        match self {
            Row::SessionFile => "Session file",
            Row::SessionId => "Session id",
            Row::SessionName => "Name",
            Row::Messages => "Messages",
            Row::Tokens => "Tokens",
            Row::Context => "Context",
            Row::LastCache => "Cache, last request",
            Row::Turn => "Turn",
            Row::Model => "Model",
            Row::Steering => "Steering",
            Row::FollowUp => "Follow-up",
            Row::EngineQueue => "Engine queue",
            Row::EngineEndpoint => "Engine endpoint",
            Row::EngineModel => "Engine model",
            Row::Jobs => "Background jobs",
            Row::Children => "Child sessions",
            Row::Goal => "Goal",
            Row::Lead => "Lead",
            Row::Cost => "Cost",
        }
    }

    /// Why a surface has no value for this row: the fixed, honest reason the view prints
    /// instead of a fabricated zero. A surface that does have the fact sets the row instead.
    pub fn unavailable(self) -> &'static str {
        match self {
            Row::SessionFile | Row::SessionId | Row::SessionName | Row::Messages => {
                "the session store is not readable from this view"
            }
            Row::Tokens | Row::Context | Row::LastCache => {
                "the worker snapshot records no counters for this row"
            }
            Row::Turn => "this view receives no turn state",
            Row::Model => "this view records no model fact",
            Row::Steering | Row::FollowUp => "this view receives no live queues",
            Row::EngineQueue | Row::EngineEndpoint | Row::EngineModel => {
                "the engine endpoint is not known to this view"
            }
            Row::Jobs => "no live job registry on this surface",
            Row::Children => "no child-session listing yet",
            Row::Goal => "no goal record for this session",
            Row::Lead => "no lead facts for this session",
            Row::Cost => "no cost meter on this surface",
        }
    }
}

/// `<model> (local)`, `<model> (cloud · <provider>)`, or empty when the surface recorded no
/// model. The location comes from the engine's per-model capability, never from a guess.
pub fn model_text(model: &str, remote: bool, provider: Option<&str>) -> String {
    if model.is_empty() {
        return String::new();
    }
    if remote {
        format!("{model} (cloud · {})", provider.unwrap_or("remote"))
    } else {
        format!("{model} (local)")
    }
}

/// `<used> / <window> tokens`, or that the engine publishes no window.
pub fn context_text(used: u32, window: u32) -> String {
    if window > 0 {
        format!("{used} / {window} tokens")
    } else {
        format!("{used} tokens (window unknown)")
    }
}

/// `user <n>, assistant <n>, tool calls <n>`.
pub fn messages_text(user: u32, assistant: u32, tool_calls: u32) -> String {
    format!("user {user}, assistant {assistant}, tool calls {tool_calls}")
}

/// `input <n>, output <n>`.
pub fn tokens_text(input: u64, output: u64) -> String {
    format!("input {input}, output {output}")
}

/// The shared status view: the rows a surface could fill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusView {
    session_id: String,
    facts: BTreeMap<Row, String>,
}

impl StatusView {
    /// A view of `session_id`. Every row starts unavailable.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            facts: BTreeMap::new(),
        }
    }

    /// Set one row to a fact the surface read from a typed API. An empty value is not a fact
    /// and leaves the row unavailable.
    pub fn set(mut self, row: Row, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.trim().is_empty() {
            self.facts.insert(row, value);
        }
        self
    }

    /// The rendered view: a title line, then each section once, then its rows in
    /// [`Row::ALL`] order. Rows are static text; nothing here reads a clock or a colour.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("Status of session {}", self.session_id)];
        let mut section = None;
        for row in Row::ALL {
            if section != Some(row.section()) {
                out.push(row.section().to_string());
                section = Some(row.section());
            }
            out.push(match self.facts.get(&row) {
                Some(value) => format!("  {}: {value}", row.label()),
                None => format!("  {}: unavailable ({})", row.label(), row.unavailable()),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_starts_unavailable_and_states_why() {
        let lines = StatusView::new("s-1").lines();
        assert_eq!(lines[0], "Status of session s-1");
        for row in Row::ALL {
            let expected = format!("  {}: unavailable ({})", row.label(), row.unavailable());
            assert!(lines.contains(&expected), "{expected:?} missing from {lines:?}");
        }
    }

    #[test]
    fn sections_appear_once_in_row_order() {
        let mut view = StatusView::new("s-1");
        view = view.set(Row::SessionId, "s-1");
        view = view.set(Row::Turn, "idle");
        view = view.set(Row::Jobs, "0 running");
        let lines = view.lines();

        // The headings occur once each, in this order: reading the sequence in the order the
        // view prints it, not by asking whether each heading appears anywhere.
        let mut seen: Vec<&str> = Vec::new();
        for line in &lines {
            if !["Session", "Turn", "Queues", "Engine", "Work", "Budget"].contains(&line.as_str()) {
                continue;
            }
            let section: &str = line;
            assert!(
                !seen.contains(&section),
                "section {section:?} appears twice: {lines:?}"
            );
            seen.push(section);
        }
        assert_eq!(
            seen,
            ["Session", "Turn", "Queues", "Engine", "Work", "Budget"],
            "{lines:?}"
        );

        // Each row sits at its own place, right after the heading of its section: a row moved
        // to another section, a missing row or an extra trailing row fails here.
        let mut at = 0;
        let mut section = None;
        for row in Row::ALL {
            if section != Some(row.section()) {
                assert_eq!(
                    lines.get(at).map(String::as_str),
                    Some(row.section()),
                    "{lines:?}"
                );
                at += 1;
                section = Some(row.section());
            }
            let expected = format!("  {}: unavailable ({})", row.label(), row.unavailable());
            let line = lines.get(at).unwrap_or_else(|| panic!("{row:?} missing"));
            if matches!(row, Row::SessionId | Row::Turn | Row::Jobs) {
                assert!(
                    line.starts_with(&format!("  {}: ", row.label())),
                    "{row:?} at {at}: {lines:?}"
                );
            } else {
                assert_eq!(line, &expected, "{row:?} at {at}: {lines:?}");
            }
            at += 1;
        }
        assert_eq!(at, lines.len(), "rows after the last one: {lines:?}");
        assert!(lines.contains(&"  Turn: idle".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"  Background jobs: 0 running".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn an_empty_fact_is_not_a_value() {
        let mut view = StatusView::new("s-1");
        view = view.set(Row::SessionName, "  ");
        let lines = view.lines();
        let expected = format!("  Name: unavailable ({})", Row::SessionName.unavailable());
        assert!(lines.contains(&expected), "{lines:?}");
    }

    #[test]
    fn a_fact_replaces_only_its_own_row() {
        let mut view = StatusView::new("s-1");
        view = view.set(Row::Cost, "local (no metered spend)");
        let lines = view.lines();
        assert!(
            lines.contains(&"  Cost: local (no metered spend)".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"  Lead: unavailable (no lead facts for this session)".to_string()));
    }
}
