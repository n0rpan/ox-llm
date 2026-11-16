//! E10: packages (install / remove / list / update, resolve to resources).
// ported from pi packages/coding-agent/src/core/package-manager.ts and pi-manifest.ts
use crate::sources::{apply_filter, glob_match, has_glob, normalize_pattern, parse_source};
use crate::{Diagnostic, DiscoverOpts, PackageSource, ResourceSource};
use oxa_config::{Dirs, Settings};
use oxa_proto::{codes, names, OxaError};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Which settings file declares a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// `<agent dir>/settings.json`.
    User,
    /// `<cwd>/.oxa/settings.json` (`--local`); read only in trusted projects.
    Project,
}

/// One entry of the settings `packages` array (string form = only `source`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageDecl {
    pub source: String,
    /// Filters per resource type; see [`crate::apply_filter`].
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
}

/// What a package offers: its manifest (`package.json` key `oxa`, with pi's `pi` key accepted)
/// or, without one, the conventional directories `extensions/`, `skills/`, `prompts/`,
/// `themes/`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageManifest {
    pub name: String,
    pub version: Option<String>,
    /// Package-relative paths, sorted.
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
}

/// A declared package and its state on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub decl: PackageDecl,
    pub source: PackageSource,
    pub scope: Scope,
    /// Where its files are (the checkout / unpack dir, or the local path).
    pub root: PathBuf,
    pub installed: bool,
    /// `None` when not installed or unreadable.
    pub manifest: Option<PackageManifest>,
}

/// The resources contributed by all installed packages, after filters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageResources {
    /// Extension entry files in load order: `-e` paths, user packages, project packages, then
    /// `<agent dir>/extensions`, project `.oxa/extensions` (trusted), settings `extensions`.
    pub extension_paths: Vec<PathBuf>,
    pub skill_paths: Vec<(PathBuf, ResourceSource)>,
    pub template_paths: Vec<(PathBuf, ResourceSource)>,
    /// Sorted.
    pub theme_paths: Vec<PathBuf>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Runs `git` for the package manager, so tests can script them. The real
/// implementation ([`SystemRunner`]) runs the program directly (no shell), with the user's own
/// rights: package commands are explicit user actions.
pub trait CommandRunner: Send + Sync {
    /// Returns `(exit code, combined output)`. `Err` when the program cannot be started.
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(i32, String), OxaError>;
}

/// The real [`CommandRunner`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(i32, String), OxaError> {
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            // A git prompt for credentials would hang the command; fail instead.
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| {
                OxaError::new(codes::NOT_FOUND, format!("cannot run `{program}`: {e}"))
                    .with_hint("Install it, or check the PATH.")
            })?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.code().unwrap_or(-1), text))
    }
}

