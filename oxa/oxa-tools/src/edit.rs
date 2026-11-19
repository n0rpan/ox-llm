//! A7: the `edit` tool.
// ported from pi packages/coding-agent/src/core/tools/edit.ts
use crate::descriptions::{self, errors};
use crate::diff::{apply_edits_tolerant, Edit};
use oxa_proto::{tool_errors, Access, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

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

/// Replace the whole content of the open `file` (what `fs::write` does to a name).
fn rewrite(file: &mut std::fs::File, content: &[u8]) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(content)
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

/// Save atomically (temp + fsync + rename, mode kept) when replacing the inode is safe.
/// Symlinks are written through. Preserve the original inode for shared ownership, hard links or
/// extended metadata, and when the directory disallows replacement; these edits remain in-place.
fn save(target: &std::path::Path, file: &mut std::fs::File, content: &[u8]) -> std::io::Result<()> {
    save_with(target, file, content, crate::write::replace_file)
}

fn save_with(
    target: &std::path::Path,
    file: &mut std::fs::File,
    content: &[u8],
    replace: impl FnOnce(
        &std::fs::File,
        &std::ffi::OsStr,
        &[u8],
        Option<&std::fs::Metadata>,
    ) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let meta = file.metadata()?;
    if requires_in_place(file, &meta) {
        return rewrite(file, content);
    }
    let parent = target.parent().unwrap_or_else(|| std::path::Path::new("."));
    let result = (|| {
        let dir = std::fs::File::open(parent)?;
        crate::checked::verify_at(parent, &dir)?;
        let name = target.file_name().unwrap_or_default();
        replace(&dir, name, content, Some(&meta))
    })();
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => rewrite(file, content),
        other => other,
    }
}

#[cfg(unix)]
fn must_keep_inode(meta: &std::fs::Metadata, uid: u32, gid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1 || meta.uid() != uid || meta.gid() != gid
}

#[cfg(unix)]
fn requires_in_place(file: &std::fs::File, meta: &std::fs::Metadata) -> bool {
    // SAFETY: these calls have no arguments or memory side effects.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    must_keep_inode(meta, uid, gid) || has_extended_metadata(file)
}

#[cfg(not(unix))]
fn requires_in_place(_file: &std::fs::File, _meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
fn has_extended_metadata(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // A size-only query catches ACLs, security labels and user attributes without reading them.
    // SAFETY: fd is open; a null buffer with length zero requests only the list's size.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    if size < 0 {
        // Unsupported xattrs mean there are none; any other uncertainty keeps the inode.
        !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOTSUP | libc::ENOSYS)
        )
    } else {
        size > 0
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
#[cfg(unix)]
fn has_extended_metadata(_file: &std::fs::File) -> bool {
    // Without a metadata query, do not risk dropping attributes on a fresh inode.
    true
}

impl Tool for EditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Write(path)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let (path, _) = parse_args(args)?;
        Ok(vec![Access::Write(crate::resolve_tool_path(cx, &path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let (path, edits) = match parse_args(args) {
            Ok(parsed) => parsed,
            Err(text) => return ToolOutput::error(text),
        };
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(errors::ABORTED);
        }
        let abs = crate::resolve_tool_path(cx, &path);

        // Resolve once, using the permission check's pin when present. Opening for read and
        // write checks both without truncating. All in-place writes use this verified handle;
        // atomic replacement uses an opened and verified parent directory instead.
        let target = cx
            .checked_real(&abs)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone()));
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&abs)
            .and_then(|f| crate::checked::verify_at(&target, &f).map(|()| f));
        let mut file = match opened {
            Ok(f) => f,
            Err(e) => return ToolOutput::error(errors::edit_could_not(&path, &io_cause(&e))),
        };
        let mut bytes = Vec::new();
        if let Err(e) = file.read_to_end(&mut bytes) {
            return ToolOutput::error(errors::edit_could_not(&path, &io_cause(&e)));
        }
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(errors::ABORTED);
        }

        let raw = String::from_utf8_lossy(&bytes);
        let matched = apply_edits_tolerant(&raw, &edits, &path);
        let outcome = match matched {
            Ok(outcome) => outcome,
            Err(text) => return ToolOutput::error(text),
        };
        if let Err(e) = save(&target, &mut file, outcome.content.as_bytes()) {
            return ToolOutput::error(errors::edit_could_not(&path, &io_cause(&e)));
        }

        ToolOutput {
            images: Vec::new(),
            text: errors::edit_ok(edits.len(), &path),
            is_error: false,
            ui: json!({"diff": outcome.diff, "firstChangedLine": outcome.first_changed_line}),
        }
    }
}

#[cfg(all(test, unix))]
mod save_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn atomic_save_refuses_a_parent_swapped_after_the_file_was_opened() {
        let env = oxa_fake::TestEnv::new();
        let path = env.write("work/dir/file", "before");
        let outside = env.write("outside/file", "unrelated");
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        std::fs::rename(env.work().join("dir"), env.work().join("saved")).unwrap();
        std::os::unix::fs::symlink(outside.parent().unwrap(), env.work().join("dir")).unwrap();
        let error = save(&path, &mut file, b"after").unwrap_err();
        assert!(error.to_string().contains("changed after the permission check"));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "unrelated");
        assert_eq!(
            std::fs::read_to_string(env.work().join("saved/file")).unwrap(),
            "before"
        );
    }

    #[test]
    fn replacement_fallback_writes_the_open_file_not_a_swapped_leaf() {
        let env = oxa_fake::TestEnv::new();
        let path = env.write("work/file", "before");
        let outside = env.write("outside/file", "unrelated");
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let original = env.work().join("original");
        std::fs::rename(&path, &original).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let mut attempted = false;
        save_with(&path, &mut file, b"after", |_, _, _, _| {
            attempted = true;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap();
        assert!(attempted);
        assert_eq!(std::fs::read_to_string(original).unwrap(), "after");
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "unrelated");
    }

    #[test]
    fn replacement_permission_denied_falls_back_even_as_root() {
        let env = oxa_fake::TestEnv::new();
        let path = env.write("work/file", "before");
        let before = std::fs::metadata(&path).unwrap();
        let mut attempted = false;
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        save_with(&path, &mut file, b"after", |_, _, _, _| {
            attempted = true;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap();
        assert!(attempted, "must exercise the replacement failure path");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
        assert_eq!(std::fs::metadata(path).unwrap().ino(), before.ino());
    }

    #[test]
    fn replacement_enospc_does_not_fall_back_or_truncate() {
        let env = oxa_fake::TestEnv::new();
        let path = env.write("work/file", "before");
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let error = save_with(&path, &mut file, b"after", |_, _, _, _| {
            Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
    }

    #[test]
    fn replacement_requires_the_same_owner_and_group() {
        let env = oxa_fake::TestEnv::new();
        let path = env.write("work/file", "before");
        let meta = std::fs::metadata(path).unwrap();
        assert!(!must_keep_inode(&meta, meta.uid(), meta.gid()));
        assert!(must_keep_inode(&meta, meta.uid().wrapping_add(1), meta.gid()));
        assert!(must_keep_inode(&meta, meta.uid(), meta.gid().wrapping_add(1)));
    }
}
