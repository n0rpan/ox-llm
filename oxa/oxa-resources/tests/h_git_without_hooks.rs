//! Wave H lane `harden` (pre-flight B4 c): oxa's own git calls (package installs and updates)
//! never run a repository's hooks: every `git` invocation carries `-c core.hooksPath=/dev/null`.
//! Isolation: `oxa_fake::TestEnv`; git itself is not run (a recording runner).

use oxa_config::Dirs;
use oxa_fake::TestEnv;
use oxa_proto::OxaError;
use oxa_resources::{CommandRunner, PackageManager, Scope};
use std::path::Path;
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<(String, Vec<String>)>>>;

struct Recorder(Calls);

impl CommandRunner for Recorder {
    fn run(&self, program: &str, args: &[String], _cwd: &Path) -> Result<(i32, String), OxaError> {
        self.0.lock().unwrap().push((program.to_string(), args.to_vec()));
        Ok((1, "not run in this test".to_string()))
    }
}

#[test]
fn every_git_call_disables_hooks() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mgr = PackageManager::new(&dirs, &env.work(), Box::new(Recorder(ran.clone())));
    let _ = mgr.install(&format!("file://{}", env.work().display()), Scope::User);
    let ran = ran.lock().unwrap();
    let git: Vec<&Vec<String>> = ran.iter().filter(|(p, _)| p == "git").map(|(_, a)| a).collect();
    assert!(!git.is_empty(), "git was asked to run: {ran:?}");
    for args in git {
        assert_eq!(&args[..2], ["-c", "core.hooksPath=/dev/null"], "{args:?}");
    }
}
