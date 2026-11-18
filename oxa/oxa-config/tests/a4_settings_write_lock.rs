//! A4: `Settings::source`, `user_value`, `unset`, and the write lock that `set` and `unset` hold
//! for their read-modify-write.

use oxa_config::{Dirs, Settings, Source};
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn unset_removes_key_and_prunes_empty_parents() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let mut s = Settings::load(&dirs, &cwd, false);
    s.set(&dirs, &cwd, "retry.provider.maxRetries", json!(2), false)
        .expect("set");
    s.set(&dirs, &cwd, "compaction.reserveTokens", json!(5000), false)
        .expect("set");

    s.unset(&dirs, &cwd, "retry.provider.maxRetries", false)
        .expect("unset");
    assert_eq!(
        read_json(&dirs.settings_file()),
        json!({"compaction": {"reserveTokens": 5000}})
    );
    assert_eq!(s.user_value("retry.provider.maxRetries"), None);

    s.unset(&dirs, &cwd, "compaction.reserveTokens", false)
        .expect("unset");
    assert_eq!(read_json(&dirs.settings_file()), json!({}));
    assert_eq!(s.source("compaction.reserveTokens"), Source::Default);
}

#[test]
fn unset_keeps_other_keys_and_order() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let file = dirs.settings_file();
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &file,
        "{\"theme\": \"dark\", \"retry\": {\"enabled\": false, \"maxRetries\": 4}, \
         \"compaction\": {\"reserveTokens\": 9}}\n",
    )
    .expect("write");

    let mut s = Settings::load(&dirs, &cwd, false);
    s.unset(&dirs, &cwd, "retry.enabled", false).expect("unset");

    let got = read_json(&file);
    assert_eq!(
        got,
        json!({"theme": "dark", "retry": {"maxRetries": 4}, "compaction": {"reserveTokens": 9}})
    );
    let keys: Vec<&str> = got
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["theme", "retry", "compaction"]);
}

#[test]
fn source_reports_default_user_project() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let key = "compaction.reserveTokens";
    let mut s = Settings::load(&dirs, &cwd, true);
    assert_eq!(s.source(key), Source::Default);

    s.set(&dirs, &cwd, key, json!(1000), false).expect("set user");
    assert_eq!(s.source(key), Source::User);
    assert_eq!(s.user_value(key), Some(&json!(1000)));

    s.set(&dirs, &cwd, key, json!(2000), true).expect("set project");
    assert_eq!(s.source(key), Source::Project);
    assert_eq!(s.user_value(key), Some(&json!(1000)));
}

#[test]
fn concurrent_sets_of_two_keys_keep_both() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut s = Settings::load(&dirs, &cwd, false);
            for i in 0..40u64 {
                s.set(&dirs, &cwd, "compaction.reserveTokens", json!(i), false)
                    .expect("set a");
            }
        });
        scope.spawn(|| {
            let mut s = Settings::load(&dirs, &cwd, false);
            for _ in 0..40 {
                s.set(&dirs, &cwd, "retry.enabled", json!(false), false)
                    .expect("set b");
            }
        });
    });
    assert_eq!(
        read_json(&dirs.settings_file()),
        json!({"compaction": {"reserveTokens": 39}, "retry": {"enabled": false}})
    );
}

#[test]
fn unset_never_follows_a_symlink() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let elsewhere = env.root().join("elsewhere.json");
    std::fs::write(&elsewhere, "{\"theme\": \"dark\"}\n").expect("write");
    let file = dirs.settings_file();
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::os::unix::fs::symlink(&elsewhere, &file).expect("symlink");

    let mut s = Settings::load(&dirs, &cwd, false);
    assert!(s.unset(&dirs, &cwd, "theme", false).is_err());
    assert_eq!(read_json(&elsewhere), json!({"theme": "dark"}));
    assert!(std::fs::symlink_metadata(&file)
        .expect("meta")
        .file_type()
        .is_symlink());
}

#[test]
fn user_file_mode_stays_0600() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let mut s = Settings::load(&dirs, &cwd, false);
    s.set(&dirs, &cwd, "retry.enabled", json!(false), false)
        .expect("set");
    s.unset(&dirs, &cwd, "retry.enabled", false).expect("unset");
    let mode = std::fs::metadata(dirs.settings_file())
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
