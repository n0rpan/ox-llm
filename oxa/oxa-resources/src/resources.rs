//! Architect-owned (W0): the `Resources` data model and `discover`, the one entry point.
use crate::{context, packages, skills, templates};
use oxa_config::{Dirs, Settings};
use oxa_proto::Level;
use std::path::{Path, PathBuf};

/// Where a resource was found. Order of precedence on a name clash is the order of discovery
/// documented on each `discover_*` function; the first one wins and the loser is a warning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceSource {
    /// A path given on the command line (`--skill`, `--prompt-template`, `-e`).
    Cli,
    /// The agent directory (`~/.oxa/agent/...`).
    User,
    /// The project directory (`<cwd>/.oxa/...`); trusted projects only.
    Project,
    /// `~/.agents/skills` or a project `.agents/skills`.
    Agents,
    /// An installed package, by its source string.
    Package(String),
    /// A path listed in settings (`skills`, `prompts`, `extensions`, `themes`).
    Settings,
    /// O-152: a skill shipped inside the oxa binary (`oxa_resources::BUILTIN_SKILLS`).
    Builtin,
    /// O-152: the system skills directory (`Dirs::system_skills_dir`, Omarchy's by default).
    System,
}

/// A problem found while loading; shown once at startup and on `/reload`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl Diagnostic {
    pub fn warning(path: Option<&Path>, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level: Level::Warning,
            path: path.map(Path::to_path_buf),
            message: message.into(),
        }
    }
    pub fn error(path: Option<&Path>, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level: Level::Error,
            path: path.map(Path::to_path_buf),
            message: message.into(),
        }
    }
}

/// A context file (`AGENTS.md`, `CLAUDE.md`, `AGENTS.override.md`) as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    /// Absolute path.
    pub path: PathBuf,
    /// File content, exactly as on disk (UTF-8, lossy).
    pub content: String,
}

/// One skill (Agent Skills spec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Absolute path of `SKILL.md` (or of the standalone `.md` file).
    pub path: PathBuf,
    /// Directory that relative paths in the skill resolve against.
    pub base_dir: PathBuf,
    pub source: ResourceSource,
    /// `disable-model-invocation: true`: not listed in `<skills>`, `/skill:name` only.
    pub disable_model_invocation: bool,
}

/// One prompt template (`prompts/*.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// Command name = file stem.
    pub name: String,
    /// Frontmatter `description`, else the first non-empty body line.
    pub description: String,
    pub argument_hint: Option<String>,
    /// The body after the frontmatter, unexpanded.
    pub body: String,
    pub path: PathBuf,
    pub source: ResourceSource,
}

/// Everything discovered for one prefix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resources {
    /// In prompt order: agent dir first, then the outermost ancestor of cwd down to cwd.
    pub context_files: Vec<ContextFile>,
    /// Content of `SYSTEM.md` (project wins over user) or `--system-prompt`; replaces the
    /// preamble section.
    pub system_override: Option<String>,
    /// `APPEND_SYSTEM.md` contents (user, then project) and `--append-system-prompt` values, in
    /// that order; they form the `append` section.
    pub append_system: Vec<String>,
    /// Sorted by name.
    pub skills: Vec<Skill>,
    /// Sorted by name.
    pub templates: Vec<Template>,
    /// Extension entry files (`.js`, `.mjs`, `.ts`), in load order.
    pub extension_paths: Vec<PathBuf>,
    /// Theme JSON files, sorted.
    pub theme_paths: Vec<PathBuf>,
    /// O-152: the identity text (`<agent dir>/identity.md`, see `read_identity`); `None` =
    /// the prefix carries the generic fallback.
    pub identity: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Inputs of [`discover`] that do not come from settings or the disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscoverOpts {
    /// The project is trusted (see the crate docs for what that gates).
    pub trusted: bool,
    /// `-nc`: no context files.
    pub no_context: bool,
    /// `-ns`: no skill discovery (`extra_skills` still load).
    pub no_skills: bool,
    /// `-np`: no template discovery (`extra_templates` still load).
    pub no_templates: bool,
    /// `-ne`: no extension discovery (`extra_extensions` still load).
    pub no_extensions: bool,
    /// `--skill <path>` values.
    pub extra_skills: Vec<PathBuf>,
    /// `--prompt-template <path>` values.
    pub extra_templates: Vec<PathBuf>,
    /// `-e <path|source>` values that are local paths (package sources are resolved by
    /// `packages::package_resources`).
    pub extra_extensions: Vec<String>,
    /// `--system-prompt <text>`: wins over any `SYSTEM.md`.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt <text>` values.
    pub append_system_prompt: Vec<String>,
}

