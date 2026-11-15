//! #60: the gh token store is masked at every visible alias of the config directory.
//! Synthetic data only. Real-bwrap checks skip (with a notice) when bwrap is unusable.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{bwrap_args, Bwrap, Layout};
use std::path::{Path, PathBuf};
use std::process::Command;

const SENTINEL: &str = "SYNTH-GH-TOKEN-SENTINEL";

fn spec(command: &str, cwd: &Path) -> ExecSpec {
    ExecSpec {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        writable: true,
        net: true,
        timeout_s: None,
        env: Vec::new(),
    }
}

fn bwrap_path() -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    let found = path
        .split(':')
        .filter(|d| d.starts_with('/'))
        .map(|d| Path::new(d).join("bwrap"))
        .find(|p| p.is_file())?;
    let ok = Command::new(&found)
        .args(["--unshare-all", "--ro-bind", "/", "/", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    ok.then_some(found)
}

fn sandbox(env: &TestEnv) -> Option<Bwrap> {
    let Some(bwrap) = bwrap_path() else {
        assert!(
            !std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1"),
            "OXA_REQUIRE_BWRAP=1 but bwrap is not usable"
        );
        eprintln!("gh_alias_mask SKIPPED: bwrap is not usable");
        return None;
    };
    Some(Bwrap {
        bwrap,
        home: env.home(),
        layout: Layout::default(),
    })
}

fn run(sb: &Bwrap, env: &TestEnv, cmd: &str) -> String {
    let res = sb.run(&spec(cmd, &env.work()), &CancelToken::new(), &mut |_| {});
    String::from_utf8_lossy(&res.output).into_owned()
}

fn symlink_setup(env: &TestEnv) -> (PathBuf, PathBuf) {
    env.write("work/gh-config/hosts.yml", SENTINEL);
    env.write("work/gh-config/config.yml", "editor: vi");
    std::fs::create_dir_all(env.home().join(".config")).unwrap();
    let gh = env.home().join(".config/gh");
    std::os::unix::fs::symlink(env.work().join("gh-config"), &gh).unwrap();
    (gh, env.work().join("gh-config"))
}

#[test]
fn symlinked_gh_source_inside_cwd_is_masked_at_both_paths_in_argv() {
    let env = TestEnv::new();
    let (_gh, src) = symlink_setup(&env);
    let a = bwrap_args(&spec("true", &env.work()), &env.home());
    let hy = std::fs::canonicalize(&src)
        .unwrap()
        .join("hosts.yml")
        .to_string_lossy()
        .into_owned();
    let cwd = std::fs::canonicalize(env.work())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let bind = a
        .windows(3)
        .position(|w| w[0] == "--bind" && w[1] == cwd && w[2] == cwd)
        .expect("cwd bind");
    let mask = a
        .windows(3)
        .position(|w| w[0] == "--ro-bind" && w[1].ends_with("/empty") && w[2] == hy)
        .unwrap_or_else(|| panic!("no empty overlay at {hy}: {a:?}"));
    assert!(bind < mask, "{a:?}");
}

#[test]
fn symlinked_gh_source_token_is_unreadable_via_either_path() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    let (gh, src) = symlink_setup(&env);
    let cmd = format!(
        "cat '{}/hosts.yml' '{}/hosts.yml' 2>&1; true",
        gh.display(),
        src.display()
    );
    let out = run(&sb, &env, &cmd);
    assert!(!out.contains(SENTINEL), "token leaked: {out}");
}

#[test]
fn ordinary_gh_config_still_opens_and_hides_the_token() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    env.write("home/.config/gh/hosts.yml", SENTINEL);
    env.write("home/.config/gh/config.yml", "editor: vi");
    let gh = env.home().join(".config/gh");
    let a = bwrap_args(&spec("true", &env.work()), &env.home());
    assert!(a.iter().any(|x| *x == gh.to_string_lossy()), "{a:?}");
    let out = run(
        &sb,
        &env,
        &format!("cat '{0}/config.yml' '{0}/hosts.yml' 2>&1; true", gh.display()),
    );
    assert!(out.contains("editor: vi"), "{out}");
    assert!(!out.contains(SENTINEL), "{out}");
}

