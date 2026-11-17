//! Explicit fixture roots must isolate every child path and clean up on drop.
use oxa_fake::TestEnv;

#[test]
fn explicit_root_controls_child_environment_and_cleanup() {
    let parent = TestEnv::new();
    let root;
    {
        let child = TestEnv::new_in(parent.work());
        root = child.root().to_path_buf();
        assert_eq!(root.parent(), Some(parent.work().as_path()));
        assert_ne!(root, parent.root());
        let envs = child.envs("http://127.0.0.1:9");
        for (key, expected) in [
            ("HOME", child.home()),
            ("OXA_AGENT_DIR", child.agent_dir()),
            ("TMPDIR", child.tmp()),
            ("XDG_CONFIG_HOME", child.home().join(".config")),
            ("XDG_CACHE_HOME", child.home().join(".cache")),
            ("XDG_DATA_HOME", child.home().join(".local/share")),
        ] {
            assert_eq!(
                envs.iter().find(|(name, _)| name == key).unwrap().1,
                expected.to_string_lossy()
            );
        }
        assert!(child.write("work/probe.txt", "synthetic").is_file());
    }
    assert!(!root.exists());
    assert!(parent.work().is_dir());
}
