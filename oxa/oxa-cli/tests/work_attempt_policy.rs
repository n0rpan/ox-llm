//! The protected owner attempt limit travels through the real board commands.
#[path = "common/work_repo.rs"]
mod work;

use oxa_fake::TestEnv;
use oxa_work::board::{assignment::ManualAuthority, AttemptState, Claims, ImportState};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Output, Stdio},
};

#[test]
fn an_explicit_owner_limit_admits_the_fourth_attempt_and_readback_agrees() {
    let env = TestEnv::new();
    work::setup(&env, "[]");
    let controls = env.agent_dir().join("attempt-limit");
    let queue = controls.join("queue");
    let claim_path = controls.join("claims");
    for dir in [&controls, &queue, &claim_path] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let number = 44;
    let claims = Claims::open(&claim_path).unwrap();
    let now = oxa_proto::time::now_ms() / 1000;
    claims
        .import_snapshot(
            Some(ManualAuthority::Owner),
            &BTreeMap::from([(
                number,
                ImportState {
                    state: AttemptState {
                        utc_day: now / 86_400,
                        attempts: 3,
                        lease: None,
                    },
                    excluded: false,
                },
            )]),
            now,
        )
        .unwrap();
    let packet = env.write(
        "home/.oxa/agent/attempt-limit/task.txt",
        "i44 fake agent/44 main\nSynthetic owner attempt.\n",
    );
    fs::set_permissions(&packet, fs::Permissions::from_mode(0o600)).unwrap();
    let config = controls.join("refill.json");
    let mut owner = json!({
        "enabled":true,"intake":true,"host_id":"host-a","queue":queue,"claims":claim_path,
        "pause":controls.join("paused"),"log":controls.join("audit.jsonl"),
        "worker":{"id":"oxa:fixture/worker","kind":"oxa","roles":["builder"],"skills":[]},
        "partition":{"modulus":22,"residues":[0]},
        "packets":[{"number":number,"revision":"fixture-revision","action":"build_issue","branch":"agent/44","taskfile":packet}]
    });
    let write_config = |value: &Value| {
        fs::write(&config, value.to_string()).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    };
    let gh = env.write(
        "home/.local/bin/gh",
        "#!/bin/sh\n[ \"$GH_TOKEN\" = synthetic ] || exit 93\nprintf 'HTTP/2.0 200 OK\\r\\n\\r\\n'\ncat \"$0.reply\"\n",
    );
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut reply = json!({"data":{"repository":{
        "mergeQueue":null,"pullRequests":{"totalCount":0,"nodes":[],
        "pageInfo":{"hasNextPage":false,"endCursor":"last"}},
        "i44":{"number":number,"state":"OPEN","updatedAt":"fixture-revision"},"b44":null
    }}});
    env.write("home/.local/bin/gh.reply", &reply.to_string());
    let path = std::env::join_paths(
        std::iter::once(gh.parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
        cmd.current_dir(env.work().join("project"))
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env("PATH", &path);
        cmd
    };
    let refill = || -> Output {
        let mut child = command()
            .args([
                "work",
                "board",
                "--refill",
                "--repo",
                "example/project",
                "--once",
                "--token-stdin",
                "--config",
            ])
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let sent = child.stdin.take().unwrap().write_all(b"synthetic\n");
        let output = child.wait_with_output().unwrap();
        if output.status.success() {
            sent.unwrap();
        }
        output
    };
    let readback = || {
        command()
            .args(["--offline", "work", "board", "--claims-readback", "--config"])
            .arg(&config)
            .output()
            .unwrap()
    };

    write_config(&owner);
    let output = refill();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(claims.snapshot().unwrap()[&number].attempts, 3);
    assert!(claims.snapshot().unwrap()[&number].lease.is_none());
    let output = readback();
    assert!(output.status.success(), "{output:?}");
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["max_attempts_per_day"], 3);

    owner["budget_pools"]["limits"]["max_attempts_per_day"] = json!(4);
    write_config(&owner);
    let output = refill();
    assert!(output.status.success(), "{output:?}");
    let assignments = claims.assignments_read_only().unwrap();
    let row = &assignments[&number];
    assert_eq!(row.state.attempts, 4, "owned fourth attempt: {output:?}");
    let binding = row.data.native_binding.as_ref().unwrap();
    assert!(row.state.lease.as_ref() == Some(&binding.lease));
    assert!(queue.join(format!("board-{}.txt", binding.key)).is_file());
    let output = readback();
    assert!(output.status.success(), "{output:?}");
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["max_attempts_per_day"], 4);

    assert!(claims
        .release_for(
            number,
            &binding.lease.holder,
            &binding.lease,
            oxa_proto::time::now_ms() / 1000,
            "verified synthetic completion",
        )
        .unwrap());
    let charged = claims.snapshot().unwrap()[&number].clone();
    assert_eq!(charged.attempts, 4);
    assert!(charged.lease.is_none());
    // A new revision has a different publication key, so a claim here is a fresh
    // eligible attempt that neither the published packet nor the released lease
    // can explain away.
    owner["packets"][0]["revision"] = json!("fixture-revision-2");
    reply["data"]["repository"]["i44"]["updatedAt"] = json!("fixture-revision-2");
    env.write("home/.local/bin/gh.reply", &reply.to_string());
    // #1448: the daily attempt threshold is advisory, not a refusal. The second
    // revision is the eligible attempt that only the cap could have refused, so
    // it must be claimed and carry exactly one operator warning on stderr.
    owner["budget_pools"]["limits"]["max_attempts_per_day"] = json!(4);
    write_config(&owner);
    let output = refill();
    assert!(output.status.success(), "{output:?}");
    let fifth = claims.snapshot().unwrap()[&number].clone();
    assert_eq!(
        fifth.attempts, 5,
        "the eligible above-threshold attempt was not claimed: {output:?}"
    );
    assert!(
        fifth.lease.is_some(),
        "the claimed fifth attempt holds no lease: {output:?}"
    );
    let warning = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        warning.matches("attempt 5 of 4").count(),
        1,
        "expected one operator warning for the above-threshold claim: {warning:?}"
    );
    for cap in [json!(4), json!(2), json!(0)] {
        owner["budget_pools"]["limits"]["max_attempts_per_day"] = cap.clone();
        write_config(&owner);
        let output = refill();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(claims.snapshot().unwrap()[&number], fifth);
        let output = readback();
        assert!(output.status.success(), "{output:?}");
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["max_attempts_per_day"], cap);
    }

    for cap in [
        Value::Null,
        json!("4"),
        json!(-1),
        json!(4.5),
        json!(4294967296_u64),
        json!(true),
        json!([]),
        json!({}),
    ] {
        owner["budget_pools"]["limits"]["max_attempts_per_day"] = cap;
        write_config(&owner);
        assert_eq!(readback().status.code(), Some(2));
        assert_eq!(refill().status.code(), Some(2));
        assert_eq!(claims.snapshot().unwrap()[&number], fifth);
    }
    owner["budget_pools"]["limits"]["max_attempts_per_day"] = json!(4);
    let project_config = env.work().join("project/owner-policy.json");
    fs::write(&project_config, owner.to_string()).unwrap();
    fs::set_permissions(&project_config, fs::Permissions::from_mode(0o600)).unwrap();
    let output = command()
        .args(["--offline", "work", "board", "--claims-readback", "--config"])
        .arg(&project_config)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(claims.snapshot().unwrap()[&number], charged);
}
