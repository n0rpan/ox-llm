//! B8: skills (Agent Skills spec).
// ported from pi packages/coding-agent/src/core/skills.ts
use crate::frontmatter::{frontmatter_bool, frontmatter_get, parse_frontmatter};
use crate::{Diagnostic, DiscoverOpts, ResourceSource, Skill};
use oxa_config::{Dirs, Settings};
use oxa_proto::OxaError;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub const SKILL_FILE: &str = "SKILL.md";
pub const MAX_SKILL_NAME: usize = 64;
pub const MAX_SKILL_DESCRIPTION: usize = 1024;
/// The setting that lists skills synced from another product (see [`is_synced_dir`]).
pub const SETTING_INCLUDE_SYNCED: &str = "oxa.skills.includeSynced";

/// A directory under `~/.agents/skills` that another product synced in: one named `synced`,
/// or one whose `manifest.json` lists skills by `skillId` (a sync manifest, not an Agent
/// Skills file).
pub fn is_synced_dir(dir: &Path) -> bool {
    if dir.file_name() == Some(OsStr::new("synced")) {
        return true;
    }
    std::fs::read_to_string(dir.join("manifest.json")).is_ok_and(|t| t.contains("\"skillId\""))
}

/// Discover skills. Order (first wins on a name clash, the loser is a warning):
/// `opts.extra_skills`, then (unless `opts.no_skills`) `<agent dir>/skills`, `~/.agents/skills`,
/// project `.oxa/skills` (trusted), project `.agents/skills` from `cwd` through its ancestors
/// up to the repository root (trusted), settings `skills` paths, `package_paths`, then (O-152)
/// oxa's built-in skills (unless `oxa.builtinSkills` is false) and last the system directory
/// `dirs.system_skills_dir` (Omarchy's shared skills). A built-in or system skill that loses a
/// name clash is dropped silently (overriding them is documented, not a mistake).
/// Directories containing `SKILL.md` are found recursively. The result is sorted by name.
pub fn discover_skills(
    dirs: &Dirs,
    cwd: &Path,
    settings: &Settings,
    opts: &DiscoverOpts,
    package_paths: &[(PathBuf, ResourceSource)],
) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut loader = SkillLoader::default();
    for path in &opts.extra_skills {
        loader.load_explicit(path, &ResourceSource::Cli);
    }
    if !opts.no_skills {
        loader.scan_dir(&dirs.agent_dir.join("skills"), &ResourceSource::User, true);
        // Skills another product syncs into `~/.agents/skills` (e.g. a `synced/<id>/` tree of
        // skills written for that product's own tools) are skipped unless
        // `oxa.skills.includeSynced` is true: they cost prefix tokens and invite calls to tools
        // oxa does not have.
        loader.skip_synced = !settings.bool(SETTING_INCLUDE_SYNCED, false);
        loader.scan_dir(
            &dirs.home.join(".agents").join("skills"),
            &ResourceSource::Agents,
            true,
        );
        loader.skip_synced = false;
        if opts.trusted {
            loader.scan_dir(
                &dirs.project_dir(cwd).join("skills"),
                &ResourceSource::Project,
                true,
            );
            for dir in project_agents_dirs(cwd, &dirs.home) {
                loader.scan_dir(&dir, &ResourceSource::Agents, true);
            }
        }
        for entry in settings.list("skills") {
            if entry.trim_start().starts_with('!') {
                loader.diags.push(Diagnostic::warning(
                    None,
                    format!(
                        "skills setting entry `{entry}` (exclusion) is not supported yet and was ignored"
                    ),
                ));
                continue;
            }
            let path = resolve_setting_path(&entry, &dirs.home, cwd);
            loader.load_explicit(&path, &ResourceSource::Settings);
        }
        for (path, source) in package_paths {
            loader.load_explicit(path, source);
        }
        if settings.bool(crate::builtin::SETTING_BUILTIN_SKILLS, true) {
            loader.load_builtins(dirs);
        }
        if let Some(dir) = &dirs.system_skills_dir {
            loader.scan_dir(dir, &ResourceSource::System, true);
        }
    }
    loader.finish()
}

/// Load one `SKILL.md` or standalone `.md`. Frontmatter rules warn, they do not fail, except:
/// a malformed file or a missing `description` is `Err` (not loaded), as in pi. A missing
/// `name` falls back to the parent directory name (file stem for a standalone file). Invalid
/// names and over-long fields are returned in the warning list but the skill loads.
pub fn load_skill_file(path: &Path, source: ResourceSource) -> Result<(Skill, Vec<Diagnostic>), Diagnostic> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Diagnostic::warning(Some(path), format!("failed to read skill file: {e}")))?;
    skill_from_content(path, &content, source)
}

