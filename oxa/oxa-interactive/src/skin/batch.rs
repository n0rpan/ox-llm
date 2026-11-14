//! The tool queue of the activity line (TOOL-QUEUE.md): what the model has asked for in the
//! reply at hand, and how far it is.
//!
//! * Several tool calls in ONE reply are a batch whose size is known as soon as each call
//!   starts to stream: `reading 4 files 2/4 ✓✓●·` (done, failed `✗`, running, pending).
//! * One call per reply is a sequence whose length nobody knows. No total is invented: the
//!   line counts what is behind it (`read cli.rs · 2 files read so far`).
//!
//! Pure: read from the transcript blocks (the reducer already keeps one `ToolBlock` per call
//! from its first streamed byte). Plain text: the activity line colours it.
use super::glyphs;
use crate::{Activity, Block, Skin, ToolBlock, ToolState};

/// The tool calls of the reply at hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBatch {
    /// The state of every call of the reply, in the order the model wrote them.
    pub states: Vec<ToolState>,
    /// The tool's name when every call of the reply is the same tool.
    pub name: Option<String>,
    /// The file of the call that runs (or, when none runs, of the last call), shortened.
    pub target: Option<String>,
    /// Finished calls of the same tool earlier in this run (since the user's message), not
    /// counting this reply's.
    pub before: usize,
}

impl ToolBatch {
    pub fn total(&self) -> usize {
        self.states.len()
    }
    /// Calls that are over, failed ones included.
    pub fn finished(&self) -> usize {
        self.states
            .iter()
            .filter(|s| matches!(s, ToolState::Done | ToolState::Error))
            .count()
    }
    pub fn failed(&self) -> usize {
        self.states.iter().filter(|s| **s == ToolState::Error).count()
    }
}

