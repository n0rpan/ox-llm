use oxa_fake::TestEnv;

#[test]
fn explicit_temp_base_controls_isolation_and_cleanup() {
    let parent = TestEnv::new();
    let root;
    {
        let child = TestEnv::new_in(&parent.tmp());
        root = child.root().to_path_buf();
        assert_eq!(root.parent(), Some(parent.tmp().as_path()));
        for directory in [child.home(), child.agent_dir(), child.tmp(), child.work()] {
            assert!(directory.is_dir(), "{}", directory.display());
            assert!(directory.starts_with(&root));
        }
        let envs = child.envs("http://127.0.0.1:9");
        for key in ["HOME", "OXA_AGENT_DIR", "TMPDIR", "XDG_CONFIG_HOME"] {
            let (_, value) = envs.iter().find(|(name, _)| name == key).unwrap();
            assert!(std::path::Path::new(value).starts_with(&root));
        }
        assert!(child.write("work/note.txt", "synthetic").is_file());
    }
    assert!(!root.exists());
    assert!(parent.root().is_dir());
}