/// Read a package's manifest from its root directory.
///
/// A missing `package.json` or one without an `oxa` / `pi` key yields the conventional
/// directories. A resource field in the manifest replaces the conventional default of that
/// field only. Entries are package-relative paths or globs (`*`, `?`, `**`); a plain directory
/// name selects the files under it. Errors: `not_found` (no directory), `invalid` (unreadable
/// `package.json`).
pub fn read_manifest(root: &Path) -> Result<PackageManifest, OxaError> {
    if !root.is_dir() {
        return Err(OxaError::new(
            codes::NOT_FOUND,
            format!("package directory {} does not exist", root.display()),
        ));
    }
    let pkg = read_package_json(root)?;
    let manifest_obj = pkg
        .as_ref()
        .and_then(|p| p.get("oxa").or_else(|| p.get("pi")))
        .and_then(Value::as_object);
    let mut all = Vec::new();
    walk(root, "", &mut all, 0);
    let field = |name: &str, default: &[&str]| -> Vec<String> {
        let patterns: Vec<String> = match manifest_obj.and_then(|d| d.get(name)).and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            None => default.iter().map(|s| s.to_string()).collect(),
        };
        let mut files: Vec<String> = Vec::new();
        for pattern in &patterns {
            for f in &all {
                if pattern_hits(pattern, f) && !files.contains(f) {
                    files.push(f.clone());
                }
            }
        }
        files.sort();
        files
    };
    let mut skills = field("skills", &["skills/**/SKILL.md", "skills/*.md"]);
    // Skill candidates: SKILL.md files and standalone .md files, not READMEs or changelogs.
    skills.retain(|f| {
        let name = f.rsplit('/').next().unwrap_or(f);
        name == "SKILL.md"
            || (name.ends_with(".md")
                && !name.eq_ignore_ascii_case("README.md")
                && !name.eq_ignore_ascii_case("CHANGELOG.md"))
    });
    let name = pkg
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let version = pkg
        .as_ref()
        .and_then(|p| p.get("version"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(PackageManifest {
        name,
        version,
        extensions: field(
            "extensions",
            &[
                "extensions/*.js",
                "extensions/*.mjs",
                "extensions/*.ts",
                "extensions/*/index.js",
                "extensions/*/index.mjs",
                "extensions/*/index.ts",
            ],
        ),
        skills,
        prompts: field("prompts", &["prompts/*.md"]),
        themes: field("themes", &["themes/*.json"]),
    })
}

/// `package.json` of a package root: `None` when absent, an error when present but invalid.
fn read_package_json(root: &Path) -> Result<Option<Value>, OxaError> {
    let path = root.join("package.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            serde_json::from_str::<Value>(text).map(Some).map_err(|e| {
                OxaError::new(
                    codes::INVALID,
                    format!("{} is not valid JSON: {e}", path.display()),
                )
            })
        }
        Err(_) => Ok(None),
    }
}

/// Package-relative paths of every regular file under `root/rel`, sorted within each directory.
/// Dot entries and `node_modules` are skipped; depth is capped.
fn walk(root: &Path, rel: &str, out: &mut Vec<String>, depth: usize) {
    if depth > 16 {
        return;
    }
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        match std::fs::metadata(root.join(&child)) {
            Ok(m) if m.is_dir() => walk(root, &child, out, depth + 1),
            Ok(m) if m.is_file() => out.push(child),
            _ => {}
        }
    }
}

/// A manifest entry selects a file: a glob, or a plain path / directory prefix.
fn pattern_hits(pattern: &str, path: &str) -> bool {
    let pattern = normalize_pattern(pattern);
    if has_glob(&pattern) {
        glob_match(&pattern, path)
    } else {
        path == pattern
            || path
                .strip_prefix(pattern.as_str())
                .is_some_and(|r| r.starts_with('/'))
    }
}

/// The `packages` declarations of the user and (if `trusted`) project settings, in that order.
///
/// The layers are read from their own files, because the merged value of an array is the
/// project's list (lists replace lower layers), and the user's packages must still count. A
/// missing or unreadable file declares nothing (its problem is already a settings diagnostic).
pub fn declared_packages(
    dirs: &Dirs,
    cwd: &Path,
    _settings: &Settings,
    trusted: bool,
) -> Vec<(Scope, PackageDecl)> {
    let mut out: Vec<(Scope, PackageDecl)> = read_layer_packages(&dirs.settings_file())
        .into_iter()
        .map(|d| (Scope::User, d))
        .collect();
    if trusted {
        out.extend(
            read_layer_packages(&dirs.project_settings_file(cwd))
                .into_iter()
                .map(|d| (Scope::Project, d)),
        );
    }
    out
}

/// The `packages` array of one settings file, as declarations. Entries that are neither a
/// string nor an object with a `source` string are skipped.
fn read_layer_packages(path: &Path) -> Vec<PackageDecl> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    root.get("packages")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(decl_from_value).collect())
        .unwrap_or_default()
}

