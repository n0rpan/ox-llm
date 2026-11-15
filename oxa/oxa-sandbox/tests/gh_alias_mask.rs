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