/// The last path segment of a call's `path` (or `file_path`) argument.
fn target_of(t: &ToolBlock) -> Option<String> {
    let path = t
        .args
        .get("path")
        .or_else(|| t.args.get("file_path"))
        .and_then(|v| v.as_str())?;
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

/// The batch of the reply at hand: the tool blocks after the last assistant block (notices in
/// between are passed over). `None` when the transcript does not end in tool calls.
pub fn tool_batch(blocks: &[Block]) -> Option<ToolBatch> {
    let mut calls: Vec<&ToolBlock> = Vec::new();
    let mut rest = blocks.len();
    for (i, b) in blocks.iter().enumerate().rev() {
        match b {
            Block::Tool(t) => calls.push(t),
            Block::Notice { .. } | Block::Custom { .. } => {}
            _ => {
                rest = i;
                break;
            }
        }
        rest = i;
    }
    if calls.is_empty() {
        return None;
    }
    calls.reverse();
    let first = calls[0].name.clone();
    let name = calls.iter().all(|t| t.name == first).then_some(first);
    let current = calls
        .iter()
        .find(|t| t.state == ToolState::Running)
        .or_else(|| calls.last());
    let before = match &name {
        Some(name) => blocks[..rest]
            .iter()
            .rev()
            .take_while(|b| !b.is_user())
            .filter(
                |b| matches!(b, Block::Tool(t) if &t.name == name && matches!(t.state, ToolState::Done | ToolState::Error)),
            )
            .count(),
        None => 0,
    };
    Some(ToolBatch {
        states: calls.iter().map(|t| t.state).collect(),
        name,
        target: current.and_then(|t| target_of(t)),
        before,
    })
}

/// The most calls whose marks are drawn one by one.
const MAX_MARKS: usize = 16;

/// One mark per call: done, failed, running, pending. Unstyled: the detail of the activity
/// line is plain text in one colour (the shapes tell the states apart).
fn marks(states: &[ToolState], skin: Skin) -> String {
    let g = glyphs(skin);
    let ascii = skin == Skin::OxaAscii;
    states
        .iter()
        .map(|s| match s {
            ToolState::Done if ascii => "+",
            ToolState::Done => g.ok,
            ToolState::Error => g.fail,
            ToolState::Running => g.dot,
            ToolState::Streaming | ToolState::Pending if ascii => ".",
            ToolState::Streaming | ToolState::Pending => g.mid,
        })
        .collect()
}

/// `n file` / `n files`.
fn files(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

/// The detail of the activity line for the tool queue, or `None` to keep the default (the
/// tool's name). Only while the reply streams, a tool runs or the batch's results go back.
///
/// * a batch (two or more calls in the reply): `reading 4 files 2/4` + marks; for another
///   tool `bash x4 2/4`; for mixed tools `4 tool calls 2/4`. Shown from the moment the second
///   call starts to stream, so the whole queue is on screen before the first result.
/// * one call: `read cli.rs`, and once earlier replies of the run used the same tool,
///   ` · 2 files read so far` (a count of what is done, never a total).
pub fn tool_queue_detail(blocks: &[Block], activity: &Activity, skin: Skin) -> Option<String> {
    // Thinking right after a batch (its results go back to the model): the batch still shows,
    // until the next reply begins.
    if !matches!(
        activity,
        Activity::Streaming | Activity::Tool(_) | Activity::Thinking
    ) {
        return None;
    }
    let batch = tool_batch(blocks)?;
    let g = glyphs(skin);
    let total = batch.total();
    if total >= 2 {
        let head = match batch.name.as_deref() {
            Some("read") => format!("reading {}", files(total)),
            Some(name) => format!("{name} x{total}"),
            None => format!("{total} tool calls"),
        };
        let mut out = format!("{head} {}/{total}", batch.finished());
        if total <= MAX_MARKS {
            out.push(' ');
            out.push_str(&marks(&batch.states, skin));
        } else if batch.failed() > 0 {
            out.push_str(&format!(" {}{}", g.fail, batch.failed()));
        }
        return Some(out);
    }
    // One call. While it still streams the line says WRITING; nothing to add yet.
    let Activity::Tool(running) = activity else {
        return None;
    };
    let name = batch.name.as_deref().unwrap_or(running);
    let mut out = match &batch.target {
        Some(target) => format!("{name} {target}"),
        None => name.to_string(),
    };
    if batch.before > 0 {
        let so_far = if name == "read" {
            format!("{} read so far", files(batch.before))
        } else {
            format!("{} {name} calls so far", batch.before)
        };
        out.push_str(&format!(" {} {so_far}", g.mid));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, path: &str, state: ToolState) -> Block {
        Block::Tool(ToolBlock {
            call_id: path.to_string(),
            name: name.to_string(),
            args: json!({ "path": path }),
            args_raw: String::new(),
            state,
            partial: String::new(),
            result: None,
            duration_ms: None,
            expanded: false,
        })
    }

    fn assistant() -> Block {
        Block::Assistant {
            text: String::new(),
            thinking: String::new(),
            streaming: false,
            stop: None,
            error: None,
            model: None,
        }
    }

    fn user() -> Block {
        Block::User {
            text: "go".to_string(),
            images: Vec::new(),
        }
    }

    #[test]
    fn a_batch_is_the_calls_of_the_last_reply() {
        use ToolState::*;
        let blocks = vec![
            user(),
            assistant(),
            tool("read", "src/a.rs", Done),
            assistant(),
            tool("read", "src/b.rs", Done),
            tool("read", "src/c.rs", Error),
            tool("read", "src/deep/d.rs", Running),
            tool("read", "e.rs", Pending),
        ];
        let b = tool_batch(&blocks).expect("a batch");
        assert_eq!(b.states, [Done, Error, Running, Pending]);
        assert_eq!((b.total(), b.finished(), b.failed()), (4, 2, 1));
        assert_eq!(b.name.as_deref(), Some("read"));
        assert_eq!(b.target.as_deref(), Some("d.rs"));
        assert_eq!(b.before, 1);
        let line = tool_queue_detail(&blocks, &Activity::Tool("read".into()), Skin::Oxa);
        assert_eq!(line.as_deref(), Some("reading 4 files 2/4 ✓✗●·"));
        let ascii = tool_queue_detail(&blocks, &Activity::Tool("read".into()), Skin::OxaAscii);
        assert_eq!(ascii.as_deref(), Some("reading 4 files 2/4 +x*."));
        assert!(tool_batch(&[user(), assistant()]).is_none());
    }

    #[test]
    fn a_sequence_counts_what_is_behind_it_and_names_no_total() {
        use ToolState::*;
        let act = Activity::Tool("read".into());
        let mut blocks = vec![user(), assistant(), tool("read", "a.rs", Running)];
        assert_eq!(
            tool_queue_detail(&blocks, &act, Skin::Oxa).as_deref(),
            Some("read a.rs")
        );
        blocks[2] = tool("read", "a.rs", Done);
        blocks.extend([assistant(), tool("read", "b.rs", Done), assistant()]);
        blocks.push(tool("read", "src/c.rs", Running));
        let line = tool_queue_detail(&blocks, &act, Skin::Oxa).expect("a detail");
        assert_eq!(line, "read c.rs · 2 files read so far");
        assert!(!line.contains('/'), "no total: {line}");
        // An earlier run's calls are not this run's.
        let mut next = blocks.clone();
        next.extend([user(), assistant(), tool("bash", "", Running)]);
        assert_eq!(
            tool_queue_detail(&next, &Activity::Tool("bash".into()), Skin::Oxa).as_deref(),
            Some("bash")
        );
        // Nothing while idle or thinking.
        assert!(tool_queue_detail(&blocks, &Activity::Thinking, Skin::Oxa).is_none());
    }

    #[test]
    fn mixed_and_large_batches() {
        use ToolState::*;
        let act = Activity::Streaming;
        let blocks = vec![
            assistant(),
            tool("read", "a", Pending),
            tool("grep", "b", Streaming),
        ];
        assert_eq!(
            tool_queue_detail(&blocks, &act, Skin::Oxa).as_deref(),
            Some("2 tool calls 0/2 ··")
        );
        let mut many = vec![assistant()];
        many.extend((0..20).map(|i| tool("bash", "", if i < 3 { Error } else { Pending })));
        assert_eq!(
            tool_queue_detail(&many, &act, Skin::Oxa).as_deref(),
            Some("bash x20 3/20 ✗3")
        );
    }
}
