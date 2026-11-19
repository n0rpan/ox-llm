//! E10 acceptance tests: Packages.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use oxa_proto::codes;
use oxa_resources::{
    apply_filter, declared_packages, package_resources, parse_source, read_manifest, DiscoverOpts,
    PackageDecl, PackageManager, PackageSource, ResourceSource, Scope, SystemRunner,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run git in `cwd` with a fixed identity and no signing (fixtures only).
fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "oxa test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "oxa test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, content).expect("write fixture");
}

fn write_json(path: &Path, value: &Value) {
    write(path, &serde_json::to_string_pretty(value).expect("json"));
}

/// A package tree: package.json, one extension, one skill, one prompt, one theme.
fn write_package(dir: &Path, name: &str, version: &str) {
    write_json(
        &dir.join("package.json"),
        &json!({"name": name, "version": version}),
    );
    write(&dir.join("extensions/e.js"), "export default function () {}\n");
    write(
        &dir.join("skills/s/SKILL.md"),
        "---\nname: s\ndescription: a skill\n---\nbody\n",
    );
    write(&dir.join("prompts/p.md"), "Say $1\n");
    write(&dir.join("themes/t.json"), "{}\n");
}

/// A git repository with one commit holding a package tree. Returns its path.
fn make_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join("origin").join(name);
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q"]);
    write_package(&repo, name, "1.0.0");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn file_url(repo: &Path) -> String {
    format!("file://{}", repo.display())
}

fn manager(dirs: &Dirs, cwd: &Path) -> PackageManager {
    PackageManager::new(dirs, cwd, Box::new(SystemRunner))
}

fn no_settings() -> Settings {
    Settings::from_values(json!({}), json!({}))
}

/// clone at ref into the packages dir
#[test]
fn install_git() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let repo = make_repo(env.root(), "gitpkg");
    let mgr = manager(&dirs, &cwd);

    let info = mgr.install(&file_url(&repo), Scope::User).expect("install");
    assert!(info.installed);
    assert!(
        info.root.starts_with(dirs.agent_dir.join("git")),
        "clone goes under <agent dir>/git: {}",
        info.root.display()
    );
    assert!(info.root.join("extensions/e.js").is_file());
    assert_eq!(info.manifest.as_ref().map(|m| m.name.as_str()), Some("gitpkg"));

    // A ref is checked out: tag the first commit, move on, install at the tag.
    git(&repo, &["tag", "v1"]);
    write(&repo.join("later.txt"), "later\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "second"]);
    let at_tag = mgr
        .install(&format!("{}@v1", file_url(&repo)), Scope::User)
        .expect("install at ref");
    assert!(at_tag.root.join("extensions/e.js").is_file());
    assert!(!at_tag.root.join("later.txt").exists(), "checkout is at the tag");
}

/// path reference
#[test]
fn install_local() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let pkg = env.root().join("pkgs/local-pack");
    write_package(&pkg, "local-pack", "0.1.0");
    let mgr = manager(&dirs, &cwd);

    let info = mgr
        .install(&pkg.display().to_string(), Scope::User)
        .expect("install local");
    assert!(info.installed);
    assert_eq!(info.root, pkg, "a local package is loaded in place");
    assert!(!dirs.agent_dir.join("git").exists(), "nothing is copied");
    assert!(matches!(info.source, PackageSource::Local(_)));

    let missing = env.root().join("pkgs/missing");
    let err = mgr
        .install(&missing.display().to_string(), Scope::User)
        .expect_err("missing directory");
    assert_eq!(err.code, codes::INVALID);
}

/// settings updated
#[test]
fn remove_and_list() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let repo = make_repo(env.root(), "removable");
    let src = file_url(&repo);
    let mut settings = no_settings();
    let mgr = manager(&dirs, &cwd);

    mgr.install(&src, Scope::User).expect("install");
    settings
        .set(&dirs, &cwd, "packages", json!([src.clone()]), false)
        .expect("declare");

    let decls = declared_packages(&dirs, &cwd, &settings, false);
    assert_eq!(decls.len(), 1);
    let listed = mgr.list(&decls);
    assert_eq!(listed.len(), 1);
    assert!(listed[0].installed);
    assert_eq!(listed[0].scope, Scope::User);

    // remove: files go, the declaration is removed from the settings file.
    assert!(mgr.remove(&src, Scope::User).expect("remove"));
    assert!(!listed[0].root.exists());
    settings
        .set(&dirs, &cwd, "packages", json!([]), false)
        .expect("undeclare");
    let reloaded = Settings::load(&dirs, &cwd, false);
    assert!(declared_packages(&dirs, &cwd, &reloaded, false).is_empty());
    let after = mgr.list(&declared_packages(&dirs, &cwd, &reloaded, false));
    assert!(after.is_empty());
    assert!(!mgr.remove(&src, Scope::User).expect("second remove"));
}

