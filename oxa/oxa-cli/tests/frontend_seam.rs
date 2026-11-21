//! Z05: `oxa_cli::run_with` hands the session it built to a front end that is not a terminal.
//! A turn runs through that front end, the project trust question reaches it, and two sessions
//! run at the same time in one process.
use oxa_cli::Frontend;
use oxa_core::Session;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_interactive::{worker_loop_with, InteractiveOpts, SessionCmd, TrustChoice, WorkerMsg, WorkerStop};
use oxa_proto::{AgentEvent, MessageDelta, OxaError};
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Where two front ends meet: how many hold an open session, how many finished their turn.
#[derive(Default)]
struct Meeting {
    open: AtomicUsize,
    settled: AtomicUsize,
}

fn both(count: &AtomicUsize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while count.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "the other session never {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A front end with no terminal: it runs the session's worker as the TUI does, sends the
/// prompt of the command line as one turn and keeps what the assistant said.
#[derive(Default)]
struct Probe {
    answer: Option<TrustChoice>,
    asked: Vec<(String, Vec<String>)>,
    replies: Vec<String>,
    meeting: Option<Arc<Meeting>>,
}

impl Frontend for Probe {
    fn ask_trust(&mut self, cwd: &str, gated: &[String]) -> Result<Option<TrustChoice>, OxaError> {
        self.asked.push((cwd.to_string(), gated.to_vec()));
        Ok(self.answer)
    }

    fn interactive(
        &mut self,
        session: Session,
        opts: InteractiveOpts,
        pinned: Option<&str>,
    ) -> Result<i32, OxaError> {
        assert_eq!(pinned, None);
        let InteractiveOpts {
            reopen,
            initial_prompt,
            ..
        } = opts;
        if let Some(meeting) = &self.meeting {
            meeting.open.fetch_add(1, Ordering::SeqCst);
            both(&meeting.open, "opened");
        }
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let (ui_tx, _ui_rx) = mpsc::channel();
        let cmd_tx = Arc::new(cmd_tx);
        let wake = Arc::downgrade(&cmd_tx);
        let worker = std::thread::spawn(move || {
            worker_loop_with(session, reopen, cmd_rx, msg_tx, ui_tx, WorkerStop::new(), wake)
        });
        cmd_tx
            .send(SessionCmd::Prompt {
                text: initial_prompt.expect("the prompt of the command line"),
                parts: Vec::new(),
            })
            .expect("the worker takes the prompt");
        let done = loop {
            match msg_rx.recv_timeout(Duration::from_secs(60)) {
                Ok(WorkerMsg::Event(AgentEvent::MessageUpdate {
                    delta: MessageDelta::TextEnd { content, .. },
                    ..
                })) => self.replies.push(content),
                Ok(WorkerMsg::Done(result)) => break result,
                Ok(_) => {}
                Err(e) => panic!("the worker answered nothing: {e}"),
            }
        };
        if let Some(meeting) = &self.meeting {
            // Both turns end while both sessions are still open.
            meeting.settled.fetch_add(1, Ordering::SeqCst);
            both(&meeting.settled, "finished its turn");
        }
        cmd_tx.send(SessionCmd::Shutdown).expect("the worker is alive");
        assert!(worker.join().expect("the worker thread").is_some());
        done.map(|()| 0)
    }
}

fn test_env() -> TestEnv {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}},"cacheWarming":"off","retry":{"enabled":false}}"#,
    );
    env
}

/// `oxa <prompt>` as a terminal start would run it, with `probe` in place of the terminal.
fn start(env: &TestEnv, server: &FakeServer, prompt: &str, probe: &mut Probe) -> (i32, String) {
    let owned = env.envs(&server.url());
    let pairs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let argv: Vec<String> = [
        "--model",
        "fake-model",
        "--no-mcp",
        "--no-extensions",
        "--no-skills",
        "--no-context-files",
        prompt,
    ]
    .iter()
    .map(|word| word.to_string())
    .collect();
    let err = Capture::default();
    let code = oxa_cli::run_with(
        &argv,
        &oxa_config::Env::from_pairs(&pairs),
        oxa_cli::Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(std::io::sink()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: true,
            stdout_is_tty: true,
            cwd: env.work(),
        },
        probe,
    );
    let text = String::from_utf8_lossy(&err.0.lock().unwrap()).into_owned();
    (code, text)
}

fn sent(server: &FakeServer, prompt: &str) -> bool {
    server
        .requests()
        .iter()
        .any(|request| format!("{:?}", request.messages).contains(prompt))
}

#[test]
fn a_turn_and_the_trust_question_go_through_a_front_end_without_a_terminal() {
    let env = test_env();
    env.write("work/.oxa/settings.json", "{}");
    let server =
        FakeServer::start(vec![ScriptTurn::text("the scripted reply")], FakeOpts::default()).unwrap();
    let mut probe = Probe {
        answer: Some(TrustChoice::Trust),
        ..Probe::default()
    };
    let (code, err) = start(&env, &server, "the first question", &mut probe);
    assert_eq!(code, 0, "{err}");
    assert_eq!(probe.replies, ["the scripted reply"], "{err}");
    assert!(sent(&server, "the first question"));
    // The trust question came to the front end once, with the project and what it gates.
    assert_eq!(probe.asked.len(), 1, "{:?}", probe.asked);
    let (cwd, gated) = &probe.asked[0];
    assert_eq!(cwd, &env.work().display().to_string());
    assert!(!gated.is_empty());
    // The answer was saved by the shared path: the next start does not ask again.
    let again = FakeServer::start(vec![ScriptTurn::text("the second reply")], FakeOpts::default()).unwrap();
    let mut probe = Probe::default();
    let (code, err) = start(&env, &again, "the second question", &mut probe);
    assert_eq!(code, 0, "{err}");
    assert!(probe.asked.is_empty(), "{:?}", probe.asked);
    assert_eq!(probe.replies, ["the second reply"], "{err}");
}

#[test]
fn two_sessions_run_at_the_same_time_in_one_process() {
    let meeting = Arc::new(Meeting::default());
    let sessions: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| {
            let meeting = meeting.clone();
            std::thread::spawn(move || {
                let env = test_env();
                let reply = format!("reply {name}");
                let prompt = format!("question {name}");
                let server = FakeServer::start(vec![ScriptTurn::text(&reply)], FakeOpts::default()).unwrap();
                let mut probe = Probe {
                    meeting: Some(meeting),
                    ..Probe::default()
                };
                let (code, err) = start(&env, &server, &prompt, &mut probe);
                assert_eq!(code, 0, "{err}");
                // Each session got its own reply from its own engine and sent only its own prompt.
                assert_eq!(probe.replies, [reply], "{err}");
                assert!(sent(&server, &prompt));
                let other = if name == "one" {
                    "question two"
                } else {
                    "question one"
                };
                assert!(!sent(&server, other));
            })
        })
        .collect();
    for session in sessions {
        session.join().expect("a session thread failed");
    }
}