/// Discover everything for one epoch. Never fails; problems are in `Resources::diagnostics`
/// (context, packages, skills, templates: in that order).
pub fn discover(dirs: &Dirs, cwd: &Path, settings: &Settings, opts: &DiscoverOpts) -> Resources {
    let mut r = Resources {
        identity: crate::builtin::read_identity(dirs),
        ..Resources::default()
    };

    let ctx = context::load_context(dirs, cwd, opts);
    r.context_files = ctx.files;
    r.system_override = ctx.system_override;
    r.append_system = ctx.append_system;
    r.diagnostics.extend(ctx.diagnostics);

    let pk = packages::package_resources(dirs, cwd, settings, opts);
    // Extension load order (pi's): user directory, project directory (trusted only), the
    // `extensions` setting, local `-e` paths, then package entries. `-ne` drops what is
    // discovered and keeps the `-e` entries.
    r.extension_paths = local_extension_paths(dirs, cwd, settings, opts);
    for p in pk.extension_paths {
        if !r.extension_paths.contains(&p) {
            r.extension_paths.push(p);
        }
    }
    r.theme_paths = pk.theme_paths;
    r.diagnostics.extend(pk.diagnostics);

    let (sk, d) = skills::discover_skills(dirs, cwd, settings, opts, &pk.skill_paths);
    r.skills = sk;
    r.diagnostics.extend(d);

    let (tp, d) = templates::discover_templates(dirs, cwd, settings, opts, &pk.template_paths);
    r.templates = tp;
    r.diagnostics.extend(d);
    r
}

/// The extension entries that are not packages: the files and extension directories directly
/// inside `<agent dir>/extensions` and (trusted) `<cwd>/.oxa/extensions`, sorted by name; the
/// paths of the `extensions` setting (`~` = home, relative = the agent directory); the local
/// `-e` paths (`~` = home, relative = `cwd`). `opts.no_extensions` keeps only the `-e` paths.
/// A path named twice is loaded once, at its first position.
fn local_extension_paths(dirs: &Dirs, cwd: &Path, settings: &Settings, opts: &DiscoverOpts) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    let expand = |text: &str, base: &Path| -> PathBuf {
        if text == "~" {
            dirs.home.clone()
        } else if let Some(rest) = text.strip_prefix("~/") {
            dirs.home.join(rest)
        } else {
            base.join(text)
        }
    };
    if !opts.no_extensions {
        for p in extension_entries(&dirs.agent_dir.join("extensions")) {
            add(p);
        }
        if opts.trusted {
            for p in extension_entries(&cwd.join(oxa_proto::names::PROJECT_DIR).join("extensions")) {
                add(p);
            }
        }
        for text in settings.list("extensions") {
            add(expand(&text, &dirs.agent_dir));
        }
    }
    for text in &opts.extra_extensions {
        if !packages::is_package_spec(text) {
            add(expand(text, cwd));
        }
    }
    out
}

/// The extensions directly inside `dir`: script files (`.js`, `.mjs`; `.ts` too, so that the
/// loader can refuse it with its hint) and directories that hold an `index.*` entry or a
/// `package.json`. Sorted by file name; a missing directory has none.
fn extension_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            if p.is_dir() {
                ["index.js", "index.mjs", "index.ts", "package.json"]
                    .iter()
                    .any(|entry| p.join(entry).is_file())
            } else {
                matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("js") | Some("mjs") | Some("ts")
                )
            }
        })
        .collect();
    found.sort();
    found
}
