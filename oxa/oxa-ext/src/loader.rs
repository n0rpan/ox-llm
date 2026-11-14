//! E1: loading extensions into runtimes. Owner: task E1. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/extensions/loader.ts
use crate::{ExtError, ExtId, ExtManifest, ExtState, LoadedExtension, Registry};
use oxa_proto::{codes, OxaError, Value};
use oxa_script::{join_specifier, CallbackPort, Limits, ModuleLoader, RuntimeThread, SlotId, Slots};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Options of one load pass.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadOpts {
    pub limits: Limits,
    /// Values of extension flags given on the command line `(name, value)`; applied to
    /// `RegisteredFlag::value` as soon as the flag is registered.
    pub flag_values: Vec<(String, Value)>,
}

impl Default for LoadOpts {
    fn default() -> Self {
        LoadOpts {
            limits: Limits::EXTENSION,
            flag_values: Vec::new(),
        }
    }
}

/// The virtual module of the pi-tui shim.
const TUI_SPECIFIER: &str = "oxa:tui";
/// The module that boots one extension: it imports the entry as a namespace, calls its
/// default export with the global `oxa`, and hands back its `manifest` export. It is the
/// only module evaluated by name; the entry is reached through its import, so it is
/// evaluated exactly once.
const BOOT_MODULE: &str = "oxa-boot.js";

/// Load `paths` in order (the order of `Resources::extension_paths`, which is pi's discovery
/// order). For each: read the manifest, create a slot on `thread`, call every `api_*::install`,
/// evaluate the entry module, call its default export with the `oxa` object (a factory may be
/// sync or return a promise; both are awaited within the deadline).
///
/// A failure (missing file, syntax error, throwing factory, deadline) is recorded in that
/// extension's `LoadedExtension::error`, everything it registered is removed
/// (`Registry::remove_extension`), its slot is dropped, and loading continues with the next
/// path. Returns the extensions of this pass, which are also appended to
/// `st.registry.extensions`.
pub fn load_extensions(
    st: &Arc<ExtState>,
    thread: &RuntimeThread,
    paths: &[PathBuf],
    opts: &LoadOpts,
) -> Vec<LoadedExtension> {
    let mut out = Vec::new();
    for path in paths {
        match crate::read_manifest(path) {
            Err(e) => {
                let display = path.display().to_string();
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&display)
                    .to_string();
                let id = push_provisional(st, display.clone(), name.clone(), false);
                out.push(fail(st, id, display, name, &e));
            }
            Ok(manifest) => {
                for entry in &manifest.entries {
                    let (root, rel) = entry_root(path, entry);
                    let source = Source::File { root, rel };
                    out.push(load_one(
                        st,
                        thread,
                        opts,
                        &manifest,
                        entry.display().to_string(),
                        source,
                    ));
                }
            }
        }
    }
    out
}

/// Load an extension from source text (built-in extensions, tests). Same rules as
/// [`load_extensions`]; `name` is shown as `builtin:<name>`.
pub fn load_source(
    st: &Arc<ExtState>,
    thread: &RuntimeThread,
    name: &str,
    source: &str,
    opts: &LoadOpts,
) -> LoadedExtension {
    let path = format!("builtin:{name}");
    let manifest = ExtManifest {
        name: name.to_string(),
        description: None,
        entries: Vec::new(),
        rebase: false,
    };
    let source = Source::Memory {
        name: path.clone(),
        source: source.to_string(),
    };
    load_one(st, thread, opts, &manifest, path, source)
}

/// Drop every extension context and clear the registry (`/reload`, shutdown). `FnRef`s held
/// anywhere become stale. Idempotent.
pub fn unload_all(st: &Arc<ExtState>, thread: &RuntimeThread) {
    // A thread that is already gone holds no runtimes, so the outcome is the same.
    let _ = thread.run(
        |slots: &mut Slots| slots.clear(),
        &mut |_: Value, _: &CallbackPort| Value::Null,
    );
    *st.registry.lock().unwrap_or_else(|p| p.into_inner()) = Registry::default();
}

/// Bind the whole API on one runtime by calling every `api_*::install` in the fixed order
/// register, ctx, ui, components, render. Called on the runtime thread.
pub fn install_api(rt: &mut oxa_script::Runtime, st: &Arc<ExtState>, ext: ExtId) {
    crate::api_register::install(rt, st, ext);
    crate::api_ctx::install(rt, st, ext);
    crate::api_ui::install(rt, st, ext);
    crate::api_components::install(rt, st, ext);
    crate::api_render::install(rt, st, ext);
}

