//! P4 (wave H lane `local`): offline, the package manager does not run `git` for a remote
//! repository; the error is the one offline shape. One process, one policy.

use oxa_config::Dirs;
use oxa_fake::TestEnv;
use oxa_proto::{codes, set_net_policy, OxaError};
use oxa_resources::{CommandRunner, PackageManager, Scope};
use std::path::Path;
use std::sync::{Arc, Mutex};

struct Recorder(Arc<Mutex<Vec<String>>>);

impl CommandRunner for Recorder {
    fn run(&self, program: &str, _args: &[String], _cwd: &Path) -> Result<(i32, String), OxaError> {
        self.0.lock().unwrap().push(program.to_string());
        Ok((1, "not run in this test".to_string()))
    }
}

#[test]
fn offline_refuses_remote_git_before_running_it() {
    set_net_policy(true, false);
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mgr = PackageManager::new(&dirs, &env.work(), Box::new(Recorder(ran.clone())));
    for source in ["git:github.com/example/pkg", "https://github.com/example/pkg"] {
        let e = mgr.install(source, Scope::User).unwrap_err();
        assert_eq!(e.code, codes::OFFLINE, "{source}: {e}");
        assert!(
            e.to_string().contains(
                "offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline)\nFix: turn offline mode off"
            ),
            "{e}"
        );
    }
    assert!(
        ran.lock().unwrap().is_empty(),
        "nothing ran: {:?}",
        ran.lock().unwrap()
    );
    // A repository on this machine still goes to git.
    let _ = mgr.install(&format!("file://{}", env.work().display()), Scope::User);
    assert_eq!(ran.lock().unwrap().first().map(String::as_str), Some("git"));
    set_net_policy(false, false);
}
