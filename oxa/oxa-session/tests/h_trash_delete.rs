//! Wave H (SES-003): `FsStore::remove` uses pi's `deleteSessionFile` rule: the `trash` command
//! first (output captured), trashed when it succeeds or the file is gone afterwards; otherwise
//! unlink. One test (it changes PATH for this process).
#![cfg(unix)]
use oxa_fake::TestEnv;
use oxa_session::{FsStore, Removed, SessionStore};
use std::os::unix::fs::PermissionsExt;

fn script(dir: &std::path::Path, body: &str) {
    let p = dir.join("trash");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn trash_first_then_unlink() {
    let env = TestEnv::new();
    let bin = env.root().join("bin");
    let bin_none = env.root().join("bin-none");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&bin_none).unwrap();
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let cwd = env.work();
    let make = || {
        let s = store.create(&cwd, None, None).unwrap();
        store
            .append(
                &s,
                &oxa_proto::Entry {
                    id: "e0000001".to_string(),
                    parent: None,
                    ts_ms: 1,
                    body: oxa_proto::EntryBody::SessionInfo { name: "x".into() },
                },
            )
            .unwrap();
        s
    };
    let old_path = std::env::var_os("PATH");
    // A `trash` that moves the file away: trashed.
    let bin_dir = env.root().join("trashed");
    std::fs::create_dir_all(&bin_dir).unwrap();
    script(&bin, &format!("echo noisy; mv \"$1\" {}/", bin_dir.display()));
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    let a = make();
    assert_eq!(store.remove(&a).unwrap(), Removed::Trashed);
    assert!(!a.path.exists());
    assert_eq!(
        std::fs::read_dir(&bin_dir).unwrap().count(),
        1,
        "moved to the trash"
    );
    // A `trash` that fails and leaves the file: unlinked.
    script(&bin, "exit 1");
    let b = make();
    assert_eq!(store.remove(&b).unwrap(), Removed::Deleted);
    assert!(!b.path.exists());
    // No `trash` at all: unlinked; `delete` is `remove` without the report.
    std::env::set_var("PATH", &bin_none);
    let c = make();
    store.delete(&c).unwrap();
    assert!(!c.path.exists());
    match old_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
}
