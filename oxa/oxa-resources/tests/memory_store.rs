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

#[test]
fn write_atomic_removes_temp_sibling_when_rename_fails() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    // A directory at the target makes the final rename fail after the temp write succeeded.
    let target = dir.join("blocked.md");
    std::fs::create_dir_all(&target).unwrap();
    assert!(oxa_resources::write_atomic(&target, b"data").is_err());
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
}

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn memory_dir_lock_entries_and_index_are_owner_only() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    // A directory created earlier with a loose mode is tightened by the next save.
    std::fs::create_dir_all(&dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    memory::save(&dir, "private-note", MemoryKind::Fact, "Secret", "Body.\n").unwrap();
    assert_eq!(mode_of(&dir), 0o700, "memory dir");
    assert_eq!(mode_of(&dir.join(".lock")), 0o600, "lock");
    assert_eq!(mode_of(&dir.join("private-note.md")), 0o600, "entry");
    assert_eq!(mode_of(&dir.join("MEMORY.md")), 0o600, "index");
    // Replacing an entry keeps it private.
    memory::save(&dir, "private-note", MemoryKind::Fact, "Secret", "New body.\n").unwrap();
    assert_eq!(mode_of(&dir.join("private-note.md")), 0o600, "replaced entry");
}

#[test]
fn index_scan_survives_an_oversized_entry_next_to_a_valid_one() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    memory::save(&dir, "good-note", MemoryKind::Rule, "Fine", "Body.\n").unwrap();
    // A file dropped in by hand, far above the save cap: it is read only up to a bounded prefix.
    let huge = format!(
        "---\nkind: fact\ndescription: \"Huge\"\n---\n{}",
        "x".repeat(3 * 1024 * 1024)
    );
    std::fs::write(dir.join("huge-note.md"), huge).unwrap();
    // Junk that never closes its frontmatter is skipped, not an error.
    std::fs::write(dir.join("junk-note.md"), "y".repeat(1024 * 1024)).unwrap();
    let index = memory::index(&dir, usize::MAX);
    assert!(
        index.contains("[good-note](good-note.md) (rule): Fine"),
        "{index}"
    );
    assert!(
        index.contains("[huge-note](huge-note.md) (fact): Huge"),
        "{index}"
    );
    assert!(!index.contains("junk-note"), "{index}");
    // A save still rebuilds a complete index around it.
    memory::save(&dir, "other-note", MemoryKind::Lesson, "Next", "Body.\n").unwrap();
    let file = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert_eq!(file.lines().count(), 3, "{file}");
}

#[test]
fn failed_index_write_does_not_hide_a_committed_save_or_delete() {
    let env = TestEnv::new();
    let dir = memory_dir(&env);
    memory::save(&dir, "first-note", MemoryKind::Fact, "One", "Body.\n").unwrap();
    // A directory at MEMORY.md makes every index rewrite fail.
    std::fs::remove_file(dir.join("MEMORY.md")).unwrap();
    std::fs::create_dir(dir.join("MEMORY.md")).unwrap();

    memory::save(&dir, "first-note", MemoryKind::Fact, "One", "Replaced.\n").unwrap();
    assert_eq!(
        memory::read(&dir, "first-note").unwrap().unwrap().text,
        "Replaced.\n"
    );
    memory::save(&dir, "second-note", MemoryKind::Fact, "Two", "Body.\n").unwrap();
    let live = memory::index(&dir, usize::MAX);
    assert!(
        live.contains("(first-note.md)") && live.contains("(second-note.md)"),
        "{live}"
    );

    assert!(memory::delete(&dir, "second-note").unwrap());
    assert!(memory::read(&dir, "second-note").unwrap().is_none());
    assert!(!memory::index(&dir, usize::MAX).contains("second-note"));

    // Once the index can be written again, a retried delete (even of the gone entry) repairs it.
    std::fs::remove_dir(dir.join("MEMORY.md")).unwrap();
    assert!(!memory::delete(&dir, "second-note").unwrap());
    let file = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert_eq!(file.lines().count(), 1, "{file}");
    assert!(
        file.contains("first-note") && !file.contains("second-note"),
        "{file}"
    );
}
