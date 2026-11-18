//! Architect-owned (W0-FROZEN, implemented): test isolation (P56).
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A private directory tree for one test, removed on drop:
/// `<root>/home/.oxa/agent`, `<root>/tmp`, `<root>/work` (the cwd for the test).
/// `oxa_config::Dirs::under(env.root())` matches this layout. Tests never touch the real
/// `HOME`, `XDG_*` or `OXA_AGENT_DIR`; child processes get [`TestEnv::envs`].
#[derive(Debug)]
pub struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        Self::new_in(std::env::temp_dir())
    }
    /// Create the same isolated layout below an explicit temporary parent, e.g. for
    /// credentials/ledgers that must live outside repositories even when the runner's
    /// TMPDIR is in a checkout.
    pub fn new_in(parent: impl AsRef<Path>) -> TestEnv {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        // Underscores between the numbers: `<pid>-<n>-<nanos>` can read as a date
        // (`6348-10-86`) once a process has made ten directories, and the cwd is part of the
        // system prompt, where the W3 check looks for dates.
        let root = parent
            .as_ref()
            .join(format!("oxa-test-{}_{}_{}", std::process::id(), n, nanos));
        for d in ["home/.oxa/agent", "tmp", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("create test dir");
        }
        TestEnv { root }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    pub fn agent_dir(&self) -> PathBuf {
        self.root.join("home/.oxa/agent")
    }
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
    /// The working directory for the test's session.
    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }
    /// Write a file below the root (parents created). Returns its absolute path.
    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&p, content).expect("write test file");
        p
    }
    /// The complete environment for a child `oxa` process: nothing is inherited except `PATH`.
    pub fn envs(&self, engine_url: &str) -> Vec<(String, String)> {
        vec![
            ("PATH".to_string(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".to_string(), self.home().to_string_lossy().into_owned()),
            (
                "OXA_AGENT_DIR".to_string(),
                self.agent_dir().to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                self.home().join(".config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_CACHE_HOME".to_string(),
                self.home().join(".cache").to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".to_string(),
                self.home().join(".local/share").to_string_lossy().into_owned(),
            ),
            ("TMPDIR".to_string(), self.tmp().to_string_lossy().into_owned()),
            ("OXA_ENGINE_URL".to_string(), engine_url.to_string()),
            // O-152: no system skills directory (the real Omarchy one is outside the test).
            ("OXA_SYSTEM_SKILLS_DIR".to_string(), String::new()),
            // O-170: the web_search provider refuses the real Brave host and a key file under
            // the real home directory. `BRAVE_SEARCH_API_KEY` is never passed (only `PATH` is
            // inherited); a test that needs a key sets a fake one itself.
            // CT-10 / CT-13: also the test isolation marker (test-only hooks; no host fallbacks).
            ("OXA_TEST_ISOLATION".to_string(), "1".to_string()),
            ("TERM".to_string(), "dumb".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
        ]
    }
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
