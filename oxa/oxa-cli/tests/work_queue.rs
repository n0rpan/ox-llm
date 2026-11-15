//! Queue behavior against real worktrees and a fake engine, with no external network.
#[path = "common/work_repo.rs"]
mod work;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{
    fs,
    process::{Command, Output},
    time::{Duration, Instant, SystemTime},
};
use work::{git, setup};
fn task_file(env: &TestEnv, file: &str, id: &str, age: u64) {
    let path = env.write(&format!("work/queue/{file}.json"), &serde_json::json!([{"id":id,"model":"fake","branch":format!("agent/{id}"),"prompt":format!("{id} task")}]).to_string());
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(age))
        .unwrap();
}
fn command(env: &TestEnv, url: &str, max: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_oxa"));
    c.args([
        "work",
        "--queue",
        "../queue",
        "--max",
        max,
        "--min-free-mib",
        "0",
        "--once",
    ])
    .current_dir(env.work().join("project"))
    .env_clear()
    .envs(env.envs(url));
    c
}
fn finish(mut c: Command) -> Output {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(c.output().unwrap());
    });
    rx.recv_timeout(Duration::from_secs(30)).expect("queue finished")
}
fn rows(out: &Output) -> Vec<serde_json::Value> {
    assert!(out.status.code().is_some(), "queue terminated");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{e}: {line} / {}", String::from_utf8_lossy(&out.stderr)))
        })
        .collect()
}
fn until(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn oldest_file_runs_first_and_restart_skips_completed_agents() {
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "a-new", "new", 10);
    task_file(&env, "z-old", "old", 20);
    let server = FakeServer::start(
        vec![ScriptTurn::text("done"), ScriptTurn::text("done")],
        FakeOpts::default(),
    )
    .unwrap();
    let out = finish(command(&env, &server.url(), "1"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let messages = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 2);
    assert!(messages[0]
        .messages
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|s| s.contains("old task"))));
    let events = rows(&out);
    let ids: Vec<_> = events
        .iter()
        .filter(|r| r["event"] == "task")
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["old", "new"]);
    let cached = finish(command(&env, &server.url(), "1"));
    assert!(cached.status.success());
    assert_eq!(
        server.requests().into_iter().filter(|r| r.max_tokens > 0).count(),
        2
    );
}
#[test]
fn concurrency_is_bounded_and_monitor_snapshot_is_a_complete_line() {
    let env = TestEnv::new();
    setup(&env, "[]");
    for (index, id) in ["a", "b", "c"].iter().enumerate() {
        task_file(&env, id, id, index as u64);
    }
    let server = FakeServer::start(
        vec![ScriptTurn::text("done"); 3],
        FakeOpts {
            hold_headers_ms: 300,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let out = finish(command(&env, &server.url(), "2"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let events = rows(&out);
    let active: Vec<_> = events
        .iter()
        .filter(|r| r["event"] == "state")
        .map(|r| r["active"].as_array().unwrap().len())
        .collect();
    assert!(active.contains(&2));
    assert!(active.iter().all(|n| *n <= 2));
    let line = fs::read_to_string(env.work().join("queue/.oxa-queue-status.json")).unwrap();
    assert_eq!(line.lines().count(), 1);
    let status: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(status["finished"], 3);
    assert_eq!(status["active"], serde_json::json!([]));
}
#[test]
fn low_disk_never_starts_a_task_and_leaves_the_file_pending() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "task", "low", 1);
    let df = env.write("tmp/df", "#!/bin/sh\nprintf 'Avail\\n0\\n'\n");
    fs::set_permissions(df, fs::Permissions::from_mode(0o755)).unwrap();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let mut cmd = command(&env, &server.url(), "2");
    cmd.args(["--min-free-mib", "1"]).env(
        "PATH",
        format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
    );
    let out = finish(cmd);
    assert_eq!(out.status.code(), Some(1));
    assert!(server.requests().is_empty());
    let events = rows(&out);
    let status = events.last().unwrap();
    assert_eq!(status["disk_blocked"], true);
    assert_eq!(status["pending"], 1);
    assert!(!env.work().join("project/.git/oxa-work/low.json").exists());
    assert!(env.work().join("queue/task.json").exists());
}
#[test]
fn invalid_file_does_not_skip_later_valid_work() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write("work/queue/bad.json", "invalid");
    task_file(&env, "good", "good", 1);
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let out = finish(command(&env, &server.url(), "2"));
    assert_eq!(out.status.code(), Some(1));
    let events = rows(&out);
    assert!(events.iter().any(|r| r["event"] == "error"));
    assert!(events.iter().any(|r| r["event"] == "task" && r["id"] == "good"));
}
#[test]
fn a_surviving_worker_counts_against_max_until_its_lock_releases() {
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "held", "held", 20);
    task_file(&env, "next", "next", 10);
    env.write("work/project/.git/oxa-work/held.lock", "");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(env.work().join("project/.git/oxa-work/held.lock"))
        .unwrap();
    file.try_lock().unwrap();
    let server = FakeServer::start(vec![ScriptTurn::text("done"); 2], FakeOpts::default()).unwrap();
    let mut child = command(&env, &server.url(), "1")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    until(|| {
        fs::read(env.work().join("queue/.oxa-queue-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["active"] == serde_json::json!(["held"]))
    });
    assert!(server.requests().is_empty());
    drop(file);
    assert!(child.wait().unwrap().success());
    git(
        &env,
        &env.work().join("project"),
        &["show-ref", "--verify", "refs/heads/agent/next"],
    );
}
#[test]
fn files_arriving_while_a_worker_runs_are_picked_up_before_once_exits() {
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "first", "first", 1);
    let server = FakeServer::start(
        vec![ScriptTurn::text("done"); 2],
        FakeOpts {
            hold_headers_ms: 400,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut child = command(&env, &server.url(), "1")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    until(|| !server.requests().is_empty());
    task_file(&env, "arrived", "arrived", 1);
    assert!(child.wait().unwrap().success());
    let path = env.work().join("project/.git/oxa-work/arrived.json");
    assert!(path.exists());
}

#[test]
fn invalid_max_is_rejected_without_starting_workers() {
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "one", "one", 1);
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let out = finish(command(&env, &server.url(), "0"));
    assert_eq!(out.status.code(), Some(2));
    assert!(server.requests().is_empty());
    assert!(!env.work().join("queue/.oxa-queue-status.json").exists());
}
#[test]
fn queued_workers_keep_the_parent_command_line_offline_policy() {
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "remote", "remote", 1);
    let server = FakeServer::start(
        vec![ScriptTurn::text("should not be requested")],
        FakeOpts {
            remote: Some("openrouter"),
            models: vec![oxa_fake::FakeModel {
                alias: "fake-model".into(),
                cap: 64000,
                max_output: Some(4096),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_oxa"));
    c.args([
        "--offline",
        "work",
        "--queue",
        "../queue",
        "--max",
        "2",
        "--once",
        "--min-free-mib",
        "0",
    ])
    .current_dir(env.work().join("project"))
    .env_clear()
    .envs(env.envs(&server.url()));
    let out = finish(c);
    assert_eq!(out.status.code(), Some(1));
    assert!(server.requests().is_empty());
    let events = rows(&out);
    assert!(events.iter().any(|r| r["event"] == "task"
        && r["outcome"] == "blocked"
        && r["detail"].as_str().is_some_and(|s| s.contains("offline"))));
}

#[test]
fn preparing_git_keeps_ownership_after_a_queue_controller_is_killed() {
    use std::os::unix::fs::PermissionsExt;
    struct Release {
        release: std::path::PathBuf,
        done: std::path::PathBuf,
    }
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.release, "release");
            let end = Instant::now() + Duration::from_secs(2);
            while !self.done.exists() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "preparing", "preparing", 1);
    let release = Release {
        release: env.home().join("release-prep"),
        done: env.home().join("prep-done"),
    };
    let hook = env.write(
        "work/project/.git/hooks/post-checkout",
        r#"#!/bin/sh
touch "$HOME/prep-started"
attempt=0
while ! test -f "$HOME/release-prep"; do
  attempt=$((attempt + 1))
  if test "$attempt" -ge 500; then exit 9; fi
  sleep 0.01
done
touch "$HOME/prep-done"
"#,
    );
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let server = FakeServer::start(vec![ScriptTurn::text("resumed")], FakeOpts::default()).unwrap();
    let mut child = command(&env, &server.url(), "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    until(|| env.home().join("prep-started").exists());
    child.kill().unwrap();
    child.wait().unwrap();
    let lock_path = env.work().join("project/.git/oxa-work/preparing.lock");
    let probe = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    assert!(
        matches!(probe.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "surviving checkout lost task ownership"
    );
    drop(probe);
    fs::write(&release.release, "release").unwrap();
    until(|| {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        file.try_lock().is_ok()
    });
    let out = finish(command(&env, &server.url(), "1"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(rows(&out)
        .iter()
        .any(|r| r["event"] == "task" && r["outcome"] == "nothing_to_do"));
}

#[test]
fn concurrent_same_problem_reports_search_and_file_serially() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "a", "a", 2);
    task_file(&env, "b", "b", 1);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"reportIssue":{"repo":"example/project"}}}"#,
    );
    let gh=env.write("tmp/gh",r##"#!/bin/sh
case "$1 $2" in
  'issue list') if test -f "$HOME/reported"; then printf '[{"number":1}]'; else printf '[]'; fi ;;
  'pr list') printf '[]' ;;
  'issue view') printf '%s' '{"title":"oxa queue problem","body":"Same synthetic reproduction","url":"https://github.com/example/project/issues/1","comments":[]}' ;;
  'issue create') sleep 0.2; touch "$HOME/reported"; printf 'created\n' >> "$HOME/report-writes"; printf '%s' 'https://github.com/example/project/issues/1' ;;
  *) exit 7 ;;
esac
"##);
    let oxc = env.write("tmp/oxc", "#!/bin/sh\nprintf '{}'\n");
    for file in [gh, oxc] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let text="<oxa-work-reports>[{\"title\":\"oxa queue problem\",\"notes\":\"Same synthetic reproduction\",\"keywords\":[\"oxa queue\",\"same reproduction\"]}]</oxa-work-reports>";
    let server = FakeServer::start(vec![ScriptTurn::text(text); 2], FakeOpts::default()).unwrap();
    let mut c = command(&env, &server.url(), "2");
    c.arg("--report-problems").env(
        "PATH",
        format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
    );
    let out = finish(c);
    assert!(
        out.status.success(),
        "{} / {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(env.home().join("report-writes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let events = rows(&out);
    let dispositions: std::collections::HashSet<_> = events
        .iter()
        .filter(|r| r["event"] == "task")
        .map(|r| r["reports"][0]["disposition"].as_str().unwrap())
        .collect();
    assert!(
        dispositions.contains("filed") && dispositions.contains("existing_issue"),
        "{dispositions:?}"
    );
}
