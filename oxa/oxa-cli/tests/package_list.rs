//! Package listing must explain the project trust gate without bypassing it.
#[cfg(target_os = "linux")]
#[path = "common/strace.rs"]
mod strace;
use oxa_cli::run_pkg;
use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use oxa_perm::TrustStore;
use oxa_resources::{declared_packages, Scope};

fn command(env: &TestEnv, verb: &str, words: &[&str]) -> String {
    let dirs = Dirs::under(env.root());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let words = words.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        run_pkg(verb, &words, &dirs, &env.work(), &mut out, &mut err).unwrap(),
        0
    );
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

#[test]
fn package_list_explains_hidden_project_declarations_after_install() {
    let env = TestEnv::new();
    env.write(
        "package/skills/synthetic/SKILL.md",
        "---\nname: synthetic\ndescription: Synthetic fixture.\n---\nReturn a short answer.\n",
    );
    let source = env.root().join("package");
    let installed = command(&env, "install", &["-l", source.to_str().unwrap()]);
    assert!(installed.contains("(project)"), "{installed}");
    let dirs = Dirs::under(env.root());
    let settings_before = std::fs::read(dirs.project_settings_file(&env.work())).unwrap();
    for words in [&[][..], &["-l"][..]] {
        let listed = command(&env, "list", words);
        assert!(listed.contains("project is not trusted"), "{listed}");
        assert!(listed.contains("run oxa once in it to decide"), "{listed}");
        assert!(!listed.contains("No packages declared."), "{listed}");
        assert!(!listed.contains("installed at"), "{listed}");
    }
    assert_eq!(
        std::fs::read(dirs.project_settings_file(&env.work())).unwrap(),
        settings_before
    );
    assert!(!dirs.trust_file().exists(), "listing must not grant trust");
    let settings = Settings::load(&dirs, &env.work(), false);
    assert!(declared_packages(&dirs, &env.work(), &settings, false).is_empty());

    // Once explicitly trusted and content-approved, the normal installed state is shown.
    let mut trust = TrustStore::load(&dirs.trust_file());
    trust.save(&dirs.trust_file(), &env.work(), true).unwrap();
    trust.approve(&dirs.trust_file(), &env.work()).unwrap();
    assert!(trust.trusted_as_approved(&env.work()));
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("project "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(!listed.contains("hidden"), "{listed}");
}

#[test]
fn package_list_keeps_user_packages_visible_and_empty_state_accurate() {
    let env = TestEnv::new();
    // Without reading untrusted settings we cannot distinguish empty from hidden.
    let untrusted_empty = command(&env, "list", &[]);
    assert!(untrusted_empty.contains("if any"), "{untrusted_empty}");
    assert!(
        untrusted_empty.contains("project is not trusted"),
        "{untrusted_empty}"
    );
    env.write("work/.oxa/settings.json", r#"{"packages":[]}"#);
    assert_eq!(command(&env, "list", &["-l"]), untrusted_empty);
    env.write("package/skills/synthetic/SKILL.md", "Synthetic skill.");
    let source = env.root().join("package");
    command(&env, "install", &[source.to_str().unwrap()]);
    command(&env, "install", &["-l", source.to_str().unwrap()]);
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("user "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(listed.contains("project is not trusted"), "{listed}");
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), false);
    let visible = declared_packages(&dirs, &env.work(), &settings, false);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].0, Scope::User);
    command(&env, "remove", &[source.to_str().unwrap()]);
    command(&env, "remove", &["-l", source.to_str().unwrap()]);
    assert_eq!(command(&env, "list", &[]), untrusted_empty);
    let mut trust = TrustStore::load(&dirs.trust_file());
    trust.save(&dirs.trust_file(), &env.work(), true).unwrap();
    trust.approve(&dirs.trust_file(), &env.work()).unwrap();
    assert_eq!(command(&env, "list", &[]), "No packages declared.\n");
}

// Run only the reporting path in the traced subprocess, not fixture creation or checks.
#[test]
fn package_list_untrusted_settings_worker() {
    let Some(root) = std::env::var_os("OXA_PACKAGE_LIST_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let dirs = Dirs::under(&root);
    for words in [vec![], vec!["-l".to_string()]] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_pkg("list", &words, &dirs, &root.join("work"), &mut out, &mut err).unwrap(),
            0
        );
        let listed = String::from_utf8(out).unwrap();
        assert!(listed.contains("project is not trusted"), "{listed}");
        assert!(!listed.contains("installed at"), "{listed}");
        assert!(err.is_empty());
    }
    assert!(!dirs.trust_file().exists());
}

#[cfg(target_os = "linux")]
#[test]
fn package_list_does_not_open_untrusted_special_or_oversized_settings() {
    use std::{fs, os::unix::fs::symlink, process::Command};

    for kind in ["fifo", "symlink", "oversized"] {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let path = dirs.project_settings_file(&env.work());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = env.root().join("synthetic-target.json");
        let contents = br#"{"packages":["./synthetic-package"]}"#;
        match kind {
            "fifo" => {
                assert!(Command::new("mkfifo").arg(&path).status().unwrap().success());
            }
            "symlink" => {
                fs::write(&target, contents).unwrap();
                symlink(&target, &path).unwrap();
            }
            "oversized" => {
                fs::write(&path, contents).unwrap();
                // Sparse synthetic input, never a real user file or an unbounded device.
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(16 * 1024 * 1024)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = fs::symlink_metadata(&path).unwrap();
        let trace = env.root().join("settings-opens.trace");
        // A hard subprocess deadline makes a regressed FIFO open fail rather than hang CI.
        // strace is mandatory: output alone cannot prove that a symlink was never followed.
        let output = Command::new("timeout")
            .args(["--kill-after=1s", "5s"])
            .arg(strace::executable())
            .args(["-f", "-e", "trace=open,openat,openat2", "-o"])
            .arg(&trace)
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "package_list_untrusted_settings_worker", "--nocapture"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("OXA_PACKAGE_LIST_FIXTURE", env.root())
            .current_dir(env.work())
            .output()
            .expect("timeout and strace are required for the untrusted settings read proof");
        assert!(
            output.status.success(),
            "{kind}: mandatory traced listing failed or timed out: {output:?}"
        );
        let opens = fs::read_to_string(trace).unwrap();
        assert!(!opens.contains(path.to_str().unwrap()), "{kind}: {opens}");
        assert!(!opens.contains(target.to_str().unwrap()), "{kind}: {opens}");
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(before.file_type(), after.file_type());
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        if kind == "symlink" {
            assert_eq!(fs::read_link(&path).unwrap(), target);
            assert_eq!(fs::read(&target).unwrap(), contents);
        }
        assert!(!dirs.trust_file().exists());
    }
}
