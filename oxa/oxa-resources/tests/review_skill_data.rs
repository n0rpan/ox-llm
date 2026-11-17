//! Review remains opt-in data, not a registered built-in or a session-prefix change.

use oxa_resources::{load_skill_file, ResourceSource, BUILTIN_SKILLS};
use std::path::Path;

#[test]
fn review_skill_is_valid_unregistered_data() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("builtin-skills/review/SKILL.md");
    let (skill, diagnostics) = load_skill_file(&path, ResourceSource::Builtin).expect("review skill");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(skill.name, "review");
    assert!(!skill.description.is_empty());
    assert!(BUILTIN_SKILLS.iter().all(|(name, _)| *name != "review"));
}
