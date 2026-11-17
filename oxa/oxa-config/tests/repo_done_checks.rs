//! The repository's own project settings (`<repo>/.oxa/settings.json`) declare the `done`
//! checks that an agent must pass before a run that edited files finishes (issue #106), and
//! `tools/ci/cargo.sh` makes the Rust ones runnable where the toolchain is installed but not
//! on `PATH` (issue #1063).

use oxa_config::Settings;
use serde_json::json;

#[test]
fn repo_done_list_runs_the_documented_checks() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.oxa/settings.json");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let done = Settings::from_values(value, json!({})).done_commands();
    assert!(
        done.iter().any(|c| c == "in oxa: cargo fmt --check"),
        "rustfmt check missing: {done:?}"
    );
    assert!(
        done.iter()
            .any(|c| c == "in oxc: sh tools/run_lab_tests.sh test_doc_links"),
        "lab wrapper missing: {done:?}"
    );
}

#[cfg(unix)]
mod toolchain_helper {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    /// `tools/ci/cargo.sh`, resolved from this crate.
    const HELPER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/ci/cargo.sh");

    fn exe(env: &oxa_fake::TestEnv, rel: &str, body: &str) -> PathBuf {
        let p = env.write(rel, body);
        let mut perms = std::fs::metadata(&p)
            .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).expect("set the executable bit");
        p
    }

    /// A fake toolchain `bin` directory: `cargo` prints the command line it was handed, and the
    /// two proxies the documented checks need sit beside it.
    fn toolchain(env: &oxa_fake::TestEnv, dir: &str) {
        exe(
            env,
            &format!("{dir}/cargo"),
            "#!/bin/sh\nprintf 'cargo:%s\\n' \"$*\"\n",
        );
        exe(env, &format!("{dir}/cargo-fmt"), "#!/bin/sh\nexit 0\n");
        exe(env, &format!("{dir}/cargo-clippy"), "#!/bin/sh\nexit 0\n");
    }

    /// Run the helper with `path` as the whole child `PATH`, so no toolchain leaks in.
    fn run(env: &oxa_fake::TestEnv, path: &Path, args: &[&str]) -> Output {
        // The full path: `sh` is looked up in the child's `PATH`, which is the fixture here.
        Command::new("/bin/sh")
            .arg(HELPER)
            .args(args)
            .current_dir(env.work())
            .env_clear()
            .env("HOME", env.home())
            .env("PATH", path)
            .output()
            .expect("run tools/ci/cargo.sh")
    }

    fn stdout(out: &Output) -> String {
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn empty_dir(env: &oxa_fake::TestEnv, rel: &str) -> PathBuf {
        let p = env.root().join(rel);
        std::fs::create_dir_all(&p).expect("create a fixture directory");
        p
    }

    /// Both documented local checks run from a toolchain that is not on `PATH`, with the
    /// arguments untouched (`clippy -p <crate> -- -D warnings` keeps its `--`).
    #[test]
    fn runs_the_documented_checks_from_an_installed_toolchain() {
        let env = oxa_fake::TestEnv::new();
        toolchain(&env, "home/.rustup/toolchains/test-1.0/bin");
        let none = empty_dir(&env, "emptybin");

        let fmt = run(&env, &none, &["fmt", "--check"]);
        assert!(
            fmt.status.success(),
            "fmt: {}{}",
            stdout(&fmt),
            String::from_utf8_lossy(&fmt.stderr)
        );
        assert_eq!(stdout(&fmt), "cargo:fmt --check\n");

        let clippy = run(&env, &none, &["clippy", "-p", "oxa-core", "--", "-D", "warnings"]);
        assert!(
            clippy.status.success(),
            "clippy: {}{}",
            stdout(&clippy),
            String::from_utf8_lossy(&clippy.stderr)
        );
        assert_eq!(stdout(&clippy), "cargo:clippy -p oxa-core -- -D warnings\n");
    }

    /// The reported environment: the toolchain sits in the rustup home beside the
    /// `~/.cargo/bin` that is on `PATH`, while `HOME` itself holds no toolchain.
    #[test]
    fn finds_the_rustup_home_beside_the_cargo_bin_on_path() {
        let env = oxa_fake::TestEnv::new();
        toolchain(&env, "rustup/.rustup/toolchains/test-1.0/bin");
        let proxies = empty_dir(&env, "rustup/.cargo/bin");
        let elsewhere = empty_dir(&env, "elsewhere");

        let out = Command::new("/bin/sh")
            .arg(HELPER)
            .args(["fmt", "--check"])
            .current_dir(env.work())
            .env_clear()
            .env("HOME", &elsewhere)
            .env("PATH", &proxies)
            .output()
            .expect("run tools/ci/cargo.sh");
        assert!(
            out.status.success(),
            "{}{}",
            stdout(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(stdout(&out), "cargo:fmt --check\n");
    }

    /// A missing toolchain fails the check and names what it looked for and where; it is never
    /// a silent pass.
    #[test]
    fn a_missing_toolchain_fails_and_lists_what_it_searched() {
        let env = oxa_fake::TestEnv::new();
        let none = empty_dir(&env, "emptybin");
        let out = run(&env, &none, &["fmt", "--check"]);
        assert!(
            !out.status.success(),
            "no toolchain must not pass: {}",
            stdout(&out)
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("no Rust toolchain with 'cargo-fmt' found"), "{err}");
        assert!(
            err.contains(&env.home().join(".cargo/bin").display().to_string()),
            "{err}"
        );
        assert!(
            err.contains(&env.home().join(".rustup").display().to_string()),
            "{err}"
        );
    }
}
