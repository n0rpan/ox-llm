//! B1: cumulative file lists of a branch.
// ported from pi packages/coding-agent/src/core/compaction/utils.ts (file operations)
use std::collections::BTreeSet;

use oxa_proto::{Entry, EntryBody, Role, Value};

/// `(read_files, modified_files)` over a branch: paths from `read` tool calls, and from
/// `write` / `edit` tool calls, in the assistant messages of `branch`, plus the lists carried
/// in the `details` of earlier `Compaction` and `BranchSummary` entries (`readFiles`,
/// `modifiedFiles`), so lists accumulate across compactions. A file both read and modified is
/// only in `modified`. Each list sorted and de-duplicated.
pub fn file_lists(branch: &[&Entry]) -> (Vec<String>, Vec<String>) {
    let mut read: BTreeSet<String> = BTreeSet::new();
    let mut modified: BTreeSet<String> = BTreeSet::new();
    for e in branch {
        match &e.body {
            EntryBody::Msg { wire, .. } if wire.role == Role::Assistant => {
                for call in &wire.tool_calls {
                    let Some(path) = path_arg(&call.args_raw) else {
                        continue;
                    };
                    match call.name.as_str() {
                        "read" => {
                            read.insert(path);
                        }
                        "write" | "edit" => {
                            modified.insert(path);
                        }
                        _ => {}
                    }
                }
            }
            // pi ignores the details of hook-generated summaries (`fromHook`).
            EntryBody::Compaction {
                details,
                from_hook: false,
                ..
            }
            | EntryBody::BranchSummary {
                details,
                from_hook: false,
                ..
            } => {
                add_strings(details, "readFiles", &mut read);
                add_strings(details, "modifiedFiles", &mut modified);
            }
            _ => {}
        }
    }
    let read_only: Vec<String> = read.iter().filter(|f| !modified.contains(*f)).cloned().collect();
    let modified: Vec<String> = modified.into_iter().collect();
    (read_only, modified)
}

/// The `path` argument of a tool call, when the arguments parse as a JSON object with a
/// non-empty string `path`. Arguments are parsed on a copy (W2: `args_raw` is never changed).
fn path_arg(args_raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args_raw).ok()?;
    let path = v.get("path")?.as_str()?;
    (!path.is_empty()).then(|| path.to_owned())
}

/// Add the string items of `details[key]` (an array) to `into`.
fn add_strings(details: &Value, key: &str, into: &mut BTreeSet<String>) {
    if let Some(items) = details.get(key).and_then(Value::as_array) {
        for item in items {
            if let Some(s) = item.as_str() {
                into.insert(s.to_owned());
            }
        }
    }
}