/// Codex review of #60: an ordinary cwd INSIDE `$HOME` (`~/work`) whose `gh` config directory is
/// a symlink into the cwd. The source then lies under the hidden home but is shown through the
/// cwd bind, so the empty file must go over it there too.
fn inside_home_setup(env: &TestEnv) -> (PathBuf, PathBuf, PathBuf) {
    let cwd = env.home().join("proj");
    let src = cwd.join("gh-config");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("hosts.yml"), SENTINEL).unwrap();
    std::fs::write(src.join("config.yml"), "editor: vi").unwrap();
    std::fs::create_dir_all(env.home().join(".config")).unwrap();
    let gh = env.home().join(".config/gh");
    std::os::unix::fs::symlink(&src, &gh).unwrap();
    (cwd, gh, src)
}

#[test]
fn gh_source_inside_a_cwd_under_home_is_masked_after_the_cwd_bind() {
    let env = TestEnv::new();
    let (cwd, _gh, src) = inside_home_setup(&env);
    let a = bwrap_args(&spec("true", &cwd), &env.home());
    let cwd_c = std::fs::canonicalize(&cwd)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let hy = std::fs::canonicalize(&src)
        .unwrap()
        .join("hosts.yml")
        .to_string_lossy()
        .into_owned();
    let bind = a
        .windows(3)
        .position(|w| w[0] == "--bind" && w[1] == cwd_c && w[2] == cwd_c)
        .expect("cwd bind");
    let mask = a
        .windows(3)
        .position(|w| w[0] == "--ro-bind" && w[1].ends_with("/empty") && w[2] == hy)
        .unwrap_or_else(|| panic!("no empty overlay at {hy}: {a:?}"));
    assert!(bind < mask, "mask must come after the cwd bind: {a:?}");
}

#[test]
fn gh_source_inside_a_cwd_under_home_is_unreadable_in_a_real_sandbox() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    let (cwd, gh, src) = inside_home_setup(&env);
    let cmd = format!(
        "cat '{}/hosts.yml' '{}/hosts.yml' 2>&1; true",
        gh.display(),
        src.display()
    );
    let res = sb.run(&spec(&cmd, &cwd), &CancelToken::new(), &mut |_| {});
    let out = String::from_utf8_lossy(&res.output).into_owned();
    assert!(!out.contains(SENTINEL), "token leaked: {out}");
}

/// N28: every gh token-store mask comes after ALL binds that could cover it.
fn assert_no_bind_after_gh_masks(a: &[String]) {
    let masks: Vec<usize> = (0..a.len().saturating_sub(2))
        .filter(|&i| a[i] == "--ro-bind" && a[i + 1].ends_with("/empty") && a[i + 2].ends_with("/hosts.yml"))
        .collect();
    assert!(!masks.is_empty(), "no gh mask in {a:?}");
    for m in masks {
        let dst = Path::new(&a[m + 2]);
        for j in m + 3..a.len().saturating_sub(2) {
            if matches!(a[j].as_str(), "--bind" | "--ro-bind" | "--tmpfs") {
                let d = if a[j] == "--tmpfs" { &a[j + 1] } else { &a[j + 2] };
                assert!(
                    !dst.starts_with(Path::new(d)),
                    "bind {d} after mask {dst:?}: {a:?}"
                );
            }
        }
    }
}

#[test]
fn no_bind_covering_a_gh_mask_follows_it() {
    let env = TestEnv::new();
    let (_gh, _src) = symlink_setup(&env);
    std::fs::create_dir_all(env.work().join(".git")).unwrap();
    assert_no_bind_after_gh_masks(&bwrap_args(&spec("true", &env.work()), &env.home()));
    let env2 = TestEnv::new();
    let (cwd, _g, _s) = inside_home_setup(&env2);
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    assert_no_bind_after_gh_masks(&bwrap_args(&spec("true", &cwd), &env2.home()));
}

#[test]
fn gh_source_under_cwd_dot_git_is_masked_after_the_dot_git_bind() {
    let env = TestEnv::new();
    let src = env.work().join(".git/ghcfg");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("hosts.yml"), SENTINEL).unwrap();
    std::fs::write(src.join("config.yml"), "editor: vi").unwrap();
    std::fs::create_dir_all(env.home().join(".config")).unwrap();
    std::os::unix::fs::symlink(&src, env.home().join(".config/gh")).unwrap();
    let a = bwrap_args(&spec("true", &env.work()), &env.home());
    assert_no_bind_after_gh_masks(&a);
    let Some(sb) = sandbox(&env) else { return };
    let cmd = format!("cat '{0}/hosts.yml' 2>&1; true", src.display());
    let out = run(&sb, &env, &cmd);
    assert!(!out.contains(SENTINEL), "token leaked: {out}");
}
