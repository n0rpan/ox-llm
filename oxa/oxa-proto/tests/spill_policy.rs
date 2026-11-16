//! Spill hooks are optional for third-party policies; default hooks grant nothing.
use oxa_proto::{Access, Decision, Mode, Policy};
use std::path::{Path, PathBuf};

struct AskingPolicy;

impl Policy for AskingPolicy {
    fn decide(&self, _mode: Mode, _access: &Access, _cwd: &Path) -> Decision {
        Decision::Ask
    }

    fn remember(&mut self, _access: &Access) {}
}

#[test]
fn default_spill_hooks_leave_custom_policy_and_walk_filter_unchanged() {
    let mut policy = AskingPolicy;
    let current = Path::new("/synthetic/tmp/current");
    let inherited = PathBuf::from("/synthetic/tmp/source");
    policy.set_spill_dir(current);
    policy.set_inherited_spill_dirs(std::slice::from_ref(&inherited));
    for dir in [current, inherited.as_path()] {
        assert_eq!(
            policy.decide(
                Mode::Workspace,
                &Access::Read(dir.join("out.txt")),
                Path::new("/synthetic/project")
            ),
            Decision::Ask
        );
    }
    assert!(policy.path_filter().is_none());
}
