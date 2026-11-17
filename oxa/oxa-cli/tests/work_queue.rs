//! Queue behavior against real worktrees and a fake engine, with no external network.
#[path = "common/work_confirm.rs"]
mod confirm;
#[path = "common/work_repo.rs"]
mod work;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use sha2::{Digest, Sha256};
use std::{
    fs,
    process::{Command, Output},
    time::{Duration, Instant, SystemTime},
};
use work::{git, setup};
fn storage(env: &TestEnv) -> std::path::PathBuf {
    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key: String = Sha256::digest(common.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    env.agent_dir().join("work").join(key)
}
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
fn finish(c: Command) -> Output {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(confirm::output(c));
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
    assert!(!storage(&env).join("low.json").exists());
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
    fs::create_dir_all(storage(&env)).unwrap();
    fs::write(storage(&env).join("held.lock"), "").unwrap();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(storage(&env).join("held.lock"))
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
    let path = storage(&env).join("arrived.json");
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
    c.env_clear()
        .args([
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
    let tree = env.work().join(".oxa-worktrees/project/preparing");
    env.write(
        "work/project/.git/info/exclude",
        ".prep-started\n.release-prep\n.prep-done\n",
    );
    let release = Release {
        release: tree.join(".release-prep"),
        done: tree.join(".prep-done"),
    };
    let hook = env.write(
        "work/project/.git/hooks/post-checkout",
        r#"#!/bin/sh
touch .prep-started
attempt=0
while ! test -f .release-prep; do
  attempt=$((attempt + 1))
  if test "$attempt" -ge 500; then exit 9; fi
  sleep 0.01
done
touch .prep-done
"#,
    );
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let server = FakeServer::start(vec![ScriptTurn::text("resumed")], FakeOpts::default()).unwrap();
    let mut child = command(&env, &server.url(), "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    until(|| tree.join(".prep-started").exists());
    child.kill().unwrap();
    child.wait().unwrap();
    let lock_path = storage(&env).join("preparing.lock");
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
  'issue create')
    for fd in /proc/self/fd/*; do
      if test "$fd" -ef "$HOME/.oxa/agent/work-report.lock"; then
        touch "$HOME/report-lease-inherited"
      fi
    done
    sleep 0.2; touch "$HOME/reported"; printf 'created\n' >> "$HOME/report-writes"; printf '%s' 'https://github.com/example/project/issues/1' ;;
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
    assert!(
        env.home().join("report-lease-inherited").exists(),
        "a surviving GitHub writer must retain the reporting lease"
    );
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

fn has_active(env: &TestEnv, count: usize) -> bool {
    fs::read(env.work().join("queue/.oxa-queue-status.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|state| state["active"].as_array().is_some_and(|ids| ids.len() == count))
}

#[test]
fn appended_tasks_and_archived_inputs_do_not_fail_or_interrupt_active_work() {
    use std::process::Stdio;
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "input", "first", 1);
    let server = FakeServer::start(
        vec![ScriptTurn::text("done"); 2],
        FakeOpts {
            hold_headers_ms: 700,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let child = command(&env, &server.url(), "2")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    until(|| has_active(&env, 1));
    let file = env.work().join("queue/input.json");
    let mut tasks: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    tasks.push(
        serde_json::json!({"id":"second","model":"fake","branch":"agent/second","prompt":"second task"}),
    );
    fs::write(&file, serde_json::to_vec(&tasks).unwrap()).unwrap();
    until(|| has_active(&env, 2));
    fs::create_dir(env.work().join("archive")).unwrap();
    fs::rename(&file, env.work().join("archive/input.json")).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{} / {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let events = rows(&out);
    assert_eq!(events.iter().filter(|v| v["event"] == "task").count(), 2);
    assert_eq!(events.last().unwrap()["failed"], 0);
}

#[test]
fn correcting_a_malformed_file_clears_its_queue_error() {
    use std::process::Stdio;
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write("work/queue/bad.json", "{");
    task_file(&env, "active", "active", 1);
    let server = FakeServer::start(
        vec![ScriptTurn::text("done"); 2],
        FakeOpts {
            hold_headers_ms: 700,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let child = command(&env, &server.url(), "2")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    until(|| has_active(&env, 1));
    task_file(&env, "bad", "corrected", 0);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{} / {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(rows(&out).last().unwrap()["failed"], 0);
}

#[test]
fn a_monitor_error_drains_admitted_workers_before_returning() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "input", "active", 1);
    let finished = Arc::new(AtomicBool::new(false));
    let observed = finished.clone();
    let policy = oxa_work::RunPolicy {
        git_binary: "/usr/bin/git".into(),
        host_path: "/usr/bin:/bin".into(),
        terminate_binary: None,
        offline: true,
        storage: env.agent_dir().join("work/monitor-fixture"),
    };
    let result = oxa_work::run_queue(
        &env.work().join("project"),
        &env.work().join("queue"),
        &oxa_work::QueueOptions {
            max: 1,
            min_free_bytes: 0,
            once: true,
        },
        &policy,
        std::path::Path::new("/usr/bin/df"),
        Arc::new(move |task| {
            std::thread::sleep(Duration::from_millis(100));
            observed.store(true, Ordering::Release);
            Ok(oxa_work::blocked_record(task, "synthetic worker result".into()))
        }),
        &mut |_| Err("synthetic monitor failure".into()),
    );
    assert!(result.unwrap_err().contains("synthetic monitor failure"));
    assert!(
        finished.load(Ordering::Acquire),
        "controller returned before its admitted worker finished"
    );
}

#[test]
fn duplicate_branches_in_separate_files_are_rejected_before_admission() {
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "first", "first", 2);
    env.write(
        "work/queue/second.json",
        r#"[{"id":"second","model":"fake","branch":"agent/first","prompt":"Synthetic duplicate branch"}]"#,
    );
    let server = FakeServer::start(vec![ScriptTurn::text("done"); 2], FakeOpts::default()).unwrap();
    let out = finish(command(&env, &server.url(), "1"));
    let events = rows(&out);
    assert!(
        events.iter().any(|row| row["event"] == "error"
            && row["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("branch"))),
        "{events:?}"
    );
    assert!(!events
        .iter()
        .any(|row| row["event"] == "task" && row["id"] == "second"));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_full_common_git_volume_blocks_admission_independently() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "input", "input", 1);
    let df = env.write("tmp/df", "#!/bin/sh\nfor arg do checked=$arg; done\nif test \"$checked\" = \"$OXA_TEST_COMMON_GIT\"; then printf '0\\n'; else printf '1000000000000\\n'; fi\n");
    fs::set_permissions(df, fs::Permissions::from_mode(0o755)).unwrap();
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let mut cmd = command(&env, &server.url(), "1");
    cmd.args(["--min-free-mib", "512"])
        .env(
            "OXA_TEST_COMMON_GIT",
            env.work().join("project/.git").canonicalize().unwrap(),
        )
        .env(
            "PATH",
            format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
        );
    let out = finish(cmd);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(rows(&out).last().unwrap()["disk_blocked"], true);
    assert!(server.requests().is_empty());
}

#[test]
fn a_retained_busy_lease_without_an_input_counts_against_max() {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let env = TestEnv::new();
    setup(&env, "[]");
    task_file(&env, "next", "next", 1);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(storage(&env))
        .unwrap();
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(storage(&env).join("retained.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    let server = FakeServer::start(
        vec![ScriptTurn::text("done")],
        FakeOpts {
            hold_headers_ms: 700,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let child = command(&env, &server.url(), "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let status = env.work().join("queue/.oxa-queue-status.json");
    until(|| status.exists());
    let before: serde_json::Value = serde_json::from_slice(&fs::read(status).unwrap()).unwrap();
    let requests = server.requests().len();
    drop(lease);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(before["active"], serde_json::json!(["retained"]), "{before}");
    assert_eq!(requests, 0, "a retained lease must block another admission");
    assert_eq!(server.requests().len(), 1);
}
