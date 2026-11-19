//! M3: `oxa memory list` and `oxa memory rm` over the memory store in the agent directory.
//! Offline and synthetic only: entries are written with `oxa_resources::memory` in a temporary
//! home, and the commands run in-process.

use oxa_cli::run_memory;
use oxa_config::Dirs;
use oxa_fake::TestEnv;
use oxa_resources::memory::{self, MemoryKind};
use std::process::Command;

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Run `oxa memory <words>` in-process; returns (exit code, stdout, stderr).
fn memory_cmd(env: &TestEnv, list: &[&str]) -> (i32, String, String) {
    let dirs = Dirs::under(env.root());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_memory(&words(list), &dirs, &mut out, &mut err).unwrap();
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn memory_list_prints_index() {
    let env = TestEnv::new();
    let dir = Dirs::under(env.root()).memory_dir();
    memory::save(
        &dir,
        "no-push",
        MemoryKind::Rule,
        "Never push to main",
        "Use PRs.",
    )
    .unwrap();
    memory::save(&dir, "build-cmd", MemoryKind::Fact, "How to build", "cargo build").unwrap();

    let (code, out, err) = memory_cmd(&env, &["list"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.is_empty(), "{err}");
    assert_eq!(out, memory::index(&dir, usize::MAX), "{out}");
    assert!(
        out.contains("- [build-cmd](build-cmd.md) (fact): How to build\n"),
        "{out}"
    );
    assert!(
        out.contains("- [no-push](no-push.md) (rule): Never push to main\n"),
        "{out}"
    );
    // The index is sorted by name, so build-cmd comes first.
    assert!(
        out.find("build-cmd").unwrap() < out.find("no-push").unwrap(),
        "{out}"
    );
}

#[test]
fn memory_rm_removes_entry_and_index_line() {
    let env = TestEnv::new();
    let dir = Dirs::under(env.root()).memory_dir();
    memory::save(&dir, "build-cmd", MemoryKind::Fact, "How to build", "cargo build").unwrap();
    memory::save(
        &dir,
        "no-push",
        MemoryKind::Rule,
        "Never push to main",
        "Use PRs.",
    )
    .unwrap();

    let (code, out, err) = memory_cmd(&env, &["rm", "build-cmd"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("removed build-cmd"), "{out}");
    assert!(memory::read(&dir, "build-cmd").unwrap().is_none());
    let index = std::fs::read_to_string(dir.join(memory::MEMORY_INDEX_FILE)).unwrap();
    assert!(!index.contains("build-cmd"), "{index}");
    assert!(index.contains("[no-push](no-push.md)"), "{index}");

    // A second removal finds nothing: a runtime failure (exit 1) reported on stderr.
    let (code, _, err) = memory_cmd(&env, &["rm", "build-cmd"]);
    assert_eq!(code, 1);
    assert!(err.contains("build-cmd"), "{err}");
}

#[test]
fn agent_markers_refuse_memory_access_without_disclosing_or_deleting_entries() {
    for marker in ["AI_AGENT", "OXA_CODING_AGENT", "OXA_SUBAGENT_DEPTH"] {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let dir = dirs.memory_dir();
        memory::save(
            &dir,
            "private-note",
            MemoryKind::Fact,
            "Synthetic private note",
            "private fixture body",
        )
        .unwrap();

        for args in [
            vec!["memory", "list"],
            vec!["memory", "show", "private-note"],
            vec!["memory", "rm", "private-note"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
                .args(args)
                .env_clear()
                .env("HOME", env.root())
                .env("OXA_TEST_ISOLATION", "1")
                .env(marker, "1")
                .current_dir(env.root())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty(), "memory contents must stay private");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains("Memory commands require the owner outside an agent tool"),
                "{error}"
            );
            assert!(!error.contains("private-note"), "arguments must not be echoed");
            assert!(!error.contains("private fixture body"), "contents must not leak");
        }

        assert!(
            memory::read(&dir, "private-note").unwrap().is_some(),
            "an agent memory removal must have no effect"
        );
    }
}
