//! `Settings::set` writes through a fresh temporary file: whatever already sits at a temporary
//! sibling path is left alone, and the user file is private to the user.

use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn set_does_not_write_through_existing_sibling_link() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();

    let elsewhere = env.root().join("elsewhere.txt");
    std::fs::write(&elsewhere, "unchanged\n").expect("write elsewhere");

    for project in [true, false] {
        let file = if project {
            dirs.project_settings_file(&cwd)
        } else {
            dirs.settings_file()
        };
        let parent = file.parent().expect("parent");
        std::fs::create_dir_all(parent).expect("mkdir");
        let old_tmp = file.with_extension("json.tmp");
        std::os::unix::fs::symlink(&elsewhere, &old_tmp).expect("symlink");

        let mut s = Settings::load(&dirs, &cwd, true);
        s.set(&dirs, &cwd, "theme", json!("light"), project).expect("set");

        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("read elsewhere"),
            "unchanged\n"
        );
        assert!(std::fs::symlink_metadata(&old_tmp)
            .expect("link still there")
            .file_type()
            .is_symlink());
        let meta = std::fs::symlink_metadata(&file).expect("settings meta");
        assert!(meta.file_type().is_file());
        assert_eq!(read_json(&file), json!({"theme": "light"}));
        if !project {
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        // No temporary files are left behind besides the pre-existing link.
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp") && n != "settings.json.tmp")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
