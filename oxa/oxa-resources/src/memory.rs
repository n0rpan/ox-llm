//! M1: the memory store. One Markdown file per entry plus a generated index.
//!
//! Layout inside the memory directory (`oxa_config::Dirs::memory_dir`):
//! * `<name>.md`: frontmatter (`kind: fact|rule|lesson`, `description: "..."`), then the body;
//! * `MEMORY.md`: one line per entry, sorted by name, rebuilt on every save and delete;
//! * `.lock`: an exclusive flock held across each write and index rebuild.
//!
//! Names are checked with [`validate_skill_name`]. Bodies are capped at [`MAX_MEMORY_BODY`].
//! Files that do not parse are skipped when the index is built, never fail the call.

use crate::frontmatter::{frontmatter_get, parse_frontmatter};
use crate::skills::validate_skill_name;
use oxa_proto::{codes, OxaError};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Largest body a memory entry may hold, in bytes.
pub const MAX_MEMORY_BODY: usize = 8 * 1024;
/// The generated index file, inside the memory directory.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";
const LOCK_FILE: &str = ".lock";
/// How much of an entry file the index scan reads: the body cap plus room for the frontmatter.
/// Bodies are never kept by the scan, and an oversized file on disk is read only this far.
const SCAN_READ_MAX: u64 = (MAX_MEMORY_BODY as u64) + 4096;

/// What an entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Fact,
    Rule,
    Lesson,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Rule => "rule",
            MemoryKind::Lesson => "lesson",
        }
    }

    /// The kind named by a frontmatter value, `None` for anything else.
    pub fn parse(s: &str) -> Option<MemoryKind> {
        match s {
            "fact" => Some(MemoryKind::Fact),
            "rule" => Some(MemoryKind::Rule),
            "lesson" => Some(MemoryKind::Lesson),
            _ => None,
        }
    }
}

/// One stored entry, as [`read`] returns it. `text` is the body exactly as saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub name: String,
    pub kind: MemoryKind,
    pub description: String,
    pub text: String,
}

/// Write (or replace) entry `name`, then rebuild the index, under the directory lock.
///
/// The entry write is the commit. `MEMORY.md` is derived and [`index`] is built from the entries
/// on disk, so when only the index file cannot be written the save still succeeds; the index is
/// rewritten by the next save or delete (also a delete of an absent name).
///
/// The description is stored on one line (whitespace runs collapse to one space). Refused when
/// the name is invalid or the body is longer than [`MAX_MEMORY_BODY`]; nothing is written then.
pub fn save(dir: &Path, name: &str, kind: MemoryKind, description: &str, text: &str) -> Result<(), OxaError> {
    check_name(name)?;
    if text.len() > MAX_MEMORY_BODY {
        return Err(OxaError::new(
            codes::INVALID,
            format!(
                "memory {name}: body is {} bytes, the limit is {MAX_MEMORY_BODY}",
                text.len()
            ),
        ));
    }
    let description = one_line(description);
    ensure_private_dir(dir)?;
    let _lock = lock(dir)?;
    let doc = format!(
        "---\nkind: {}\ndescription: {}\n---\n{text}",
        kind.as_str(),
        quote(&description)
    );
    write_replace(&entry_path(dir, name), doc.as_bytes())?;
    let _ = rebuild_index(dir);
    Ok(())
}

/// The entry `name`, `None` when there is no such file. An invalid name is an error.
pub fn read(dir: &Path, name: &str) -> Result<Option<MemoryEntry>, OxaError> {
    check_name(name)?;
    let path = entry_path(dir, name);
    match std::fs::read_to_string(&path) {
        Ok(src) => parse_entry(name, &src).map(Some).ok_or_else(|| {
            OxaError::new(
                codes::INVALID,
                format!("memory {name}: frontmatter has no valid kind"),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OxaError::io(&format!("read {}", path.display()), &e)),
    }
}

/// Remove entry `name` and rebuild the index. `Ok(false)` when there was no such entry. As with
/// [`save`], a failed index rewrite does not hide the committed removal.
pub fn delete(dir: &Path, name: &str) -> Result<bool, OxaError> {
    check_name(name)?;
    if !dir.is_dir() {
        return Ok(false);
    }
    let _lock = lock(dir)?;
    match std::fs::remove_file(entry_path(dir, name)) {
        Ok(()) => {
            let _ = rebuild_index(dir);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Nothing to remove, but repair a stale index left by an earlier failed rebuild.
            let _ = rebuild_index(dir);
            Ok(false)
        }
        Err(e) => Err(OxaError::io(&format!("delete memory {name}"), &e)),
    }
}

/// The index text built from the entries on disk: one line per entry, sorted by name. When it
/// is longer than `max_bytes`, as many whole lines as fit are kept, followed by `(+N more)`.
/// A missing directory gives the empty string.
pub fn index(dir: &Path, max_bytes: usize) -> String {
    match scan(dir) {
        Ok(entries) => render_index(&entries, max_bytes),
        Err(_) => String::new(),
    }
}

fn check_name(name: &str) -> Result<(), OxaError> {
    let problems = validate_skill_name(name);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(OxaError::new(
            codes::INVALID,
            format!("memory name {name:?}: {}", problems.join("; ")),
        ))
    }
}

fn entry_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.md"))
}