/// The module loader of one extension: relative imports resolve inside the extension's own
/// directory (a path that leaves it is `not_found`); the virtual module `oxa:tui` is
/// [`crate::api_components::TUI_JS`]; everything else (bare specifiers, `node:*`, URLs) is
/// `not_found` with the hint "Node and npm imports are not available in oxa".
#[derive(Debug, Clone)]
pub struct ExtModuleLoader {
    pub root: PathBuf,
}

impl ModuleLoader for ExtModuleLoader {
    fn resolve(&self, importer: &str, specifier: &str) -> Result<String, OxaError> {
        if specifier == TUI_SPECIFIER {
            return Ok(TUI_SPECIFIER.to_string());
        }
        if is_relative(specifier) {
            return join_specifier(importer, specifier).map_err(|_| outside_root(specifier));
        }
        Err(node_refused(specifier))
    }

    fn load(&self, resolved: &str) -> Result<String, OxaError> {
        if resolved == TUI_SPECIFIER {
            return Ok(crate::api_components::TUI_JS.to_string());
        }
        let not_found = || OxaError::new(codes::NOT_FOUND, format!("module not found: {resolved}"));
        let root = self.root.canonicalize().map_err(|_| not_found())?;
        let file = root.join(resolved).canonicalize().map_err(|_| not_found())?;
        // A symlink inside the directory that points outside it is outside it.
        if !file.starts_with(&root) {
            return Err(outside_root(resolved));
        }
        let source =
            std::fs::read_to_string(&file).map_err(|e| OxaError::io(&format!("read {resolved}"), &e))?;
        check_imports(resolved, &source)?;
        Ok(source)
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// What one runtime loads.
#[derive(Debug, Clone)]
enum Source {
    /// A file (or a package entry) below `root`; `rel` is its name relative to `root`.
    File { root: PathBuf, rel: String },
    /// A built-in or test extension, held in memory under its module `name`.
    Memory { name: String, source: String },
}

impl Source {
    /// The specifier the boot module imports the entry by (relative to the boot module).
    fn specifier(&self) -> String {
        match self {
            Source::File { rel, .. } => format!("./{rel}"),
            Source::Memory { name, .. } => format!("./{name}"),
        }
    }

    fn loader(&self) -> Box<dyn ModuleLoader> {
        match self {
            Source::File { root, .. } => Box::new(ExtModuleLoader { root: root.clone() }),
            Source::Memory { name, source } => Box::new(MemoryLoader {
                name: name.clone(),
                source: source.clone(),
            }),
        }
    }
}

/// A built-in extension's loader: its own module and the virtual `oxa:tui`, nothing else.
struct MemoryLoader {
    name: String,
    source: String,
}

impl ModuleLoader for MemoryLoader {
    fn resolve(&self, importer: &str, specifier: &str) -> Result<String, OxaError> {
        if specifier == TUI_SPECIFIER {
            return Ok(TUI_SPECIFIER.to_string());
        }
        if is_relative(specifier) {
            let joined = join_specifier(importer, specifier).map_err(|_| outside_root(specifier))?;
            if joined == self.name {
                return Ok(joined);
            }
            return Err(outside_root(specifier));
        }
        Err(node_refused(specifier))
    }

    fn load(&self, resolved: &str) -> Result<String, OxaError> {
        if resolved == TUI_SPECIFIER {
            return Ok(crate::api_components::TUI_JS.to_string());
        }
        if resolved == self.name {
            check_imports(resolved, &self.source)?;
            return Ok(self.source.clone());
        }
        Err(OxaError::new(
            codes::NOT_FOUND,
            format!("module not found: {resolved}"),
        ))
    }
}

fn is_relative(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

fn outside_root(specifier: &str) -> OxaError {
    OxaError::new(codes::NOT_FOUND, format!("module not found: {specifier}"))
        .with_hint("the module lies outside the extension's own directory")
}

fn node_refused(specifier: &str) -> OxaError {
    OxaError::new(
        codes::UNSUPPORTED_IN_OXA,
        format!("import of {specifier:?}: Node and npm imports are not available in oxa"),
    )
    .with_hint(IMPORT_HINT)
}

/// What an extension uses instead of a Node or npm module.
const IMPORT_HINT: &str = "use the oxa: modules (`import { Text } from \"oxa:tui\"`), the host API \
     (oxa.exec, ctx.fs, ctx.fetch) or a relative import inside the extension's directory";

/// Check the module statements of one extension module before it is compiled, so that a Node
/// or npm dependency fails with a clear `unsupported_in_oxa` error naming the statement instead
/// of a QuickJS `SyntaxError` or a missing module:
///
/// * `import .. from "<bare or node: specifier>"`, `import("<..>")` -> `unsupported_in_oxa`;
/// * `require(..)` (CommonJS) -> `unsupported_in_oxa` (extensions are ES modules);
/// * an `import` statement inside a function or block -> `unsupported_in_oxa` when its
///   specifier is not supported, else `invalid` (import statements belong at the top level).
///
/// Supported specifiers are relative (`./`, `../`) and the `oxa:` modules. The scan skips
/// strings, template literals, comments and regular expression literals.
pub fn check_imports(module: &str, source: &str) -> Result<(), OxaError> {
    for stmt in scan_module_statements(source) {
        let line = stmt.line;
        let text = stmt.text.trim();
        let supported = stmt
            .specifier
            .as_deref()
            .is_some_and(|s| is_relative(s) || s.starts_with("oxa:"));
        if stmt.require {
            return Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                format!("{module}:{line}: `{text}`: require() is not supported in oxa (extensions are ES modules)"),
            )
            .with_hint(IMPORT_HINT));
        }
        if !supported {
            return Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                format!("{module}:{line}: `{text}`: Node and npm imports are not available in oxa"),
            )
            .with_hint(IMPORT_HINT));
        }
        if stmt.nested && !stmt.dynamic {
            return Err(OxaError::new(
                codes::INVALID,
                format!(
                    "{module}:{line}: `{text}`: an import statement must be at the top level of the module"
                ),
            ));
        }
    }
    Ok(())
}

