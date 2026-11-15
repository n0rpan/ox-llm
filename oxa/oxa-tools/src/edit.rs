//! A7: the `edit` tool. Owner: task A7. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/tools/edit.ts
use crate::descriptions::{self, errors};
use crate::diff::{apply_edits_tolerant, Edit};
use oxa_proto::{tool_errors, Access, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Read;

/// Fixed for `path` missing or not a string (not in the frozen text table; see notes).
const INVALID_PATH: &str = "Edit tool input is invalid. path must be a string.";
/// Fixed for an element of `edits` without string `oldText` and `newText`.
fn invalid_edit_item(index: usize) -> String {
    format!("Edit tool input is invalid. edits[{index}] must contain string oldText and newText.")
}

/// `edit {path, edits: [{oldText, newText}]}` (and pi's single-edit legacy arguments): exact replacements through `crate::apply_edits`,
/// falling back to the indentation-tolerant match when the exact one cannot place an edit. BOM and line endings of the file are preserved; lines the edit does not touch keep their endings. Errors use pi's texts (not found, not unique, overlap, no change). `ToolOutput::ui` = `{"diff": <unified diff>, "firstChangedLine": n}`; the text for the model is pi's success line.
#[derive(Debug, Clone)]
pub struct EditTool {
    spec: ToolSpec,
}

impl EditTool {
    pub fn new() -> EditTool {
        EditTool {
            spec: crate::spec_of(&descriptions::EDIT_FLAT),
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

fn is_single_edit(v: &Value) -> bool {
    v.is_object()
        && v.get("oldText").is_some_and(Value::is_string)
        && v.get("newText").is_some_and(Value::is_string)
}

/// The path and the edits of an `edit` call. Accepts pi's forms: `edits` as an array, as a JSON
/// string holding one, or as one edit object; and top-level `oldText`/`newText` appended as a
/// final edit.
fn parse_args(args: &Value) -> Result<(String, Vec<Edit>), String> {
    let obj = args
        .as_object()
        .ok_or_else(|| tool_errors::BAD_JSON.to_string())?;
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| INVALID_PATH.to_string())?;

    let mut items: Vec<Value> = match obj.get("edits") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            Ok(v) if is_single_edit(&v) => vec![v],
            _ => Vec::new(),
        },
        Some(v) if is_single_edit(v) => vec![v.clone()],
        _ => Vec::new(),
    };
    if let (Some(old), Some(new)) = (
        obj.get("oldText").and_then(Value::as_str),
        obj.get("newText").and_then(Value::as_str),
    ) {
        items.push(json!({"oldText": old, "newText": new}));
    }
    if items.is_empty() {
        return Err(errors::EDIT_NO_EDITS.to_string());
    }

    let mut edits = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match (
            item.get("oldText").and_then(Value::as_str),
            item.get("newText").and_then(Value::as_str),
        ) {
            (Some(old), Some(new)) => edits.push(Edit {
                old_text: old.to_string(),
                new_text: new.to_string(),
            }),
            _ => return Err(invalid_edit_item(i)),
        }
    }
    Ok((path.to_string(), edits))
}

/// pi's error cause for an I/O failure: `Error code: X` for the codes pi names, else the message.
fn io_cause(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "Error code: ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "Error code: EACCES".to_string(),
        _ => match e.raw_os_error() {
            Some(21) => "Error code: EISDIR".to_string(),
            Some(20) => "Error code: ENOTDIR".to_string(),
            _ => format!("Error: {e}"),
        },
    }
}

impl Tool for EditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Write(path)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let (path, _) = parse_args(args)?;
        Ok(vec![Access::Write(crate::resolve_path(cx.cwd, &path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let (path, edits) = match parse_args(args) {
            Ok(parsed) => parsed,
            Err(text) => return ToolOutput::error(text),
        };
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(errors::ABORTED);
        }
        let abs = crate::resolve_path(cx.cwd, &path);

        // Opening for read and write checks both, as pi's access(R_OK | W_OK) does, and does
        // not truncate.
        let mut file = match OpenOptions::new().read(true).write(true).open(&abs) {
            Ok(f) => f,
            Err(e) => return ToolOutput::error(errors::edit_could_not(&path, &io_cause(&e))),
        };
        let mut bytes = Vec::new();
        if let Err(e) = file.read_to_end(&mut bytes) {
            return ToolOutput::error(errors::edit_could_not(&path, &io_cause(&e)));
        }
        drop(file);
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(errors::ABORTED);
        }

        let raw = String::from_utf8_lossy(&bytes);
        let matched = apply_edits_tolerant(&raw, &edits, &path);
        let outcome = match matched {
            Ok(outcome) => outcome,
            Err(text) => return ToolOutput::error(text),
        };
        if let Err(e) = std::fs::write(&abs, outcome.content.as_bytes()) {
            return ToolOutput::error(errors::edit_could_not(&path, &io_cause(&e)));
        }

        ToolOutput {
            text: errors::edit_ok(edits.len(), &path),
            is_error: false,
            ui: json!({"diff": outcome.diff, "firstChangedLine": outcome.first_changed_line}),
        }
    }
}
