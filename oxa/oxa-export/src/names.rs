//! E12: derived export file names. Owner: task E12. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/export-html/index.ts (output path derivation)
use std::path::{Path, PathBuf};

/// Export format, chosen by the output extension (`/export out.jsonl`) or the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Html,
    Jsonl,
}

/// The format for a user-supplied output path: `.jsonl` = `Jsonl`, everything else = `Html`.
pub fn format_for(out: &Path) -> ExportFormat {
    match out.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("jsonl") => ExportFormat::Jsonl,
        _ => ExportFormat::Html,
    }
}

/// pi's default output name when the user gives none: `<app>-session-<session file stem>.html`
/// (app name = `oxa_proto::names::APP`), placed in `cwd`. `session_file` is the session's JSONL
/// path; an ephemeral session (`None`) uses the session id as the stem.
pub fn default_output_name(session_file: Option<&Path>, session_id: &str, cwd: &Path) -> PathBuf {
    // pi: basename(sessionFile, ".jsonl"); the suffix is removed only when present.
    let stem = match session_file.and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
        Some(name) => name.strip_suffix(".jsonl").unwrap_or(name).to_string(),
        None => session_id.to_string(),
    };
    cwd.join(format!("{}-session-{}.html", oxa_proto::names::APP, stem))
}