/// The directory lock; released when the returned file is dropped.
fn lock(dir: &Path) -> Result<File, OxaError> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| OxaError::io(&format!("open {}", path.display()), &e))?;
    file.lock()
        .map_err(|e| OxaError::io(&format!("lock {}", path.display()), &e))?;
    Ok(file)
}

/// The memory directory, owner-only (0700), created when missing.
fn ensure_private_dir(dir: &Path) -> Result<(), OxaError> {
    let io = |e: std::io::Error| OxaError::io(&format!("create {}", dir.display()), &e);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)
}

/// Write `bytes` to `path` through an owner-only (0600) temporary sibling that is synced and then
/// renamed over `path`, so a crash never leaves a half-written entry and no mode is ever looser.
fn write_replace(path: &Path, bytes: &[u8]) -> Result<(), OxaError> {
    let err = |e: std::io::Error| OxaError::io(&format!("write {}", path.display()), &e);
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err(e));
    }
    if let Some(parent) = path.parent() {
        let _ = File::open(parent).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Regenerate `MEMORY.md` from the entries on disk (uncapped). Callers hold the lock.
fn rebuild_index(dir: &Path) -> Result<(), OxaError> {
    let entries = scan(dir)?;
    let text = render_index(&entries, usize::MAX);
    write_replace(&dir.join(MEMORY_INDEX_FILE), text.as_bytes())
}

/// The metadata of one entry, as the index needs it (no body).
struct MemoryHead {
    name: String,
    kind: MemoryKind,
    description: String,
}

/// At most `max` bytes of `path`, as text (a cut multi-byte character becomes U+FFFD).
fn read_prefix(path: &Path, max: u64) -> std::io::Result<String> {
    let mut buf = Vec::new();
    File::open(path)?.take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Every well-formed entry in `dir`, sorted by name. A missing directory is empty. Each file is
/// read only up to [`SCAN_READ_MAX`] bytes and bodies are not retained.
fn scan(dir: &Path) -> Result<Vec<MemoryHead>, OxaError> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(OxaError::io(&format!("read {}", dir.display()), &e)),
    };
    let mut entries = Vec::new();
    for item in read_dir.flatten() {
        let file_name = item.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(name) = file_name.strip_suffix(".md") else {
            continue;
        };
        if !validate_skill_name(name).is_empty() {
            continue;
        }
        let Ok(src) = read_prefix(&item.path(), SCAN_READ_MAX) else {
            continue;
        };
        if let Some(head) = parse_head(name, &src) {
            entries.push(head);
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn parse_head(name: &str, src: &str) -> Option<MemoryHead> {
    let (pairs, _body) = parse_frontmatter(src);
    let kind = MemoryKind::parse(frontmatter_get(&pairs, "kind")?)?;
    Some(MemoryHead {
        name: name.to_string(),
        kind,
        description: frontmatter_get(&pairs, "description")
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_entry(name: &str, src: &str) -> Option<MemoryEntry> {
    let (pairs, body) = parse_frontmatter(src);
    let kind = MemoryKind::parse(frontmatter_get(&pairs, "kind")?)?;
    Some(MemoryEntry {
        name: name.to_string(),
        kind,
        description: frontmatter_get(&pairs, "description")
            .unwrap_or_default()
            .to_string(),
        text: body.to_string(),
    })
}

fn render_index(entries: &[MemoryHead], max_bytes: usize) -> String {
    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "- [{name}]({name}.md) ({kind}): {description}",
                name = e.name,
                kind = e.kind.as_str(),
                description = e.description
            )
        })
        .collect();
    let total: usize = lines.iter().map(|l| l.len() + 1).sum();
    if total <= max_bytes {
        return lines.iter().map(|l| format!("{l}\n")).collect();
    }
    for keep in (0..lines.len()).rev() {
        let kept: usize = lines[..keep].iter().map(|l| l.len() + 1).sum();
        let marker = format!("(+{} more)\n", lines.len() - keep);
        if kept + marker.len() <= max_bytes {
            let mut out: String = lines[..keep].iter().map(|l| format!("{l}\n")).collect();
            out.push_str(&marker);
            return out;
        }
    }
    String::new()
}

/// Whitespace runs collapsed to single spaces, so the description fits on one index line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A double-quoted YAML-style scalar that [`parse_frontmatter`] reads back unchanged.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