/// fetch and checkout
#[test]
fn update_git() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let repo = make_repo(env.root(), "moving");
    let mgr = manager(&dirs, &cwd);
    let decl = (
        Scope::User,
        PackageDecl {
            source: file_url(&repo),
            ..PackageDecl::default()
        },
    );
    let (lines, diags) = mgr.update(std::slice::from_ref(&decl));
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(lines.len(), 1, "a missing package is installed: {lines:?}");
    let root = mgr.list(std::slice::from_ref(&decl))[0].root.clone();
    assert!(!root.join("new.txt").exists());

    write(&repo.join("new.txt"), "new\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "new commit"]);
    let (lines, diags) = mgr.update(std::slice::from_ref(&decl));
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(lines.len(), 1, "the head moved: {lines:?}");
    assert!(lines[0].starts_with("updated "), "{lines:?}");
    assert!(root.join("new.txt").is_file(), "fetched and checked out");

    let (lines, _) = mgr.update(std::slice::from_ref(&decl));
    assert!(lines.is_empty(), "nothing to do the second time");

    // A pinned ref never moves.
    let pinned = (
        Scope::User,
        PackageDecl {
            source: format!("{}@v1", file_url(&repo)),
            ..PackageDecl::default()
        },
    );
    git(&repo, &["tag", "v1"]);
    let (lines, diags) = mgr.update(std::slice::from_ref(&pinned));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(lines.is_empty(), "a pinned ref never moves: {lines:?}");
}

/// `!x +x -x` resource filters
#[test]
fn filters() {
    let all: Vec<String> = [
        "extensions/a.ts",
        "extensions/b.ts",
        "extensions/sub/c.ts",
        "skills/s/SKILL.md",
        "skills/s/x.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let f = |list: &[&str]| -> Vec<String> {
        let owned: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        apply_filter(&all, Some(&owned))
    };
    // None = everything; Some([]) = nothing.
    assert_eq!(apply_filter(&all, None), all);
    assert!(apply_filter(&all, Some(&[])).is_empty());
    // plain entry = include glob (single segment star does not cross '/')
    assert_eq!(
        f(&["extensions/*.ts"]),
        vec!["extensions/a.ts", "extensions/b.ts"]
    );
    // ** crosses directories
    assert_eq!(
        f(&["extensions/**/*.ts"]),
        vec!["extensions/a.ts", "extensions/b.ts", "extensions/sub/c.ts"]
    );
    // !name excludes by file name too
    assert_eq!(f(&["extensions/*.ts", "!a.ts"]), vec!["extensions/b.ts"]);
    // -path force-excludes one exact path
    assert_eq!(
        f(&["extensions/*.ts", "-extensions/a.ts"]),
        vec!["extensions/b.ts"]
    );
    // +path force-includes after excludes
    assert_eq!(
        f(&["extensions/*.ts", "!extensions/b.ts", "+extensions/b.ts"]),
        vec!["extensions/a.ts", "extensions/b.ts"]
    );
    // +path alone adds nothing outside `all`, and with no includes everything stays
    assert_eq!(f(&["+skills/s/x.md"]), all);
    // only excludes: everything else stays
    assert_eq!(
        f(&["!extensions/*.ts"]),
        vec!["extensions/sub/c.ts", "skills/s/SKILL.md", "skills/s/x.md"]
    );
    // a plain directory name selects the files under it
    assert_eq!(f(&["skills/s"]), vec!["skills/s/SKILL.md", "skills/s/x.md"]);
    // a path not in `all` never appears
    assert!(f(&["nowhere/*.ts"]).is_empty());
}

/// project settings
#[test]
fn local_flag() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let repo = make_repo(env.root(), "projectpkg");
    let src = file_url(&repo);
    let mut settings = no_settings();
    let mgr = manager(&dirs, &cwd);

    let info = mgr.install(&src, Scope::Project).expect("project install");
    assert!(
        info.root.starts_with(cwd.join(".oxa/git")),
        "{}",
        info.root.display()
    );
    assert!(info.root.join("extensions/e.js").is_file());
    assert!(!dirs.agent_dir.join("git").exists(), "user storage untouched");

    // Declared in the project file: only a trusted project's declarations count.
    settings
        .set(&dirs, &cwd, "packages", json!([src.clone()]), true)
        .expect("project declaration");
    let reloaded = Settings::load(&dirs, &cwd, true);
    assert!(declared_packages(&dirs, &cwd, &reloaded, false).is_empty());
    let trusted = declared_packages(&dirs, &cwd, &reloaded, true);
    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0].0, Scope::Project);
    assert!(mgr.list(&trusted)[0].installed);
}

/// O-175: an `npm:` source is refused with one clear error, before anything runs.
#[test]
fn npm_source_unsupported() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let mgr = manager(&dirs, &cwd);
    for spec in ["npm:demo-pack@1.2.3", "npm:@scope/pack", "npm:"] {
        let err = mgr.install(spec, Scope::User).expect_err("npm source");
        assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA, "{spec}");
        assert_eq!(
            err.message, "npm package sources are not supported; use a git URL or a local path",
            "{spec}"
        );
    }
    assert!(!dirs.agent_dir.join("npm").exists());
}

