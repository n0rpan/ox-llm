//! H (THM-003, THM-004): theme files are read and checked at startup in every mode, so an
//! invalid theme is reported even when no terminal UI runs. Messages follow pi's theme schema
//! validation (`modes/interactive/theme/theme-schema.ts` `validateThemeJson`) and resource
//! loader (`core/resource-loader.ts` `loadThemes`).
use oxa_config::{Dirs, Settings};
use oxa_proto::{Level, Value};
use oxa_tui::{load_theme_file, ColorMode, THEME_TOKENS};
use std::path::{Path, PathBuf};

/// Where themes come from (pi themes.md "Load a theme"): the agent directory's `themes/`, the
/// trusted project's `.oxa/themes/`, the `themes` setting, package themes, and `--theme`
/// paths. `--no-themes` keeps only the `--theme` paths.
pub(crate) struct ThemeSources<'a> {
    pub dirs: &'a Dirs,
    pub cwd: &'a Path,
    pub settings: &'a Settings,
    pub trusted: bool,
    pub package_paths: &'a [PathBuf],
    pub cli_paths: &'a [PathBuf],
    pub no_themes: bool,
}

/// Every problem of every theme file of `src`, one notice each.
pub(crate) fn theme_diagnostics(src: &ThemeSources<'_>) -> Vec<(Level, String)> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut explicit: Vec<PathBuf> = Vec::new();
    if !src.no_themes {
        paths.push(src.dirs.agent_dir.join("themes"));
        if src.trusted {
            paths.push(src.dirs.project_dir(src.cwd).join("themes"));
        }
        for p in src.settings.list("themes") {
            explicit.push(resolve(&p, &src.dirs.home, src.cwd));
        }
        explicit.extend(src.package_paths.iter().cloned());
    }
    explicit.extend(src.cli_paths.iter().cloned());

    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in &paths {
        files.extend(json_files(dir));
    }
    for p in &explicit {
        if !p.exists() {
            out.push((
                Level::Warning,
                format!("{}: theme path does not exist", p.display()),
            ));
        } else if p.is_dir() {
            files.extend(json_files(p));
        } else if p.extension().is_some_and(|x| x == "json") {
            files.push(p.clone());
        } else {
            out.push((
                Level::Warning,
                format!("{}: theme path is not a json file", p.display()),
            ));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for f in files {
        if seen.insert(f.clone()) {
            if let Some(message) = check_theme_file(&f) {
                out.push((Level::Warning, message));
            }
        }
    }
    out
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn resolve(raw: &str, home: &Path, cwd: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.join(rest);
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// pi's message for an invalid theme file, `None` when it loads.
pub(crate) fn check_theme_file(path: &Path) -> Option<String> {
    let label = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return Some(format!("{label}: {e}")),
    };
    let json: Value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(e) => return Some(format!("Failed to parse theme {label}: {e}")),
    };
    if let Some(name) = json.get("name").and_then(Value::as_str) {
        if name.contains('/') {
            return Some(format!(
                "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
            ));
        }
    }
    // pi themes.md: `system` is reserved (a custom theme with that name is ignored); `dark`
    // and `light` are built-ins that a custom theme cannot replace (a name collision).
    if let Some(name) = json.get("name").and_then(Value::as_str) {
        if oxa_tui::BUILTIN_THEME_NAMES.contains(&name) {
            return Some(format!(
                "{label}: theme name \"{name}\" is reserved for a built-in theme; the file is ignored"
            ));
        }
    }
    let Some(colors) = json.get("colors").and_then(Value::as_object) else {
        return Some(format!(
            "Invalid theme \"{label}\": expected an object with a \"colors\" map."
        ));
    };
    let mut missing: Vec<&str> = THEME_TOKENS
        .iter()
        .filter(|(name, _, fallback)| fallback.is_none() && !colors.contains_key(*name))
        .map(|(name, _, _)| *name)
        .collect();
    missing.sort_unstable();
    let mut other: Vec<String> = colors
        .keys()
        .filter(|k| !THEME_TOKENS.iter().any(|(name, _, _)| name == k))
        .map(|k| format!("  - /colors: must NOT have additional properties ({k})"))
        .collect();
    other.sort();
    if missing.is_empty() && other.is_empty() {
        return load_theme_file(path, ColorMode::TrueColor)
            .err()
            .map(|e| e.message);
    }
    let mut message = format!("Invalid theme \"{label}\":\n");
    if !missing.is_empty() {
        message.push_str("\nMissing required color tokens:\n");
        message.push_str(
            &missing
                .iter()
                .map(|c| format!("  - {c}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        message.push_str("\n\nPlease add these colors to your theme's \"colors\" object.");
        message.push_str("\nSee the built-in themes (dark.json, light.json) for reference values.");
    }
    if !other.is_empty() {
        message.push_str(&format!("\n\nOther errors:\n{}", other.join("\n")));
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;

    #[test]
    fn invalid_and_valid_themes() {
        let env = TestEnv::new();
        let bad = env.write("t/bad.json", r##"{"name":"bad","colors":{"notARole":"#fff"}}"##);
        let msg = check_theme_file(&bad).expect("invalid");
        assert!(
            msg.starts_with(&format!("Invalid theme \"{}\":", bad.display())),
            "{msg}"
        );
        assert!(
            msg.contains("Missing required color tokens:\n  - accent"),
            "{msg}"
        );
        assert!(
            msg.contains("must NOT have additional properties (notARole)"),
            "{msg}"
        );
        let broken = env.write("t/broken.json", "{");
        assert!(check_theme_file(&broken)
            .unwrap()
            .starts_with("Failed to parse theme"));
        let slash = env.write("t/slash.json", r#"{"name":"a/b","colors":{}}"#);
        assert!(check_theme_file(&slash)
            .unwrap()
            .starts_with("Invalid theme name \"a/b\""));
        let reserved = env.write("t/system.json", r#"{"name":"system","colors":{}}"#);
        assert!(check_theme_file(&reserved).unwrap().contains("reserved"));
        let good = env.write(
            "t/good.json",
            &oxa_tui::THEME_DARK_JSON.replacen("\"name\": \"dark\"", "\"name\": \"good\"", 1),
        );
        assert_eq!(check_theme_file(&good), None);
    }
}