/// One `import` / `require` found by [`scan_module_statements`].
struct ModuleStatement {
    /// 1-based line of the keyword.
    line: usize,
    /// The statement text (up to its end or the end of the line), for the error message.
    text: String,
    /// The module specifier, when it is a string literal.
    specifier: Option<String>,
    /// Inside braces or parentheses (not at the top level).
    nested: bool,
    /// `import(..)`.
    dynamic: bool,
    /// `require(..)`.
    require: bool,
}

/// A small JavaScript scanner: finds the `import` and `require` keywords outside strings,
/// comments, template literals and regular expressions, with their specifiers.
fn scan_module_statements(src: &str) -> Vec<ModuleStatement> {
    let b: Vec<char> = src.chars().collect();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth: i64 = 0;
    let mut line = 1;
    // The last significant character, to tell a regex literal from a division.
    let mut prev: char = ';';
    let mut prev_word = String::new();
    let skip_string = |i: &mut usize, line: &mut usize, q: char| {
        *i += 1;
        while *i < n && b[*i] != q {
            if b[*i] == '\\' {
                *i += 1;
            }
            if *i < n && b[*i] == '\n' {
                *line += 1;
            }
            *i += 1;
        }
        *i += 1;
    };
    while i < n {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < n && b[i + 1] == '/' {
            while i < n && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && b[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(b[i] == '*' && b[i + 1] == '/') {
                if b[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            skip_string(&mut i, &mut line, c);
            prev = 'a';
            prev_word.clear();
            continue;
        }
        if c == '/' {
            let regex = "(,=:[!&|?{};+-*%<>~^".contains(prev)
                || matches!(
                    prev_word.as_str(),
                    "return" | "typeof" | "case" | "do" | "else" | "in" | "of"
                );
            if regex {
                i += 1;
                let mut class = false;
                while i < n && b[i] != '\n' {
                    match b[i] {
                        '\\' => i += 1,
                        '[' => class = true,
                        ']' => class = false,
                        '/' if !class => break,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                prev = 'a';
                prev_word.clear();
                continue;
            }
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < n && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                i += 1;
            }
            let word: String = b[start..i].iter().collect();
            let member = prev == '.';
            if !member && (word == "import" || word == "require") {
                let mut j = i;
                while j < n && b[j].is_whitespace() {
                    j += 1;
                }
                let next = b.get(j).copied().unwrap_or(' ');
                let is_require = word == "require";
                if (is_require && next == '(') || (!is_require && next != '.') {
                    let dynamic = !is_require && next == '(';
                    // The statement ends at `;` or at the end of its last line.
                    let mut end = j;
                    while end < n && b[end] != ';' && !(b[end] == '\n' && statement_done(&b[start..end])) {
                        end += 1;
                    }
                    let text: String = b[start..end.min(n)].iter().collect();
                    let first_line = text.lines().next().unwrap_or("").to_string();
                    out.push(ModuleStatement {
                        line,
                        text: if text.contains('\n') {
                            first_line
                        } else {
                            text.clone()
                        },
                        specifier: specifier_of(&text, !is_require && !dynamic),
                        nested: depth > 0,
                        dynamic,
                        require: is_require,
                    });
                }
            }
            prev = 'a';
            prev_word = word;
            continue;
        }
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
        prev = c;
        prev_word.clear();
        i += 1;
    }
    out
}

/// `true` when the statement text so far has its specifier (a closing quote after `from`, or
/// a bare `import "x"`), so a newline ends it.
fn statement_done(text: &[char]) -> bool {
    let t: String = text.iter().collect();
    let quotes = t.chars().filter(|c| *c == '"' || *c == '\'').count();
    quotes >= 2 || t.contains(')')
}

/// The string literal that names the module: after `from` in a static import with bindings,
/// else the first string literal.
fn specifier_of(text: &str, static_import: bool) -> Option<String> {
    let lit = |s: &str| -> Option<String> {
        let start = s.find(['"', '\''])?;
        let q = s[start..].chars().next()?;
        let rest = &s[start + 1..];
        let end = rest.find(q)?;
        Some(rest[..end].to_string())
    };
    if static_import {
        if let Some(pos) = text.rfind("from") {
            if let Some(s) = lit(&text[pos + 4..]) {
                return Some(s);
            }
        }
    }
    lit(text)
}

/// The directory an entry's imports resolve in, and the entry's name inside it. For a package
/// entry that is the package directory; for a file it is the file's own directory.
fn entry_root(manifest_path: &Path, entry: &Path) -> (PathBuf, String) {
    let package_root = if manifest_path.is_dir() {
        Some(manifest_path.to_path_buf())
    } else {
        None
    };
    if let Some(root) = package_root {
        if let Ok(rel) = entry.strip_prefix(&root) {
            let plain = rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
            if plain {
                return (root, rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let root = entry.parent().map(Path::to_path_buf).unwrap_or_default();
    let rel = entry
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (root, rel)
}

/// The boot module's source for one entry; `SPEC` is replaced by the entry's JSON-quoted
/// specifier.
const BOOT_TEMPLATE: &str = r#"import * as __oxa_ext from SPEC;
export function boot() {
  const factory = __oxa_ext.default;
  if (typeof factory !== "function") {
    throw new Error("Extension does not export a valid factory function");
  }
  return factory(globalThis.oxa);
}
export function manifest() {
  const m = __oxa_ext.manifest;
  return m === undefined ? null : m;
}
"#;

fn boot_source(specifier: &str) -> String {
    let spec = serde_json::to_string(specifier).unwrap_or_else(|_| "\"\"".to_string());
    BOOT_TEMPLATE.replace("SPEC", &spec)
}

/// Reserve the next id and a provisional entry in the registry (ids are indices in load
/// order). Never holds the lock across a script call.
fn push_provisional(st: &Arc<ExtState>, path: String, name: String, rebase: bool) -> ExtId {
    let mut reg = st.registry.lock().unwrap_or_else(|p| p.into_inner());
    let id = ExtId(reg.extensions.len() as u32);
    reg.extensions.push(LoadedExtension {
        id,
        path,
        name,
        slot: None,
        declared_rebase: rebase,
        error: None,
    });
    id
}

/// Record a failed load: remove what it registered, keep the error on its entry, and report it
/// as an `ExtensionError`.
fn fail(st: &Arc<ExtState>, id: ExtId, path: String, name: String, err: &OxaError) -> LoadedExtension {
    // pi's text; oxa adds the stable code (e.g. `unsupported_in_oxa`) when it is not the
    // generic `script`.
    let message = if err.code == codes::SCRIPT {
        format!("Failed to load extension: {err}")
    } else {
        format!("Failed to load extension: {err} ({})", err.code)
    };
    let entry = {
        let mut reg = st.registry.lock().unwrap_or_else(|p| p.into_inner());
        reg.remove_extension(id);
        match reg.extensions.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.slot = None;
                e.error = Some(message.clone());
                e.clone()
            }
            None => LoadedExtension {
                id,
                path: path.clone(),
                name: name.clone(),
                slot: None,
                declared_rebase: false,
                error: Some(message.clone()),
            },
        }
    };
    st.push_error(ExtError {
        extension_path: path,
        event: "load".to_string(),
        error: message,
    });
    entry
}

/// Load one entry on a fresh runtime of `thread`.
fn load_one(
    st: &Arc<ExtState>,
    thread: &RuntimeThread,
    opts: &LoadOpts,
    manifest: &ExtManifest,
    path: String,
    source: Source,
) -> LoadedExtension {
    let id = push_provisional(st, path.clone(), manifest.name.clone(), manifest.rebase);
    let job_state = st.clone();
    let limits = opts.limits;
    let job_source = source.clone();
    let run = thread.run(
        move |slots: &mut Slots| boot(slots, &job_state, id, limits, &job_source),
        &mut |_: Value, _: &CallbackPort| Value::Null,
    );
    let outcome = match run {
        Ok(inner) => inner,
        Err(e) => Err(e),
    };
    match outcome {
        Ok((slot, export)) => {
            let merged = crate::merge_exported_manifest(manifest.clone(), &export);
            let mut reg = st.registry.lock().unwrap_or_else(|p| p.into_inner());
            apply_flags(&mut reg, id, &opts.flag_values);
            match reg.extensions.iter_mut().find(|e| e.id == id) {
                Some(e) => {
                    e.slot = Some(slot);
                    e.name = merged.name;
                    e.declared_rebase = merged.rebase;
                    e.error = None;
                    e.clone()
                }
                None => LoadedExtension {
                    id,
                    path,
                    name: merged.name,
                    slot: Some(slot),
                    declared_rebase: merged.rebase,
                    error: None,
                },
            }
        }
        Err(err) => {
            let name = manifest.name.clone();
            fail(st, id, path, name, &err)
        }
    }
}

/// Create a runtime for `source`, install the API, evaluate and boot the extension, and read
/// its `manifest` export. On failure the slot is dropped again.
fn boot(
    slots: &mut Slots,
    st: &Arc<ExtState>,
    id: ExtId,
    limits: Limits,
    source: &Source,
) -> Result<(SlotId, Value), OxaError> {
    let slot = slots.create(limits)?;
    match boot_in(slots, slot, st, id, source) {
        Ok(export) => Ok((slot, export)),
        Err(e) => {
            slots.remove(slot);
            Err(e)
        }
    }
}

fn boot_in(
    slots: &mut Slots,
    slot: SlotId,
    st: &Arc<ExtState>,
    id: ExtId,
    source: &Source,
) -> Result<Value, OxaError> {
    let rt = slots
        .get(slot)
        .ok_or_else(|| OxaError::new(codes::INTERNAL, "the new extension runtime is missing"))?;
    rt.set_loader(source.loader());
    // The registration calls made while the factory runs need the slot (handlers, tools and
    // commands are stored with it), so it is recorded before the API is installed. `fail`
    // clears it again when the load does not complete.
    {
        let mut reg = st.registry.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = reg.extensions.iter_mut().find(|e| e.id == id) {
            e.slot = Some(slot);
        }
    }
    install_api(rt, st, id);
    let glue = rt.eval_module(BOOT_MODULE, &boot_source(&source.specifier()))?;
    rt.call_export(&glue, "boot", &[])?;
    rt.call_export(&glue, "manifest", &[])
}

/// Copy the command-line flag values onto the flags this extension registered.
fn apply_flags(reg: &mut Registry, id: ExtId, values: &[(String, Value)]) {
    for flag in reg.flags.iter_mut().filter(|f| f.ext == id) {
        if let Some((_, value)) = values.iter().find(|(name, _)| *name == flag.spec.name) {
            flag.value = value.clone();
        }
    }
}
