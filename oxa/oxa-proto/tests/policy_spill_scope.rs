//! The spill-directory callback is opt-in: custom policies keep their decisions unchanged.
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
fn spill_callback_preserves_custom_policy_decisions_by_default() {
    let mut policy: Box<dyn Policy> = Box::new(AskPolicy);
    let spill = Path::new("/synthetic/agent/tmp/session");
    policy.set_spill_dir(spill);
    assert_eq!(
        policy.decide(
            Mode::Ask,
            &Access::Read(spill.join("out.txt")),
            Path::new("/synthetic/work")
        ),
        Decision::Ask
    );
}
