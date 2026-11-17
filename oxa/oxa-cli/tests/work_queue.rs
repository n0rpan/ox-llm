use oxa_fake::TestEnv;
use oxa_work::{board::Policy, queue::publish};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

const TASK: &str =
    "c42 local/fake agent/42-fix agent/42-fix\nSOURCE_PR: 42\nSynthetic bounded source packet.\n";

fn enabled() -> Policy {
    Policy {
        enabled: true,
        ..Policy::default()
    }
}

#[test]
fn default_off_and_invalid_packets_never_publish() {
    let env = TestEnv::new();
    let queue = env.tmp().join("queue");
    assert!(!publish(&queue, "invalid", "invalid", false, &Policy::default()).unwrap());
    assert!(!queue.exists());
    assert!(publish(&queue, &"a".repeat(64), "missing packet", false, &enabled()).is_err());
    assert!(!queue.exists());
}

#[test]
fn one_claim_cannot_publish_a_second_task_block() {
    let env = TestEnv::new();
    let queue = env.tmp().join("queue");
    fs::create_dir(&queue).unwrap();
    let packet = format!("{TASK}---\n{TASK}");
    assert!(publish(&queue, &"c".repeat(64), &packet, false, &enabled()).is_err());
    assert_eq!(fs::read_dir(queue).unwrap().count(), 0);
}

#[test]
fn complete_private_publication_dedups_started_and_priority_changes() {
    let env = TestEnv::new();
    let queue = env.tmp().join("queue");
    fs::create_dir(&queue).unwrap();
    let key = "a".repeat(64);
    assert!(publish(&queue, &key, TASK, true, &enabled()).unwrap());
    let name = format!("p1-board-{key}.txt");
    let path = queue.join(&name);
    assert_eq!(fs::read_to_string(&path).unwrap(), TASK);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    fs::rename(path, queue.join("started").join(&name)).unwrap();
    assert!(!publish(&queue, &key, TASK, false, &enabled()).unwrap());
    assert!(!publish(&queue, &key, TASK, true, &enabled()).unwrap());
    assert_eq!(fs::read_dir(&queue).unwrap().count(), 1);
    assert_eq!(
        fs::read_to_string(queue.join("started").join(name)).unwrap(),
        TASK
    );
}

#[test]
fn concurrent_publishers_create_exactly_one_complete_task() {
    if let Some(queue) = std::env::var_os("OXA_TEST_BOARD_QUEUE") {
        publish(
            std::path::Path::new(&queue),
            &"b".repeat(64),
            TASK,
            false,
            &enabled(),
        )
        .unwrap();
        return;
    }
    let env = TestEnv::new();
    let queue = env.tmp().join("queue");
    fs::create_dir(&queue).unwrap();
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "concurrent_publishers_create_exactly_one_complete_task",
                ])
                .env_clear()
                .envs(env.envs("http://127.0.0.1:1"))
                .env("OXA_TEST_BOARD_QUEUE", &queue)
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let tasks: Vec<_> = fs::read_dir(&queue)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    assert_eq!(tasks.len(), 1);
    assert_eq!(fs::read_to_string(&tasks[0]).unwrap(), TASK);
    assert_eq!(fs::read_dir(queue).unwrap().count(), 2);
}
