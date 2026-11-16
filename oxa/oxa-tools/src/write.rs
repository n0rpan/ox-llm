//! A6: the `write` tool.
// ported from pi packages/coding-agent/src/core/tools/write.ts
use crate::descriptions::errors;
use crate::read::arg_path;
use oxa_proto::{Access, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use std::fs::{self, File, OpenOptions};
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
    let existing = match fs::metadata(&target) {
        Ok(m) if m.is_dir() => return Err(format!("Path is a directory: {path}")),
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(write_io_error(path, &e)),
    };
    // A symlink is written through: the link stays, its target gets the content.
    if existing.is_some() {
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

    replace_file(&target, content.as_bytes(), existing.as_ref()).map_err(|e| write_io_error(path, &e))?;
    Ok((content.len(), created))
}

/// Replace `target` atomically: temp file in the same directory, fsync, rename. `existing` is the
/// metadata of the file being replaced, whose mode the new file gets. Nothing is left behind on error.
pub(crate) fn replace_file(target: &Path, content: &[u8], existing: Option<&fs::Metadata>) -> io::Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (tmp, file) = create_temp(parent, &name)?;
    let res = commit(file, &tmp, target, parent, content, existing);
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// The one I/O failure text of `write`.
fn write_io_error(shown: &str, e: &io::Error) -> String {
    format!("Could not write file: {shown}. {e}")
}

/// Write the temp file, give it the old file's mode, fsync, and rename it over the target.
fn commit(
    mut file: File,
    tmp: &Path,
    target: &Path,
    parent: &Path,
    content: &[u8],
    existing: Option<&fs::Metadata>,
) -> io::Result<()> {
    file.write_all(content)?;
    if let Some(meta) = existing {
        keep_mode(tmp, meta)?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, target)?;
    sync_dir(parent);
    Ok(())
}

/// A new file in `dir`, named after `name`, that does not exist yet.
fn create_temp(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let pid = std::process::id();
    for _ in 0..100 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.oxa-tmp-{pid}-{seq}"));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
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
#[cfg(unix)]
fn keep_mode(tmp: &Path, existing: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        tmp,
        fs::Permissions::from_mode(existing.permissions().mode() & 0o7777),
    )
}

#[cfg(not(unix))]
fn keep_mode(_tmp: &Path, _existing: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// Best effort: make the rename durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    let _ = File::open(dir).and_then(|d| d.sync_all());
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}