/// unsupported_in_oxa
#[test]
fn self_update_unsupported() {
    let env = TestEnv::new();
    for word in ["self", "pi"] {
        let err = parse_source(word, env.root()).expect_err("self update");
        assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA, "{word}");
    }
    assert_eq!(
        parse_source("bogus-word", env.root())
            .expect_err("no scheme")
            .code,
        codes::INVALID
    );
}

/// extensions, skills, prompts, themes
#[test]
fn package_resources_discovered() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let user_pkg = env.root().join("pkgs/user-pack");
    let project_pkg = env.root().join("pkgs/project-pack");
    write_package(&user_pkg, "user-pack", "0.1.0");
    write_package(&project_pkg, "project-pack", "0.2.0");
    write_json(
        &dirs.settings_file(),
        &json!({ "packages": [ user_pkg.display().to_string() ] }),
    );
    write_json(
        &dirs.project_settings_file(&cwd),
        &json!({ "packages": [ { "source": project_pkg.display().to_string(), "skills": [] } ] }),
    );

    // Untrusted: the project package does not load.
    let settings = Settings::load(&dirs, &cwd, false);
    let res = package_resources(&dirs, &cwd, &settings, &DiscoverOpts::default());
    assert_eq!(res.extension_paths, vec![user_pkg.join("extensions/e.js")]);
    assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);

    // Trusted: user then project; the project filter `skills: []` drops its skills.
    let opts = DiscoverOpts {
        trusted: true,
        ..DiscoverOpts::default()
    };
    let settings = Settings::load(&dirs, &cwd, true);
    let res = package_resources(&dirs, &cwd, &settings, &opts);
    assert_eq!(
        res.extension_paths,
        vec![
            user_pkg.join("extensions/e.js"),
            project_pkg.join("extensions/e.js")
        ]
    );
    assert_eq!(
        res.skill_paths,
        vec![(
            user_pkg.join("skills/s/SKILL.md"),
            ResourceSource::Package(user_pkg.display().to_string())
        )]
    );
    assert_eq!(
        res.template_paths,
        vec![
            (
                user_pkg.join("prompts/p.md"),
                ResourceSource::Package(user_pkg.display().to_string())
            ),
            (
                project_pkg.join("prompts/p.md"),
                ResourceSource::Package(project_pkg.display().to_string())
            ),
        ]
    );
    let mut themes = vec![user_pkg.join("themes/t.json"), project_pkg.join("themes/t.json")];
    themes.sort();
    assert_eq!(res.theme_paths, themes);
    assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);

    // A declared package that is not on disk is a warning, not a failure.
    write_json(
        &dirs.settings_file(),
        &json!({ "packages": [ env.root().join("pkgs/gone").display().to_string() ] }),
    );
    let settings = Settings::load(&dirs, &cwd, false);
    let res = package_resources(&dirs, &cwd, &settings, &DiscoverOpts::default());
    assert!(res.extension_paths.is_empty());
    assert_eq!(res.diagnostics.len(), 1);
    assert!(res.diagnostics[0].message.contains("not installed"));
}

/// Extra checks beyond the acceptance bullets.
#[test]
fn source_forms_and_identity() {
    let base = Path::new("/base");
    let short = parse_source("git:github.com/user/repo@v2", base).expect("git short");
    assert_eq!(
        short,
        PackageSource::Git {
            url: "https://github.com/user/repo".to_string(),
            reference: Some("v2".to_string())
        }
    );
    assert_eq!(short.identity(), "git:github.com/user/repo");

    let scp = parse_source("git@github.com:user/repo.git", base).expect("scp");
    assert_eq!(scp.identity(), "git:github.com/user/repo");
    assert!(!scp.pinned());

    let rel = parse_source("./pkg", base).expect("relative local");
    assert_eq!(rel, PackageSource::Local(base.join("./pkg")));
    assert!(rel.pinned());

    assert_eq!(
        parse_source("git:github.com/../x", base)
            .expect_err("escape")
            .code,
        codes::INVALID
    );
}

/// The manifest: `oxa` key wins, conventional directories otherwise.
#[test]
fn manifest_keys_and_defaults() {
    let env = TestEnv::new();
    let root = env.root().join("pkgs/manifest");
    write_package(&root, "manifest", "1.0.0");
    write(&root.join("tools/t.js"), "");
    write_json(
        &root.join("package.json"),
        &json!({"name": "manifest", "pi": {"extensions": ["tools/*.js"]}}),
    );
    let m = read_manifest(&root).expect("manifest");
    assert_eq!(
        m.extensions,
        vec!["tools/t.js"],
        "declared field replaces the default"
    );
    assert_eq!(
        m.skills,
        vec!["skills/s/SKILL.md"],
        "other fields keep the defaults"
    );
    assert_eq!(m.themes, vec!["themes/t.json"]);

    write(&root.join("package.json"), "{ not json");
    assert_eq!(read_manifest(&root).expect_err("bad json").code, codes::INVALID);
}
