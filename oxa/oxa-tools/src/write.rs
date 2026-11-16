//! A6: the `write` tool.
// ported from pi packages/coding-agent/src/core/tools/write.ts
use crate::checked;
use crate::descriptions::errors;
use crate::read::arg_path;
use oxa_proto::{Access, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Sequence number of temporary file names, unique within the process.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `write {path, content}`: create or overwrite, creating parent directories. Atomic: write a temp file in the same directory, fsync, rename; an existing file's mode is kept. `ToolOutput::ui` = `{"bytes": n, "created": bool}`.
#[derive(Debug, Clone)]
pub struct WriteTool {
    spec: ToolSpec,
}

impl WriteTool {
    pub fn new() -> WriteTool {
        WriteTool {
            spec: crate::spec_of(&crate::descriptions::builtin_specs::WRITE_DEFAULT),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WriteTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Write(path)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let path = arg_path(args)?;
        Ok(vec![Access::Write(crate::resolve_path(cx.cwd, path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = match arg_path(args) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        match write_file(args, cx) {
            Ok((bytes, created)) => ToolOutput {
                text: errors::write_ok(path),
                is_error: false,
                ui: serde_json::json!({ "bytes": bytes, "created": created }),
            },
            Err(text) => ToolOutput::error(text),
        }
    }
}

/// Write the content. `Ok((bytes, created))`; `Err` is the tool error text.
fn write_file(args: &Value, cx: &ToolCtx) -> Result<(usize, bool), String> {
    let path = arg_path(args)?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid arguments: content must be a string".to_string())?;
    if cx.cancel.is_cancelled() {
        return Err(errors::ABORTED.to_string());
    }

    let mut target = crate::resolve_path(cx.cwd, path);
    // #518: the location the permission check judged (symlinks resolved): written there or not
    // at all, whatever a link points at by now.
    let pin = cx.checked_real(&target).map(Path::to_path_buf);
    if let Some(real) = &pin {
        target = real.clone();
    }
    let existing = match fs::metadata(&target) {
        Ok(m) if m.is_dir() => return Err(format!("Path is a directory: {path}")),
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(write_io_error(path, &e)),
    };
    // A symlink is written through: the link stays, its target gets the content.
    if pin.is_none() && existing.is_some() {
        if let Ok(real) = fs::canonicalize(&target) {
            target = real;
        }
    }
    let created = existing.is_none();

    let parent: PathBuf = match target.parent() {
        Some(p) => p.to_path_buf(),
        None => return Err(format!("Path has no parent directory: {path}")),
    };
    fs::create_dir_all(&parent).map_err(|e| write_io_error(path, &e))?;
    // The temp file and the rename go through this directory, opened once and checked: a
    // directory swapped for a link later does not move the write.
    let dir = File::open(&parent).map_err(|e| write_io_error(path, &e))?;
    if pin.is_some() {
        checked::verify_at(&parent, &dir).map_err(|e| write_io_error(path, &e))?;
    }

    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (tmp, file) = create_temp(&dir, &name).map_err(|e| write_io_error(path, &e))?;
    match commit(file, &dir, &tmp, &name, content, existing.as_ref()) {
        Ok(()) => Ok((content.len(), created)),
        Err(e) => {
            checked::remove_in(&dir, &tmp);
            Err(write_io_error(path, &e))
        }
    }
}

/// The one I/O failure text of `write`.
fn write_io_error(shown: &str, e: &io::Error) -> String {
    format!("Could not write file: {shown}. {e}")
}

/// Write the temp file, give it the old file's mode, fsync, and rename it over the target, both
/// names in `dir`.
fn commit(
    mut file: File,
    dir: &File,
    tmp: &str,
    name: &str,
    content: &str,
    existing: Option<&fs::Metadata>,
) -> io::Result<()> {
    file.write_all(content.as_bytes())?;
    if let Some(meta) = existing {
        keep_mode(&file, meta)?;
    }
    file.sync_all()?;
    drop(file);
    checked::rename_in(dir, tmp, name)?;
    sync_dir(dir);
    Ok(())
}

/// A new file in `dir`, named after `name`, that does not exist yet; returns its name.
fn create_temp(dir: &File, name: &str) -> io::Result<(String, File)> {
    let pid = std::process::id();
    for _ in 0..100 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = format!(".{name}.oxa-tmp-{pid}-{seq}");
        match checked::create_new_in(dir, &tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free temporary file name",
    ))
}

/// The permission bits of the existing file, on the temp file (set explicitly: the umask does not
/// apply to it).
fn keep_mode(tmp: &File, existing: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tmp.set_permissions(fs::Permissions::from_mode(existing.permissions().mode() & 0o7777))
}

/// Best effort: make the rename durable.
fn sync_dir(dir: &File) {
    let _ = dir.sync_all();
}
