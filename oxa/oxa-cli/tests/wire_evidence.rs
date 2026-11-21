//! Real foreground terminal runs against a synthetic engine. No credentials or live models.
#![cfg(target_os = "linux")]

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_oxa");
const PROMPT: &str = "synthetic-private-prompt";
const REPORT: &str = "Wire adjacency (oxa handoff only): ";

fn run(env: &TestEnv, capture: bool, agent: bool) -> (String, Vec<Recorded>) {
    let replies: Vec<_> = (1..=9).map(|i| format!("synthetic-reply-{i}")).collect();
    let server = FakeServer::start(
        replies.iter().map(|r| ScriptTurn::text(r)).collect(),
        FakeOpts::default(),
    )
    .unwrap();
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 36,
        ws_col: 160,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: both output descriptors point to valid storage and are owned on success.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    let mut reader = master.try_clone().unwrap();
    let timer = env.work().join("wire-timing");
    let timing = !agent && Path::new("/usr/bin/time").is_file();
    let mut cmd = if timing {
        let mut cmd = Command::new("/usr/bin/time");
        cmd.args(["-f", "wall_s=%e user_s=%U system_s=%S peak_rss_kib=%M", "-o"])
            .arg(&timer)
            .arg(BIN);
        cmd
    } else {
        Command::new(BIN)
    };
    cmd.process_group(0)
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("TERM", "xterm-256color")
        .current_dir(env.work())
        .args([
            "--yolo",
            "--model",
            "fake-model",
            "--no-tools",
            "--no-mcp",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--tui-mode",
            "regular",
        ])
        .stdin(slave.try_clone().unwrap())
        .stdout(slave)
        .stderr(Stdio::piped());
    if capture {
        cmd.arg("--wire-evidence");
    }
    if agent {
        cmd.env("AI_AGENT", "oxa");
    }
    let mut child = cmd.spawn().unwrap();
    // Drop Command's stdio handles so closing the child's slave ends the reader.
    drop(cmd);
    if !agent {
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let exchange = (|| -> Option<()> {
            let mut bytes = Vec::new();
            let mut wait_for = |marker: &str| {
                let limit = Instant::now() + Duration::from_secs(15);
                loop {
                    if let Some(index) = bytes.windows(marker.len()).position(|w| w == marker.as_bytes()) {
                        bytes.drain(..index + marker.len());
                        return Some(());
                    }
                    bytes.extend(
                        rx.recv_timeout(limit.checked_duration_since(Instant::now())?)
                            .ok()?,
                    );
                }
            };
            wait_for("READY")?;
            for reply in replies {
                write!(master, "{PROMPT}\r").ok()?;
                wait_for(&reply)?;
                wait_for("READY")?;
            }
            master.write_all(b"\x04").ok()?;
            let limit = Instant::now() + Duration::from_secs(15);
            loop {
                match rx.recv_timeout(limit.checked_duration_since(Instant::now())?) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Some(()),
                    Err(mpsc::RecvTimeoutError::Timeout) => return None,
                    Ok(_) => {}
                }
            }
        })();
        if exchange.is_none() {
            // SAFETY: this is the process group we just created, including a timer's child.
            unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = child.wait();
            panic!("synthetic terminal exchange timed out");
        }
        reader.join().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.success(), !agent, "unexpected exit: {stderr}");
    if timing {
        eprintln!(
            "wire fixture (nine chats, capture={capture}): {}",
            std::fs::read_to_string(timer).unwrap().trim()
        );
    }
    (
        stderr,
        server
            .requests()
            .into_iter()
            .filter(|r| r.max_tokens > 0)
            .collect(),
    )
}

#[test]
fn terminal_capture_compares_adjacent_requests_without_changing_the_wire_or_exporting_it() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}},"cacheWarming":"off","retry":{"enabled":false}}"#);
    let (denied, requests) = run(&env, true, true);
    assert!(denied.contains("needs a foreground owner terminal"));
    assert!(requests.is_empty());
    for _ in 0..3 {
        let (quiet, before) = run(&env, false, false);
        let (observed, after) = run(&env, true, false);
        assert!(!quiet.contains(REPORT));
        let payload = observed
            .lines()
            .find_map(|line| line.strip_prefix(REPORT))
            .unwrap();
        let report: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(report["row_limit_reached"], false);
        let rows = report["rows"].as_array().unwrap();
        assert_eq!((before.len(), after.len(), rows.len()), (9, 9, 9));
        assert!(rows[0]["adjacent"].is_null());
        for (i, ((before, after), row)) in before.iter().zip(&after).zip(rows).enumerate() {
            assert_eq!(before.messages, after.messages);
            assert_eq!(before.system, after.system);
            assert_eq!(before.tools_json, after.tools_json);
            assert_eq!(before.usage, after.usage);
            assert_eq!(row["request"], i + 1);
            assert_eq!(row["attempt"], 1);
            assert_eq!(row["status"], "returned");
            assert_eq!(row["messages"], after.messages.len());
            if i > 0 {
                let adjacent = &row["adjacent"];
                assert_eq!(adjacent["common_leading_messages"], rows[i - 1]["messages"]);
                assert_eq!(adjacent["previous_messages_preserved"], true);
                assert_eq!(adjacent["system_same"], true);
                assert_eq!(adjacent["tools_same"], true);
            }
            for private in [
                PROMPT,
                "synthetic-reply-",
                &after.model,
                &after.session,
                &after.idem_key,
            ] {
                if !private.is_empty() {
                    assert!(!payload.contains(private));
                }
            }
        }
    }
}
