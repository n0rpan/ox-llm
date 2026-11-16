//! A15: ES module loading without ambient fs/net, and the `.ts` gate.
use oxa_proto::{codes, OxaError};
use std::collections::BTreeMap;

/// Resolves and loads modules for `import`. Given by the embedder: the runtime itself never
/// touches the file system or the network.
pub trait ModuleLoader {
    /// Map `specifier` as written in `importer` (a resolved name) to a resolved name.
    /// `Err(not_found)` for anything the embedder does not offer (bare Node / npm specifiers
    /// get the hint "Node and npm imports are not available in oxa").
    fn resolve(&self, importer: &str, specifier: &str) -> Result<String, OxaError>;
    /// The JavaScript source of a resolved name.
    fn load(&self, resolved: &str) -> Result<String, OxaError>;
}

/// A loader over an in-memory table `resolved name -> source`. Relative specifiers (`./x.js`,
/// `../y.js`) resolve lexically against the importer's name; anything else must match a key.
#[derive(Debug, Clone, Default)]
pub struct MapLoader {
    pub modules: BTreeMap<String, String>,
}

impl MapLoader {
    pub fn new() -> MapLoader {
        MapLoader::default()
    }
    pub fn with(mut self, name: &str, source: &str) -> MapLoader {
        self.modules.insert(name.to_string(), source.to_string());
        self
    }
}

impl ModuleLoader for MapLoader {
    fn resolve(&self, importer: &str, specifier: &str) -> Result<String, OxaError> {
        if self.modules.contains_key(specifier) {
            return Ok(specifier.to_string());
        }
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let joined = join_specifier(importer, specifier)?;
            if self.modules.contains_key(&joined) {
                return Ok(joined);
            }
            return Err(
                OxaError::new(codes::NOT_FOUND, format!("module not found: {specifier}"))
                    .with_hint(format!("resolved to {joined}, which the loader does not offer")),
            );
        }
        let err = OxaError::new(codes::NOT_FOUND, format!("module not found: {specifier}"));
        Err(err.with_hint("Node and npm imports are not available in oxa"))
    }
    fn load(&self, resolved: &str) -> Result<String, OxaError> {
        self.modules
            .get(resolved)
            .cloned()
            .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("module not found: {resolved}")))
    }
}

/// Lexically join a relative specifier onto the importer's directory (`a/b.js` + `./c.js` =
/// `a/c.js`; `..` never climbs above the root: that is `Err(invalid)`).
pub fn join_specifier(importer: &str, specifier: &str) -> Result<String, OxaError> {
    if specifier.starts_with('/') {
        return Err(OxaError::new(
            codes::INVALID,
            format!("absolute module path refused: {specifier}"),
        ));
    }
    let mut parts: Vec<&str> = importer.split('/').collect();
    // Drop the importer's own file name; the specifier is relative to its directory.
    parts.pop();
    for seg in specifier.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(OxaError::new(
                        codes::INVALID,
                        format!("module path climbs above the root: {specifier}"),
                    ));
                }
            }
            s => parts.push(s),
        }
    }
    Ok(parts.join("/"))
}

/// Strip TypeScript types so a `.ts` extension can load. W0 decision: not available; returns
/// `unsupported_in_oxa` with the hint "compile the extension to .js (tsc or esbuild) and load
/// that". The architect revisits this if `oxc` fits the size budget.
pub fn strip_types(_source: &str) -> Result<String, OxaError> {
    Err(OxaError::unsupported("loading TypeScript extensions")
        .with_hint("compile the extension to .js (tsc or esbuild) and load that"))
}
