//! E1: extension manifests and the type declarations.
// ported from pi packages/coding-agent/src/core/pi-manifest.ts
use oxa_proto::{codes, OxaError, Value};
use oxa_script::strip_types;
use std::path::{Path, PathBuf};

/// The TypeScript declarations of the extension API, shipped for editors (`oxa.d.ts` at the
/// crate root; E1 writes it from pi's `ExtensionAPI` types, minus the omitted calls).
pub const OXA_D_TS: &str = include_str!("../oxa.d.ts");

/// What an extension declares about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtManifest {
    pub name: String,
    pub description: Option<String>,
    /// Entry files, absolute.
    pub entries: Vec<PathBuf>,
    /// `rebase: true`: this extension may return a non-prefix message list from the
    /// prefix-checked hooks; each such rebase is logged as a miss cause (W-guard).
    pub rebase: bool,
}

/// Resolve a path from `Resources::extension_paths` to a manifest:
///
/// * a `.js` / `.mjs` file: one entry, name = file stem;
/// * a `.ts` file: `Err(unsupported_in_oxa)` from `oxa_script::strip_types`;
/// * a directory: `package.json` key `oxa` (pi's `pi` key accepted) `{extensions: [..],
///   rebase?}`, else `index.js` / `index.mjs`.
///
/// Errors: `not_found` (nothing loadable), `invalid` (bad manifest JSON).
pub fn read_manifest(path: &Path) -> Result<ExtManifest, OxaError> {
    let abs = absolute(path);
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(OxaError::new(
                codes::NOT_FOUND,
                format!("extension not found: {}", abs.display()),
            ))
        }
        Err(e) => return Err(OxaError::io(&format!("read {}", abs.display()), &e)),
    };
    if meta.is_dir() {
        return read_package(&abs);
    }
    match extension_kind(&abs) {
        Kind::Script => Ok(file_manifest(&abs)),
        Kind::TypeScript => Err(typescript_refused(&abs)),
        Kind::Other => Err(OxaError::new(
            codes::INVALID,
            format!("not an extension file: {}", abs.display()),
        )
        .with_hint(
            "extensions are .js or .mjs files, or directories with a package.json `oxa` key or an index.js",
        )),
    }
}

/// A single-file extension declares options with `export const manifest = { name?, rebase? }`.
/// Merge that export (already bridged to JSON; `Value::Null` when absent) into `base`.
pub fn merge_exported_manifest(base: ExtManifest, exported: &Value) -> ExtManifest {
    let Some(obj) = exported.as_object() else {
        return base;
    };
    let mut m = base;
    if let Some(name) = obj.get("name").and_then(Value::as_str) {
        if !name.is_empty() {
            m.name = name.to_string();
        }
    }
    if let Some(description) = obj.get("description").and_then(Value::as_str) {
        m.description = Some(description.to_string());
    }
    if let Some(rebase) = obj.get("rebase").and_then(Value::as_bool) {
        m.rebase = rebase;
    }
    m
}

enum Kind {
    Script,
    TypeScript,
    Other,
}

fn extension_kind(path: &Path) -> Kind {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js") | Some("mjs") => Kind::Script,
        Some("ts") => Kind::TypeScript,
        _ => Kind::Other,
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// The refusal of a `.ts` entry: `strip_types` is the single place that decides it.
fn typescript_refused(path: &Path) -> OxaError {
    let source = std::fs::read_to_string(path).unwrap_or_default();
    match strip_types(&source) {
        Err(e) => e,
        Ok(_) => OxaError::unsupported("loading TypeScript extensions")
            .with_hint("compile the extension to .js (tsc or esbuild) and load that"),
    }
}

fn file_name_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("extension")
        .to_string()
}

fn file_manifest(path: &Path) -> ExtManifest {
    ExtManifest {
        name: file_name_stem(path),
        description: None,
        entries: vec![path.to_path_buf()],
        rebase: false,
    }
}

/// A package directory: `package.json` `oxa` (or `pi`) object first, then `index.js` /
/// `index.mjs`.
fn read_package(dir: &Path) -> Result<ExtManifest, OxaError> {
    let dir_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("extension")
        .to_string();
    let mut manifest = ExtManifest {
        name: dir_name,
        description: None,
        entries: Vec::new(),
        rebase: false,
    };

    let pkg_path = dir.join("package.json");
    if pkg_path.is_file() {
        let text = std::fs::read_to_string(&pkg_path)
            .map_err(|e| OxaError::io(&format!("read {}", pkg_path.display()), &e))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let json: Value = serde_json::from_str(text).map_err(|e| {
            OxaError::new(
                codes::INVALID,
                format!("invalid package.json: {}: {e}", pkg_path.display()),
            )
        })?;
        let block = json.get("oxa").or_else(|| json.get("pi"));
        if let Some(block) = block {
            let obj = block.as_object().ok_or_else(|| {
                OxaError::new(
                    codes::INVALID,
                    format!("the `oxa` key of {} must be an object", pkg_path.display()),
                )
            })?;
            if let Some(name) = obj.get("name").and_then(Value::as_str) {
                if !name.is_empty() {
                    manifest.name = name.to_string();
                }
            }
            if let Some(description) = obj.get("description").and_then(Value::as_str) {
                manifest.description = Some(description.to_string());
            }
            if let Some(rebase) = obj.get("rebase").and_then(Value::as_bool) {
                manifest.rebase = rebase;
            }
            if let Some(list) = obj.get("extensions").and_then(Value::as_array) {
                let mut declared = Vec::new();
                for item in list.iter().filter_map(Value::as_str) {
                    let entry = dir.join(item);
                    if matches!(extension_kind(&entry), Kind::TypeScript) {
                        return Err(typescript_refused(&entry));
                    }
                    if entry.is_file() {
                        declared.push(entry);
                    }
                }
                if !declared.is_empty() {
                    manifest.entries = declared;
                    return Ok(manifest);
                }
            }
        }
    }

    for index in ["index.js", "index.mjs"] {
        let entry = dir.join(index);
        if entry.is_file() {
            manifest.entries = vec![entry];
            return Ok(manifest);
        }
    }
    let ts_index = dir.join("index.ts");
    if ts_index.is_file() {
        return Err(typescript_refused(&ts_index));
    }
    Err(OxaError::new(
        codes::NOT_FOUND,
        format!("no loadable extension in {}", dir.display()),
    )
    .with_hint("add index.js, or a package.json with an `oxa` key listing `extensions`"))
}
