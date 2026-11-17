//! O-170 guard (wave H lane `search`): no test process can reach the owner's Brave key.
//!
//! (a) `TestEnv::envs` never passes `BRAVE_SEARCH_API_KEY`, points `HOME` / the agent dir into
//!     its temporary root and sets `OXA_TEST_ISOLATION=1` (the provider then refuses the real
//!     host and a key file under the real home: `oxa-web/tests/h_key_and_hooks.rs`
//!     `guard_testenv_environment_isolates_the_provider`, guard (c)).
//! (b) Every test helper in the tree that spawns the `oxa` binary clears the inherited
//!     environment (`env_clear()`) before it passes `TestEnv::envs`: a source scan over
//!     `oxa-*/tests/**/*.rs` and `oxa-*/src/**/*.rs` of the test crates.
use oxa_fake::TestEnv;
use std::path::{Path, PathBuf};

#[test]
fn testenv_envs_is_isolated() {
    let env = TestEnv::new();
    let vars = env.envs("http://127.0.0.1:1");
    let get = |n: &str| vars.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert!(
        get("BRAVE_SEARCH_API_KEY").is_none(),
        "the key variable is never passed"
    );
    assert!(vars.iter().all(|(_, v)| !v.contains("test-key-not-real")));
    assert_eq!(get("OXA_TEST_ISOLATION").as_deref(), Some("1"));
    for name in [
        "HOME",
        "OXA_AGENT_DIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "TMPDIR",
    ] {
        let v = get(name).unwrap_or_else(|| panic!("{name} is set"));
        assert!(
            Path::new(&v).starts_with(env.root()),
            "{name}={v} is outside the test root"
        );
    }
    // Only PATH comes from the parent; the other names are a fixed list.
    let names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
    for n in &names {
        assert!(
            [
                "PATH",
                "HOME",
                "OXA_AGENT_DIR",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "TMPDIR",
                "OXA_ENGINE_URL",
                "OXA_SYSTEM_SKILLS_DIR",
                "OXA_TEST_ISOLATION",
                "TERM",
                "LANG",
            ]
            .contains(n),
            "unexpected variable {n} in TestEnv::envs"
        );
    }
}

#[test]
fn testenv_explicit_parent_is_isolated_and_cleaned_up() {
    let parent = TestEnv::new();
    let root;
    {
        let first = TestEnv::new_in(parent.tmp());
        let second = TestEnv::new_in(parent.tmp());
        root = first.root().to_path_buf();
        assert_eq!(root.parent(), Some(parent.tmp().as_path()));
        assert_ne!(first.root(), second.root());
        for dir in [first.home(), first.agent_dir(), first.tmp(), first.work()] {
            assert!(dir.is_dir() && dir.starts_with(&root));
        }
        for (name, value) in first.envs("http://127.0.0.1:1") {
            if matches!(
                name.as_str(),
                "HOME" | "OXA_AGENT_DIR" | "TMPDIR" | "XDG_CONFIG_HOME"
            ) {
                assert!(Path::new(&value).starts_with(&root));
            }
        }
        assert!(first.write("work/synthetic.txt", "synthetic content").is_file());
    }
    assert!(
        !root.exists(),
        "the explicit-parent environment must be removed on drop"
    );
    assert!(parent.tmp().is_dir(), "cleanup must preserve the caller's parent");
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// `true` when the argument text of a `Command::new(` names the `oxa` binary.
fn spawns_oxa(arg: &str) -> bool {
    arg.contains("oxa_bin()") || arg.contains("CARGO_BIN_EXE_oxa\"") || arg.trim() == "\"oxa\""
}

/// Spawn sites that legitimately do not clear the environment, with the reason. Empty today.
const ALLOWED: &[(&str, &str)] = &[];

#[test]
fn every_oxa_spawn_in_the_tests_clears_the_environment() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    let mut crates: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("oxa-"))
        })
        .collect();
    crates.sort();
    for c in &crates {
        rs_files(&c.join("tests"), &mut files);
    }
    // The test-only crates' own helpers (`run_oxa`, a pty harness).
    rs_files(&root.join("oxa-parity/src"), &mut files);
    rs_files(&root.join("oxa-fake/src"), &mut files);
    let mut sites = 0;
    let mut bad = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(at) = line.find("Command::new(") else {
                continue;
            };
            let arg = &line[at + "Command::new(".len()..];
            let arg = arg.split(')').next().unwrap_or("");
            if !spawns_oxa(&format!("{arg})")) && !spawns_oxa(arg) {
                continue;
            }
            sites += 1;
            // The builder chain of this spawn: this line and the next twelve.
            let window = lines[i..lines.len().min(i + 13)].join("\n");
            let key = format!("{rel}:{}", i + 1);
            if !window.contains(".env_clear()") && !ALLOWED.iter().any(|(k, _)| *k == key) {
                bad.push(key);
            }
        }
    }
    assert!(sites >= 5, "the scan found the known spawn sites ({sites})");
    assert!(bad.is_empty(), "oxa spawned without env_clear(): {bad:?}");
}