/// [`load_skill_file`] over content already read (the built-in skills are embedded).
fn skill_from_content(
    path: &Path,
    content: &str,
    source: ResourceSource,
) -> Result<(Skill, Vec<Diagnostic>), Diagnostic> {
    let (pairs, _body) = parse_frontmatter(content);
    let description = match frontmatter_get(&pairs, "description") {
        Some(d) if !d.trim().is_empty() => d.to_string(),
        _ => return Err(Diagnostic::warning(Some(path), "description is required")),
    };
    let fallback = if is_declared(path) {
        path.parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_string()
    } else {
        path.file_stem().and_then(OsStr::to_str).unwrap_or("").to_string()
    };
    let name = match frontmatter_get(&pairs, "name") {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => fallback,
    };

    let mut diags: Vec<Diagnostic> = validate_skill_name(&name)
        .into_iter()
        .map(|problem| Diagnostic::warning(Some(path), problem))
        .collect();
    let desc_len = utf16_len(&description);
    if desc_len > MAX_SKILL_DESCRIPTION {
        diags.push(Diagnostic::warning(
            Some(path),
            format!("description exceeds {MAX_SKILL_DESCRIPTION} characters ({desc_len})"),
        ));
    }

    let skill = Skill {
        name,
        description,
        path: path.to_path_buf(),
        base_dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        source,
        disable_model_invocation: frontmatter_bool(&pairs, "disable-model-invocation"),
    };
    Ok((skill, diags))
}

/// Check a skill name against the spec: lowercase letters, digits and hyphens; no leading,
/// trailing or consecutive hyphens; at most 64 characters. Returns the problems (empty = ok).
pub fn validate_skill_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let len = utf16_len(name);
    if len > MAX_SKILL_NAME {
        errors.push(format!("name exceeds {MAX_SKILL_NAME} characters ({len})"));
    }
    let charset_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !charset_ok {
        errors
            .push("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

/// The `skills` prefix section: pi's `<skills>` text for the skills that allow model
/// invocation, in the given order. Returns the empty string when there are none (the section
/// is then omitted). Pure; these bytes are part of the frozen prefix (W3).
///
/// Same bytes as `oxa_core::prompt::skills_section` fed with `(name, description, path)` of each
/// visible skill (oxa-resources cannot depend on oxa-core, so the format is repeated here).
pub fn skills_section(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|s| !s.disable_model_invocation).collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls."
            .to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    format!("<skills>\n{}\n</skills>", lines.join("\n"))
}

/// The user message for `/skill:<name> <args>`: the skill's instructions as pi wraps them,
/// with `args` appended as a user request when non-empty. Reads the skill file now.
pub fn expand_skill_command(skill: &Skill, args: &str) -> Result<String, OxaError> {
    // A built-in skill whose file is not written yet expands from the embedded copy.
    let embedded = (skill.source == ResourceSource::Builtin)
        .then(|| {
            crate::builtin::BUILTIN_SKILLS
                .iter()
                .find(|(n, _)| *n == skill.name)
        })
        .flatten()
        .map(|(_, c)| c.to_string());
    let content = match std::fs::read_to_string(&skill.path) {
        Ok(c) => c,
        Err(_) if embedded.is_some() => embedded.unwrap_or_default(),
        Err(e) => {
            return Err(OxaError::io(
                &format!("failed to read skill {}", skill.path.display()),
                &e,
            ))
        }
    };
    let (_pairs, body) = parse_frontmatter(&content);
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.path.to_string_lossy(),
        skill.base_dir.to_string_lossy(),
        body.trim(),
    );
    let args = args.trim();
    if args.is_empty() {
        Ok(block)
    } else {
        Ok(format!("{block}\n\n{args}"))
    }
}

// ---- internal helpers (shared with templates.rs) -----------------------------------------

/// Accumulates skills in discovery order; keeps the first skill of each name.
#[derive(Default)]
struct SkillLoader {
    found: Vec<Skill>,
    diags: Vec<Diagnostic>,
    /// Canonical paths of files and directories already visited (symlink aliases, cycles).
    seen: HashSet<PathBuf>,
    /// Skip [`is_synced_dir`] subtrees (set while `~/.agents/skills` is scanned).
    skip_synced: bool,
}

impl SkillLoader {
    /// An explicit path (`--skill`, settings, package): a directory is scanned, a `.md` file is
    /// loaded, anything else is a warning. A missing path is a warning, as in pi.
    fn load_explicit(&mut self, path: &Path, source: &ResourceSource) {
        if !path.exists() {
            self.diags
                .push(Diagnostic::warning(Some(path), "skill path does not exist"));
            return;
        }
        if path.is_dir() {
            self.scan_dir(path, source, true);
        } else if path.is_file() && path.to_string_lossy().ends_with(".md") {
            self.load_file(path, source);
        } else {
            self.diags.push(Diagnostic::warning(
                Some(path),
                "skill path is not a markdown file",
            ));
        }
    }

