//! D12: external editor and process suspend. Owner: task D12. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/external-editor.ts
use oxa_proto::OxaError;
use oxa_tui::Terminal;
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The editor command from the environment: `$VISUAL`, then `$EDITOR`, else `None` (the caller
/// shows pi's hint). The value may contain arguments (`code --wait`).
pub fn resolve_editor(env: &[(String, String)]) -> Option<String> {
    ["VISUAL", "EDITOR"].iter().find_map(|key| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

/// The editor command: the `externalEditor` setting, then `$VISUAL`, then `$EDITOR` (pi
/// settings.md `externalEditor`). A blank setting counts as unset.
pub fn resolve_editor_with(setting: Option<&str>, env: &[(String, String)]) -> Option<String> {
    setting
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| resolve_editor(env))
}

/// Edit `text` in the external editor (the Ctrl+G round trip).
///
/// Writes `text` to a new 0600 file in `tmp_dir` (`oxa-editor-<random>.md`), suspends the
/// terminal (`Terminal::suspend`), runs `editor_cmd <file>` through `sh -c` with inherited
/// stdio, resumes the terminal, reads the file back and deletes it. Returns `Ok(Some(new
/// text))` (one trailing newline removed) when the editor exited 0, `Ok(None)` when it exited
/// non-zero (text unchanged). The terminal is resumed on every path, also on error. Errors:
/// `io`. The caller resets the renderer afterwards.
pub fn edit_externally(
    text: &str,
    editor_cmd: &str,
    tmp_dir: &Path,
    term: &mut dyn Terminal,
) -> Result<Option<String>, OxaError> {
    let (path, mut file) = create_temp_file(tmp_dir)?;
    if let Err(e) = file.write_all(text.as_bytes()).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(OxaError::io("write the external editor file", &e));
    }
    drop(file);

    term.suspend();
    let command = format!("{editor_cmd} {}", shell_quote(&path.to_string_lossy()));
    // `status()` inherits stdin, stdout and stderr: the editor owns the terminal.
    let status = Command::new("sh").arg("-c").arg(&command).status();
    term.resume();

    let result = match status {
        Err(e) => Err(OxaError::io("run the external editor", &e)),
        Ok(st) if st.success() => fs::read_to_string(&path)
            .map(|s| {
                let body = strip_bom(&s);
                Some(body.strip_suffix('\n').unwrap_or(body).to_string())
            })
            .map_err(|e| OxaError::io("read the external editor file", &e)),
        Ok(_) => Ok(None),
    };
    let _ = fs::remove_file(&path);
    result
}

/// Ctrl+Z: suspend the terminal, stop the process group with `SIGTSTP`, and resume the
/// terminal when the process continues. Acceptance (plan D12): suspend/resume restores the
/// screen (the caller resets the renderer and redraws).
pub fn suspend_process(term: &mut dyn Terminal) {
    suspend_with(term, &stop_process_group);
}

/// [`suspend_process`] with the stop action injected (tests pass a closure; the real stop
/// sends `SIGTSTP` to the own process group, which would stop a test runner).
///
/// Raw mode turns off ISIG, so the terminal never raises the signal itself; `stop` must do it.
/// The terminal is resumed after `stop` returns, which is when the process continues.
pub fn suspend_with(term: &mut dyn Terminal, stop: &dyn Fn()) {
    term.suspend();
    stop();
    term.resume();
}

fn stop_process_group() {
    // SAFETY: `kill` only sends a signal; pid 0 addresses the caller's own process group.
    unsafe {
        kill(0, SIGTSTP);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const SIGTSTP: i32 = 20;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SIGTSTP: i32 = 18;

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// POSIX single-quote quoting for the temp path (it may contain spaces).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A new file `oxa-editor-<random>.md` with mode 0600, created exclusively.
fn create_temp_file(dir: &Path) -> Result<(PathBuf, std::fs::File), OxaError> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::create_dir_all(dir).map_err(|e| OxaError::io("create the editor temp directory", &e))?;
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..16u32 {
        let path = dir.join(format!("oxa-editor-{}.md", random_suffix(attempt)));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(OxaError::io("create the editor temp file", &e)),
        }
    }
    Err(OxaError::io(
        "create the editor temp file",
        &last.unwrap_or_else(|| std::io::Error::other("no free name")),
    ))
}

fn random_suffix(attempt: u32) -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.write_u128(nanos);
    h.write_u32(std::process::id());
    h.write_u32(attempt);
    format!("{:016x}", h.finish())
}
