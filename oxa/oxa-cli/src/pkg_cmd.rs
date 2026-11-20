//! E10: `oxa install|remove|update|list`.
// ported from pi packages/coding-agent/src/package-manager-cli.ts (docs: packages.md)
use oxa_config::{Dirs, Settings};
use oxa_proto::{codes, OxaError};
use oxa_resources::{declared_packages, parse_source, PackageManager, PackageSource, Scope, SystemRunner};
use serde_json::Value;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Run a package subcommand: `install <source> [-l]`, `remove <source> [-l]`, `update
/// [source]`, `list`. Sources: `git:` / https git URLs and local paths as pi; `npm:` sources,
/// self-update and version checks are `unsupported_in_oxa`. Returns the exit code.
pub fn run_pkg(
    verb: &str,
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let mut local = false;
    let mut positional: Vec<&str> = Vec::new();
    for w in words {
        match w.as_str() {
            "-l" | "--local" => local = true,
            // pi's `update` options (cli.md): `--self` / `--all` include the self-update and
            // `--models` refreshes model catalogs, neither of which oxa has (CLI-054, CLI-056);
            // `--extensions` is the package update.
            "--self" | "--all" if verb == "update" => {
                return Err(
                    OxaError::unsupported(&format!("updating oxa itself (`oxa update {w}`)")).with_hint(
                        "Update oxa by installing a new build; `oxa update` only updates packages.",
                    ),
                );
            }
            "--models" if verb == "update" => {
                return Err(
                    OxaError::unsupported("updating model catalogs (`oxa update --models`)")
                        .with_hint("oxa uses the engine's model list; there is no catalog to refresh."),
                );
            }
            "--extensions" if verb == "update" => {}
            s if s.starts_with('-') => {
                return Err(usage(format!("unknown option for `oxa {verb}`: {s}")));
            }
            s => positional.push(s),
        }
    }
    let verb = if verb == "uninstall" { "remove" } else { verb };
    let (min, max) = match verb {
        "install" | "remove" => (1, 1),
        "update" => (0, 1),
        "list" => (0, 0),
        other => return Err(usage(format!("unknown package command: {other}"))),
    };
    if positional.len() < min || positional.len() > max {
        return Err(usage(format!(
            "`oxa {verb}` takes {}",
            match (min, max) {
                (1, 1) => "one source",
                (0, 1) => "an optional source",
                _ => "no arguments",
            }
        )));
    }
    let scope = if local { Scope::Project } else { Scope::User };
    let trusted = crate::approval::saved_project_resources(dirs, cwd, err)?;
    let mut settings = Settings::load(dirs, cwd, trusted);
    let mgr = PackageManager::new(dirs, cwd, Box::new(SystemRunner));

    match verb {
        "install" => {
            let expanded = expand_home(positional[0], &dirs.home);
            let src = expanded.as_str();
            let stored = match parse_source(src, cwd)? {
                PackageSource::Local(path) => normalize(&path).display().to_string(),
                _ => src.trim().to_string(),
            };
            let before = read_declarations(dirs, cwd, scope);
            let info = mgr.install(&stored, scope)?;
            let already = before.iter().any(|e| {
                entry_identity(e, dirs, cwd, scope).as_deref() == Some(info.source.identity().as_str())
            });
            if !already {
                write_declarations(dirs, cwd, &mut settings, scope, |items| {
                    items.push(Value::String(stored.clone()));
                })?;
            }
            let note = if already { " (already declared)" } else { "" };
            writeln!(out, "Installed {stored} ({}){note}", scope_label(scope))
                .map_err(|e| OxaError::io("write output", &e))?;
            Ok(0)
        }
        "remove" => {
            let expanded = expand_home(positional[0], &dirs.home);
            let src = expanded.as_str();
            let identity = parse_source(src, cwd)?.identity();
            let removed_files = mgr.remove(src, scope)?;
            let mut declared = false;
            write_declarations(dirs, cwd, &mut settings, scope, |items| {
                let before = items.len();
                items.retain(|e| entry_identity(e, dirs, cwd, scope).as_deref() != Some(identity.as_str()));
                declared = items.len() != before;
            })?;
            if !declared && !removed_files {
                return Err(OxaError::new(
                    codes::NOT_FOUND,
                    format!(
                        "{src} is not installed or declared in {} settings",
                        scope_label(scope)
                    ),
                ));
            }
            writeln!(out, "Removed {src} ({})", scope_label(scope))
                .map_err(|e| OxaError::io("write output", &e))?;
            Ok(0)
        }
        "list" => {
            let decls = declared_packages(dirs, cwd, &settings, trusted);
            // The loaded trust state already tells us project declarations are excluded.
            // Do not probe untrusted settings just to discover whether any exist: even a
            // reporting-only read could follow a symlink, block on a FIFO or allocate freely.
            let hidden_project = !trusted;
            if hidden_project {
                writeln!(
                    out,
                    "Project package declarations, if any, are hidden: the project is not trusted (run oxa once in it to decide)."
                )
                .map_err(|e| OxaError::io("write output", &e))?;
            }
            if decls.is_empty() && !hidden_project {
                writeln!(out, "No packages declared.").map_err(|e| OxaError::io("write output", &e))?;
                return Ok(0);
            }
            for info in mgr.list(&decls) {
                let state = if info.installed {
                    format!("installed at {}", info.root.display())
                } else {
                    "not installed (run `oxa update` or `oxa install`)".to_string()
                };
                writeln!(out, "{} {} {state}", scope_label(info.scope), info.decl.source)
                    .map_err(|e| OxaError::io("write output", &e))?;
            }
            Ok(0)
        }
        _ => {
            // update [source]
            let mut decls = declared_packages(dirs, cwd, &settings, trusted);
            if local {
                decls.retain(|(s, _)| *s == Scope::Project);
            }
            if let Some(word) = positional.first() {
                // Parsing first: `update self` and `update pi` are refused with the error code.
                let src = expand_home(word, &dirs.home);
                let identity = parse_source(&src, cwd)?.identity();
                decls.retain(|(s, d)| {
                    decl_identity(&d.source, dirs, cwd, *s).as_deref() == Some(identity.as_str())
                });
                if decls.is_empty() {
                    return Err(OxaError::new(
                        codes::NOT_FOUND,
                        format!("{word} is not declared in any settings file"),
                    ));
                }
            }
            let (lines, diags) = mgr.update(&decls);
            for line in &lines {
                writeln!(out, "{line}").map_err(|e| OxaError::io("write output", &e))?;
            }
            if lines.is_empty() && diags.is_empty() {
                writeln!(out, "All packages are up to date.")
                    .map_err(|e| OxaError::io("write output", &e))?;
            }
            let mut failed = false;
            for d in &diags {
                let label = match d.level {
                    oxa_proto::Level::Error => {
                        failed = true;
                        "error"
                    }
                    _ => "warning",
                };
                writeln!(err, "{label}: {}", d.message).map_err(|e| OxaError::io("write output", &e))?;
            }
            Ok(if failed { 1 } else { 0 })
        }
    }
}

