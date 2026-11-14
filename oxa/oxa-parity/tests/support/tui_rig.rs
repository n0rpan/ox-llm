//! Shared pty rig of the interactive rows (lane tui, wave H): the real `oxa` in a pty against
//! the fake engine, plus the screen helpers the tree, session and TUI rows use. Included with
//! `#[path = "support/tui_rig.rs"] mod rig;` (each test file is its own crate).
#![allow(dead_code, unused_imports)]

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
pub use oxa_parity::pty::{PtyOpts, PtySession, Screen, TermProfile, DEFAULT_TIMEOUT};
use oxa_proto::Role;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const WAIT: Duration = Duration::from_secs(30);

/// A pty session with its engine and directories. Field order = drop order: the pty (and the
/// child) go first, then the engine, then the dirs.
pub struct Rig {
    pub pty: PtySession,
    pub server: FakeServer,
    pub env: TestEnv,
}

/// Options of [`Rig::start_with`].
pub struct RigOpts {
    pub size: (u16, u16),
    pub profile: TermProfile,
    pub args: Vec<String>,
    /// Extra environment for the child.
    pub env: Vec<(String, String)>,
    pub fake: FakeOpts,
}

impl Default for RigOpts {
    fn default() -> RigOpts {
        RigOpts {
            size: (30, 100),
            profile: TermProfile::xterm(),
            args: Vec::new(),
            env: Vec::new(),
            fake: FakeOpts::default(),
        }
    }
}

/// `n` scripted text replies `reply 1`, `reply 2`, ...
pub fn replies(n: usize) -> Vec<ScriptTurn> {
    (1..=n).map(|i| ScriptTurn::text(&format!("reply {i}"))).collect()
}

impl Rig {
    /// A TUI session on an existing env (settings, sessions written before) with `script`.
    pub fn start_in(env: TestEnv, script: Vec<ScriptTurn>, opts: RigOpts) -> Rig {
        let server = FakeServer::start(script, opts.fake).expect("start fake engine");
        let args: Vec<&str> = opts.args.iter().map(String::as_str).collect();
        let pty = PtySession::spawn_with(
            &env,
            &server.url(),
            &args,
            PtyOpts {
                size: opts.size,
                profile: opts.profile,
                env: opts.env,
                ..PtyOpts::default()
            },
        );
        pty.wait_ready().unwrap();
        Rig { pty, server, env }
    }

    pub fn start(script: Vec<ScriptTurn>, opts: RigOpts) -> Rig {
        let rig = Rig::start_in(TestEnv::new(), script, opts);
        // The first full frame: the header's key hints (at any width: the hint line may be
        // cut) and the editor between its two rules.
        rig.pty
            .wait_for_named(
                "the first frame",
                |s| {
                    s.contains("oxa v")
                        && s.rows()
                            .iter()
                            .filter(|r| !r.is_empty() && r.chars().all(|c| c == '─'))
                            .count()
                            >= 2
                },
                WAIT,
            )
            .unwrap();
        rig
    }

    pub fn screen(&self) -> Screen {
        self.pty.screen()
    }

    /// Type `text` and wait until the editor shows it.
    pub fn type_text(&self, text: &str) {
        self.pty.send_text(text);
        let tail: String = text
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.pty
            .wait_for_named(
                &format!("{tail:?} in the editor"),
                |s| editor_text(s).contains(&tail),
                WAIT,
            )
            .unwrap();
    }

    /// Type a prompt, submit it and wait until `reply` is on screen and the run has settled.
    pub fn prompt(&self, text: &str, reply: &str) {
        let before = self.generating().len();
        self.type_text(text);
        self.pty.send_key("enter");
        self.wait_requests(before + 1);
        self.pty.wait_for_text(reply).unwrap();
        self.settle();
    }

    /// Run a slash command: type it, let the menu settle, Enter.
    pub fn slash(&self, cmd: &str) {
        self.type_text(cmd);
        self.pty.wait_stable(150, WAIT).unwrap();
        self.pty.send_key("enter");
    }

    /// Wait until the screen has not changed for a moment (and the run is over: no spinner).
    pub fn settle(&self) -> Screen {
        self.pty
            .wait_for_named(
                "the run settled",
                |s| !s.contains("Working") && !s.contains("esc to interrupt"),
                WAIT,
            )
            .unwrap();
        self.pty.wait_stable(200, WAIT).unwrap()
    }

    /// The generating requests (idle prefills, `max_tokens: 0`, are left out), arrival order.
    pub fn generating(&self) -> Vec<Recorded> {
        let mut reqs = self.server.requests();
        reqs.sort_by_key(|r| r.seq);
        reqs.retain(|r| r.max_tokens > 0);
        reqs
    }

    /// Wait until the engine has seen `n` generating requests.
    pub fn wait_requests(&self, n: usize) -> Vec<Recorded> {
        let t = Instant::now();
        loop {
            let reqs = self.generating();
            if reqs.len() >= n {
                return reqs;
            }
            assert!(
                t.elapsed() < WAIT,
                "waited for {n} engine requests, got {}\n{:?}",
                reqs.len(),
                self.pty.screen()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The session files below the agent dir's `sessions/`.
    pub fn session_files(&self) -> Vec<PathBuf> {
        session_files(&self.env)
    }

    /// Ctrl+D on an empty editor and wait for the exit.
    pub fn quit(&mut self) {
        self.pty.wait_stable(150, WAIT).unwrap();
        self.pty.send_key("ctrl+d");
        let st = self.pty.wait_exit(WAIT).unwrap();
        assert!(st.success(), "exit status {st}");
    }
}

/// Every `*.jsonl` below `<agent dir>/sessions`, sorted.
pub fn session_files(env: &TestEnv) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&env.agent_dir().join("sessions"), &mut out);
    out.sort();
    out
}

/// The JSON lines of a session file.
pub fn session_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The rows of the editor: between the last two horizontal rules at the bottom.
pub fn editor_rows(s: &Screen) -> Vec<String> {
    let rows = s.rows();
    let rules: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.is_empty() && r.chars().all(|c| c == '─'))
        .map(|(i, _)| i)
        .collect();
    if rules.len() < 2 {
        return Vec::new();
    }
    let (a, b) = (rules[rules.len() - 2], rules[rules.len() - 1]);
    rows[a + 1..b].to_vec()
}

/// The editor text (rows joined with `\n`, right-trimmed).
pub fn editor_text(s: &Screen) -> String {
    editor_rows(s)
        .iter()
        .map(|r| r.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// The text contents of a request's messages, in order.
pub fn contents(r: &Recorded) -> Vec<String> {
    r.messages
        .iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

/// The user texts of a request's messages.
pub fn user_texts(r: &Recorded) -> Vec<String> {
    r.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}