/// One `packages` entry as a declaration.
fn decl_from_value(v: &Value) -> Option<PackageDecl> {
    match v {
        Value::String(s) => Some(PackageDecl {
            source: s.clone(),
            ..PackageDecl::default()
        }),
        Value::Object(o) => {
            let source = o.get("source")?.as_str()?.to_string();
            let list = |key: &str| -> Option<Vec<String>> {
                o.get(key).and_then(Value::as_array).map(|items| {
                    items
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
            };
            Some(PackageDecl {
                source,
                extensions: list("extensions"),
                skills: list("skills"),
                prompts: list("prompts"),
                themes: list("themes"),
            })
        }
        _ => None,
    }
}

/// Resolve everything packages and extension directories contribute for this epoch. Reads the
/// disk only: a package that is declared but not installed is a warning (the CLI and `/reload`
/// install; discovery never touches the network).
///
/// Order: `-e` package sources (`opts.extra_extensions` that are git sources), user
/// packages, project packages (trusted only). A project declaration replaces the user's one of
/// the same identity. `opts.no_extensions` / `no_skills` / `no_templates` drop the declared
/// packages' resources of that kind (not the `-e` ones).
pub fn package_resources(
    dirs: &Dirs,
    cwd: &Path,
    settings: &Settings,
    opts: &DiscoverOpts,
) -> PackageResources {
    let mgr = PackageManager::new(dirs, cwd, Box::new(SystemRunner));
    let mut out = PackageResources::default();

    // (scope, declaration, parsed source, is an -e entry)
    let mut items: Vec<(Scope, PackageDecl, PackageSource, bool)> = Vec::new();
    for e in &opts.extra_extensions {
        if !is_package_spec(e) {
            continue;
        }
        match parse_source(e, cwd) {
            Ok(source) => items.push((
                Scope::User,
                PackageDecl {
                    source: e.clone(),
                    ..PackageDecl::default()
                },
                source,
                true,
            )),
            Err(err) => out
                .diagnostics
                .push(Diagnostic::warning(None, format!("-e {e}: {}", err.message))),
        }
    }
    for (scope, decl) in declared_packages(dirs, cwd, settings, opts.trusted) {
        match parse_source(&decl.source, &scope_base(dirs, cwd, scope)) {
            Ok(source) => items.push((scope, decl, source, false)),
            Err(err) => out.diagnostics.push(Diagnostic::warning(
                None,
                format!("package {}: {}", decl.source, err.message),
            )),
        }
    }

    // Same identity twice: the project declaration wins, at the position of the user one.
    let mut kept: Vec<(Scope, PackageDecl, PackageSource, bool)> = Vec::new();
    for item in items {
        let id = item.2.identity();
        match kept.iter().position(|k| k.2.identity() == id) {
            None => kept.push(item),
            Some(i) if item.0 == Scope::Project && kept[i].0 == Scope::User && !kept[i].3 => {
                kept[i] = item;
            }
            Some(_) => {}
        }
    }

    for (scope, decl, source, is_e) in kept {
        let root = mgr.install_root(&source, scope);
        if !root.is_dir() {
            let hint = match source {
                PackageSource::Local(_) => "the path does not exist".to_string(),
                _ => format!("run `oxa install {}`", decl.source),
            };
            out.diagnostics.push(Diagnostic::warning(
                Some(root.as_path()),
                format!("package {} is not installed; {hint}", decl.source),
            ));
            continue;
        }
        let manifest = match read_manifest(&root) {
            Ok(m) => m,
            Err(err) => {
                out.diagnostics.push(Diagnostic::warning(
                    Some(root.as_path()),
                    format!("package {}: {}", decl.source, err.message),
                ));
                continue;
            }
        };
        if node_dependencies(&root) {
            out.diagnostics.push(Diagnostic::warning(
                Some(root.as_path()),
                format!(
                    "package {} declares Node dependencies, which oxa does not install; its extensions may not load",
                    decl.source
                ),
            ));
        }
        let want_ext = is_e || !opts.no_extensions;
        let want_skills = is_e || !opts.no_skills;
        let want_templates = is_e || !opts.no_templates;
        if want_ext {
            for rel in apply_filter(&manifest.extensions, decl.extensions.as_deref()) {
                out.extension_paths.push(root.join(rel));
            }
        }
        if want_skills {
            for rel in apply_filter(&manifest.skills, decl.skills.as_deref()) {
                out.skill_paths
                    .push((root.join(rel), ResourceSource::Package(decl.source.clone())));
            }
        }
        if want_templates {
            for rel in apply_filter(&manifest.prompts, decl.prompts.as_deref()) {
                out.template_paths
                    .push((root.join(rel), ResourceSource::Package(decl.source.clone())));
            }
        }
        for rel in apply_filter(&manifest.themes, decl.themes.as_deref()) {
            out.theme_paths.push(root.join(rel));
        }
    }
    out.theme_paths.sort();
    out
}

/// A `-e` value that names a package (`git:` / URL), not a local path. `npm:` counts too, so
/// that it reaches `parse_source` and its refusal instead of being read as a path.
pub(crate) fn is_package_spec(e: &str) -> bool {
    ["npm:", "git:", "https://", "http://", "ssh://", "git@", "file://"]
        .iter()
        .any(|p| e.starts_with(p))
}

/// Where relative local paths of a declaration resolve: the directory of its settings file.
fn scope_base(dirs: &Dirs, cwd: &Path, scope: Scope) -> PathBuf {
    match scope {
        Scope::User => dirs.agent_dir.clone(),
        Scope::Project => cwd.join(names::PROJECT_DIR),
    }
}

/// `true` when the package's `package.json` declares `dependencies` or `optionalDependencies`.
fn node_dependencies(root: &Path) -> bool {
    match read_package_json(root) {
        Ok(Some(pkg)) => ["dependencies", "optionalDependencies"].iter().any(|k| {
            pkg.get(*k)
                .and_then(Value::as_object)
                .is_some_and(|o| !o.is_empty())
        }),
        _ => false,
    }
}

/// The package manager behind `oxa install / remove / update / list`.
///
/// Install locations: git under `<agent dir>/git/<host>/<path>`; project scope uses
/// `<cwd>/.oxa/git`.
/// Persisting declarations to settings.json is the caller's job (`oxa-cli/src/pkg_cmd.rs`,
/// same task), through `oxa-config`; the manager only touches package files.
pub struct PackageManager {
    pub(crate) agent_dir: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) runner: Box<dyn CommandRunner>,
}

impl PackageManager {
    pub fn new(dirs: &Dirs, cwd: &Path, runner: Box<dyn CommandRunner>) -> PackageManager {
        PackageManager {
            agent_dir: dirs.agent_dir.clone(),
            cwd: cwd.to_path_buf(),
            runner,
        }
    }

    /// Fetch `source` into `scope`'s install location (git clone at the ref / nothing for a
    /// local path) and return its declaration and state. Installing an installed package
    /// reconciles it. Errors: `invalid` source, `unsupported_in_oxa` for an `npm:` source, `io`,
    /// or `not_found` with git's output when git fails.
    pub fn install(&self, source: &str, scope: Scope) -> Result<PackageInfo, OxaError> {
        let parsed = self.parse(source, scope)?;
        let root = self.install_root(&parsed, scope);
        match &parsed {
            PackageSource::Local(path) => {
                if !path.is_dir() {
                    return Err(OxaError::new(
                        codes::INVALID,
                        format!("local package {} is not a directory", path.display()),
                    )
                    .with_hint("A local source must name an existing directory."));
                }
            }
            PackageSource::Git { url, reference } => {
                self.install_git(url, reference.as_deref(), scope, &root)?;
            }
        }
        Ok(self.info(source, parsed, scope))
    }

    /// Delete the installed files of `source` in `scope` (never a local path's files).
    /// Returns `false` when nothing was installed.
    pub fn remove(&self, source: &str, scope: Scope) -> Result<bool, OxaError> {
        let parsed = self.parse(source, scope)?;
        if matches!(parsed, PackageSource::Local(_)) {
            return Ok(false);
        }
        let root = self.install_root(&parsed, scope);
        if !root.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&root)
            .map_err(|e| OxaError::io(&format!("remove {}", root.display()), &e))?;
        self.prune(&root, scope);
        Ok(true)
    }

    /// State of every declared package, in declaration order. A declaration that does not parse
    /// is left out (`package_resources` reports it as a diagnostic).
    pub fn list(&self, decls: &[(Scope, PackageDecl)]) -> Vec<PackageInfo> {
        decls
            .iter()
            .filter_map(|(scope, decl)| {
                let source = parse_source(&decl.source, &self.scope_base(*scope)).ok()?;
                let mut info = self.info(&decl.source, source, *scope);
                info.decl = decl.clone();
                Some(info)
            })
            .collect()
    }

    /// Reconcile every declared package: install missing ones, move unpinned ones to the newest
    /// matching version / the branch head; pinned ones never move. Returns one line per package
    /// that changed. A failure of one package is a diagnostic, not an error.
    pub fn update(&self, decls: &[(Scope, PackageDecl)]) -> (Vec<String>, Vec<Diagnostic>) {
        let mut lines = Vec::new();
        let mut diags = Vec::new();
        for (scope, decl) in decls {
            let parsed = match parse_source(&decl.source, &self.scope_base(*scope)) {
                Ok(p) => p,
                Err(e) => {
                    diags.push(Diagnostic::error(
                        None,
                        format!("package {}: {}", decl.source, e.message),
                    ));
                    continue;
                }
            };
            let root = self.install_root(&parsed, *scope);
            let installed = root.exists();
            let result: Result<Option<String>, OxaError> = match &parsed {
                PackageSource::Local(path) => {
                    if path.is_dir() {
                        Ok(None)
                    } else {
                        Err(OxaError::new(
                            codes::NOT_FOUND,
                            format!("local package {} does not exist", path.display()),
                        ))
                    }
                }
                PackageSource::Git { url, reference } => {
                    if !installed {
                        self.install_git(url, reference.as_deref(), *scope, &root)
                            .map(|()| Some(format!("installed {}", decl.source)))
                    } else if reference.is_some() {
                        // Pinned: the checkout stays at the ref.
                        Ok(None)
                    } else {
                        self.git_fetch_head(&root).map(|(before, after)| {
                            (before != after).then(|| {
                                format!("updated {}: {} -> {}", decl.source, short(&before), short(&after))
                            })
                        })
                    }
                }
            };
            match result {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => {}
                Err(e) => diags.push(Diagnostic::error(
                    Some(root.as_path()),
                    format!("package {}: {}", decl.source, e.message),
                )),
            }
        }
        (lines, diags)
    }

    /// Where `source` is (or would be) installed in `scope`.
    pub fn install_root(&self, source: &PackageSource, scope: Scope) -> PathBuf {
        match source {
            PackageSource::Git { .. } => {
                let mut p = self.scope_base(scope).join("git");
                let identity = source.identity();
                let path = identity.strip_prefix("git:").unwrap_or(&identity);
                for part in path.split('/') {
                    p.push(part);
                }
                p
            }
            PackageSource::Local(path) => path.clone(),
        }
    }

    fn scope_base(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::User => self.agent_dir.clone(),
            Scope::Project => self.cwd.join(names::PROJECT_DIR),
        }
    }

    fn parse(&self, source: &str, scope: Scope) -> Result<PackageSource, OxaError> {
        parse_source(source, &self.scope_base(scope))
    }

    fn info(&self, source: &str, parsed: PackageSource, scope: Scope) -> PackageInfo {
        let root = self.install_root(&parsed, scope);
        let installed = root.is_dir();
        let manifest = if installed {
            read_manifest(&root).ok()
        } else {
            None
        };
        PackageInfo {
            decl: PackageDecl {
                source: source.to_string(),
                ..PackageDecl::default()
            },
            source: parsed,
            scope,
            root,
            installed,
            manifest,
        }
    }

    /// Remove empty directories between `root` and the scope's `git` directory.
    fn prune(&self, root: &Path, scope: Scope) {
        let base = self.scope_base(scope);
        let mut cur = root.parent();
        while let Some(dir) = cur {
            if dir == base.as_path() || !dir.starts_with(&base) {
                break;
            }
            if matches!(dir.file_name().and_then(|n| n.to_str()), Some("git")) {
                break;
            }
            let empty = std::fs::read_dir(dir)
                .map(|mut r| r.next().is_none())
                .unwrap_or(false);
            if !empty {
                break;
            }
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            cur = dir.parent();
        }
    }

    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<String, OxaError> {
        let (code, out) = self.runner.run(program, args, cwd)?;
        if code == 0 {
            Ok(out)
        } else {
            Err(OxaError::new(
                codes::NOT_FOUND,
                format!(
                    "`{program} {}` failed (exit {code}): {}",
                    args.join(" "),
                    out.trim()
                ),
            ))
        }
    }

    fn git(&self, args: &[&str], cwd: &Path) -> Result<String, OxaError> {
        // B4 (wave H lane `harden`): oxa's own git calls never run a repository's hooks (a
        // checkout under a project's `.oxa` is writable by the model's file tools after an ask).
        let args: Vec<String> = ["-c", "core.hooksPath=/dev/null"]
            .iter()
            .chain(args)
            .map(|s| s.to_string())
            .collect();
        self.run("git", &args, cwd)
    }

    fn install_git(
        &self,
        url: &str,
        reference: Option<&str>,
        scope: Scope,
        root: &Path,
    ) -> Result<(), OxaError> {
        // P4: offline, only a repository on this machine (file://, a path, loopback).
        let local = url.starts_with("file://")
            || url.starts_with('/')
            || oxa_proto::url_host(url).is_some_and(|h| oxa_proto::is_loopback_host(&h));
        if !local {
            oxa_proto::check_online(&format!("git for package {url}"))?;
        }
        if root.is_dir() {
            // Reconcile an installed checkout: pinned ones are fetched and checked out again,
            // unpinned ones move to the remote head.
            return match reference {
                Some(r) => {
                    self.git(&["fetch", "--quiet", "origin"], root)?;
                    self.git(&["checkout", "--quiet", r], root).map(|_| ())
                }
                None => self.git_fetch_head(root).map(|_| ()),
            };
        }
        let parent = root
            .parent()
            .ok_or_else(|| OxaError::new(codes::INVALID, "git package has no install directory"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| OxaError::io(&format!("create {}", parent.display()), &e))?;
        let target = root.to_string_lossy().into_owned();
        let result = self
            .git(&["clone", "--quiet", url, &target], parent)
            .and_then(|_| match reference {
                Some(r) => self.git(&["checkout", "--quiet", r], root).map(|_| ()),
                None => Ok(()),
            });
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(root);
            self.prune(root, scope);
            return Err(e);
        }
        Ok(())
    }

    /// Fetch the remote head and reset the checkout to it. Returns (head before, head after).
    fn git_fetch_head(&self, root: &Path) -> Result<(String, String), OxaError> {
        let before = self.git(&["rev-parse", "HEAD"], root)?.trim().to_string();
        self.git(&["fetch", "--quiet", "origin", "HEAD"], root)?;
        self.git(&["reset", "--quiet", "--hard", "FETCH_HEAD"], root)?;
        let after = self.git(&["rev-parse", "HEAD"], root)?.trim().to_string();
        Ok((before, after))
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}