fn usage(message: String) -> OxaError {
    OxaError::new(codes::USAGE, message).with_hint("Run `oxa --help` for the list of options.")
}

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
    }
}

/// The settings file of a scope.
fn settings_path(dirs: &Dirs, cwd: &Path, scope: Scope) -> PathBuf {
    match scope {
        Scope::User => dirs.settings_file(),
        Scope::Project => dirs.project_settings_file(cwd),
    }
}

/// The raw `packages` array of one settings file (not merged: each file keeps its own list).
fn read_declarations(dirs: &Dirs, cwd: &Path, scope: Scope) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(settings_path(dirs, cwd, scope)) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(root)) => root
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The identity of one `packages` entry (string or object with `source`).
fn entry_identity(entry: &Value, dirs: &Dirs, cwd: &Path, scope: Scope) -> Option<String> {
    let source = match entry {
        Value::String(s) => s.as_str(),
        Value::Object(o) => o.get("source")?.as_str()?,
        _ => return None,
    };
    decl_identity(source, dirs, cwd, scope)
}

/// The identity of a declared source, relative local paths resolved from the scope's directory.
fn decl_identity(source: &str, dirs: &Dirs, cwd: &Path, scope: Scope) -> Option<String> {
    let base = match scope {
        Scope::User => dirs.agent_dir.clone(),
        Scope::Project => cwd.join(oxa_proto::names::PROJECT_DIR),
    };
    parse_source(source, &base).ok().map(|p| p.identity())
}

/// Rewrite the `packages` array of one settings file through `oxa-config`, which keeps the other
/// keys of that file and its order. `edit` gets the file's current entries.
fn write_declarations(
    dirs: &Dirs,
    cwd: &Path,
    settings: &mut Settings,
    scope: Scope,
    edit: impl FnOnce(&mut Vec<Value>),
) -> Result<(), OxaError> {
    let mut items = read_declarations(dirs, cwd, scope);
    edit(&mut items);
    settings.set(
        dirs,
        cwd,
        "packages",
        Value::Array(items),
        scope == Scope::Project,
    )
}

/// A leading `~/` is the user's home directory (the parser has no `Dirs`).
fn expand_home(src: &str, home: &Path) -> String {
    match src.strip_prefix("~/") {
        Some(rest) => home.join(rest).display().to_string(),
        None => src.to_string(),
    }
}

/// `.` and `..` resolved without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