    /// pi's rules: a directory holding `SKILL.md` is a skill root (no further recursion);
    /// otherwise its direct `.md` children are standalone skills (top level only) and its
    /// subdirectories are scanned for `SKILL.md`. Dot-entries and `node_modules` are skipped.
    fn scan_dir(&mut self, dir: &Path, source: &ResourceSource, include_root_files: bool) {
        if !dir.is_dir() || !self.seen.insert(canonical_path(dir)) {
            return;
        }
        let skill_md = dir.join(SKILL_FILE);
        if skill_md.is_file() {
            self.load_file(&skill_md, source);
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for entry in entries {
            let Some(name) = entry.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            if entry.is_dir() {
                if self.skip_synced && is_synced_dir(&entry) {
                    continue;
                }
                self.scan_dir(&entry, source, false);
            } else if include_root_files && entry.is_file() && name.ends_with(".md") {
                self.load_file(&entry, source);
            }
        }
    }

    /// The embedded built-in skills at their install paths (the files need not exist yet).
    fn load_builtins(&mut self, dirs: &Dirs) {
        for (name, content) in crate::builtin::BUILTIN_SKILLS {
            let path = crate::builtin::builtin_skill_path(dirs, name);
            if !self.seen.insert(canonical_path(&path)) {
                continue;
            }
            match skill_from_content(&path, content, ResourceSource::Builtin) {
                Ok((skill, diags)) => {
                    self.found.push(skill);
                    self.diags.extend(diags);
                }
                Err(diag) => self.diags.push(diag),
            }
        }
    }

    fn load_file(&mut self, path: &Path, source: &ResourceSource) {
        if !self.seen.insert(canonical_path(path)) {
            return;
        }
        // pi: a standalone `.md` without a description is not a skill; skipped silently, so
        // README.md and friends in a skills directory do not produce warnings.
        if !is_declared(path) {
            if let Ok(content) = std::fs::read_to_string(path) {
                if !has_description(&content) {
                    return;
                }
            }
        }
        match load_skill_file(path, source.clone()) {
            Ok((skill, diags)) => {
                self.found.push(skill);
                self.diags.extend(diags);
            }
            Err(diag) => self.diags.push(diag),
        }
    }

    /// First skill of each name wins; each later one is a collision warning. Sorted by name.
    fn finish(self) -> (Vec<Skill>, Vec<Diagnostic>) {
        let mut diags = self.diags;
        let mut kept: Vec<Skill> = Vec::new();
        for skill in self.found {
            if let Some(winner) = kept.iter().find(|k| k.name == skill.name) {
                if matches!(skill.source, ResourceSource::Builtin | ResourceSource::System) {
                    continue;
                }
                diags.push(Diagnostic::warning(
                    Some(skill.path.as_path()),
                    format!(
                        "name \"{}\" collision, kept {}",
                        skill.name,
                        winner.path.display()
                    ),
                ));
            } else {
                kept.push(skill);
            }
        }
        kept.sort_by(|a, b| a.name.cmp(&b.name));
        (kept, diags)
    }
}

/// `<cwd>/.agents/skills` and each ancestor's, nearest first, stopping at the repository root
/// (the first directory holding `.git`), or at the filesystem root outside a repository (pi),
/// and never above or outside `home`; the home's own `.agents/skills` is user-level (CT-10).
fn project_agents_dirs(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in oxa_proto::guard::project_ancestors(cwd, home) {
        if dir == home {
            break;
        }
        out.push(dir.join(".agents").join("skills"));
        if dir.join(".git").exists() {
            break;
        }
    }
    out
}

/// A settings path entry: trimmed, `~` expanded to the home directory, relative to `cwd`.
pub(crate) fn resolve_setting_path(entry: &str, home: &Path, cwd: &Path) -> PathBuf {
    let entry = entry.trim();
    let expanded = if entry == "~" {
        home.to_path_buf()
    } else if let Some(rest) = entry.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(entry)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// The path with symlinks resolved, or the path itself when it cannot be resolved.
pub(crate) fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A file named `SKILL.md` is a declared skill; any other `.md` is a standalone skill.
fn is_declared(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(SKILL_FILE))
}

fn has_description(content: &str) -> bool {
    let (pairs, _) = parse_frontmatter(content);
    frontmatter_get(&pairs, "description").is_some_and(|d| !d.trim().is_empty())
}

/// JavaScript `string.length` (UTF-16 code units), as pi counts the limits.
pub(crate) fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// pi's `escapeXml`.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
