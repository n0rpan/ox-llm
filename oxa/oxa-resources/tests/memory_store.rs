//! M1 acceptance tests: the memory store (`oxa_resources::memory`).
//! Isolation: every path lives under `oxa_fake::TestEnv`; the real home is never touched.

use oxa_config::Dirs;
use oxa_fake::TestEnv;
use oxa_resources::memory::{self, MemoryKind, MAX_MEMORY_BODY};
use std::path::PathBuf;

fn memory_dir(env: &TestEnv) -> PathBuf {
    Dirs::under(env.root()).memory_dir()
}

#[test]
fn save_read_delete_round_trip_rebuilds_index() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    memory::save(
        &dir,
        "prefer-small-prs",
        MemoryKind::Rule,
        "Keep PRs small",
        "Under 300 changed lines.\n",
    )
    .unwrap();
    memory::save(
        &dir,
        "wal-mode",
        MemoryKind::Fact,
        "SQLite runs in WAL mode",
        "The session db uses WAL.\n",
    )
    .unwrap();

    let got = memory::read(&dir, "wal-mode")
        .unwrap()
        .expect("saved entry reads back");
    assert_eq!(got.kind, MemoryKind::Fact);
    assert_eq!(got.description, "SQLite runs in WAL mode");
    assert_eq!(got.text, "The session db uses WAL.\n");
    assert!(memory::read(&dir, "missing-entry").unwrap().is_none());

    let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(index.contains("[wal-mode](wal-mode.md)"), "{index}");
    assert!(
        index.contains("[prefer-small-prs](prefer-small-prs.md)"),
        "{index}"
    );
    assert_eq!(index.lines().count(), 2, "{index}");

    assert!(memory::delete(&dir, "wal-mode").unwrap());
    assert!(!memory::delete(&dir, "wal-mode").unwrap());
    assert!(memory::read(&dir, "wal-mode").unwrap().is_none());
    let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert_eq!(index.lines().count(), 1, "{index}");
    assert!(!index.contains("wal-mode"), "{index}");
}

#[test]
fn index_is_capped_and_sorted() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    for name in ["charlie", "alpha", "bravo"] {
        memory::save(&dir, name, MemoryKind::Lesson, "A lesson", "Body.\n").unwrap();
    }

    let full = memory::index(&dir, usize::MAX);
    let a = full.find("(alpha.md)").expect("alpha listed");
    let b = full.find("(bravo.md)").expect("bravo listed");
    let c = full.find("(charlie.md)").expect("charlie listed");
    assert!(a < b && b < c, "index not sorted by name: {full}");
    assert_eq!(full.lines().count(), 3, "{full}");

    // Room for the first line and the "(+2 more)" line, but not for a second entry.
    let first_line = full.lines().next().unwrap().len() + 1;
    let cap = first_line + 20;
    let capped = memory::index(&dir, cap);
    assert!(capped.starts_with("- [alpha]"), "{capped}");
    assert!(capped.contains("(+2 more)"), "{capped}");
    assert!(capped.len() <= cap, "{capped}");
    assert_eq!(capped.lines().count(), 2, "{capped}");
}

#[test]
fn bad_name_or_oversize_is_refused() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    for bad in [
        "Bad_Name",
        "-leading",
        "trailing-",
        "double--dash",
        "has/slash",
        "",
        "MEMORY",
    ] {
        assert!(
            memory::save(&dir, bad, MemoryKind::Fact, "x", "body\n").is_err(),
            "accepted bad name {bad:?}"
        );
    }
    assert!(memory::read(&dir, "../escape").is_err());

    let too_big = "x".repeat(MAX_MEMORY_BODY + 1);
    assert!(memory::save(&dir, "too-big", MemoryKind::Fact, "x", &too_big).is_err());
    assert!(!dir.join("too-big.md").exists(), "oversize body was written");

    let at_limit = "x".repeat(MAX_MEMORY_BODY);
    memory::save(&dir, "at-limit", MemoryKind::Fact, "x", &at_limit).unwrap();
    assert_eq!(
        memory::read(&dir, "at-limit").unwrap().unwrap().text.len(),
        MAX_MEMORY_BODY
    );
}
