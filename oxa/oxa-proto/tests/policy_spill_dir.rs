//! Attaching session paths must not silently change a custom policy's decisions.
use oxa_proto::{Access, Decision, Mode, Policy};
use std::path::Path;

struct AskPolicy;

impl Policy for AskPolicy {
    fn decide(&self, _: Mode, _: &Access, _: &Path) -> Decision {
        Decision::Ask
    }

    fn remember(&mut self, _: &Access) {}
}

#[test]
fn spill_read_allowance_is_opt_in_for_custom_policies() {
    let mut policy: Box<dyn Policy> = Box::new(AskPolicy);
    let dir = Path::new("/synthetic/tmp/session");
    policy.set_spill_dir(dir);
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        assert_eq!(
            policy.decide(
                mode,
                &Access::Read(dir.join("out.txt")),
                Path::new("/synthetic/work")
            ),
            Decision::Ask
        );
    }
}
