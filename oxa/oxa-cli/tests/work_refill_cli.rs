use oxa_fake::TestEnv;
use serde_json::json;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

fn invoke(env: &TestEnv, config: &Path, token: &str, offline: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("GH_TOKEN", "synthetic-cached")
        .env(
            "PATH",
            format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
        );
    if offline {
        command.arg("--offline");
    }
    command
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
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(token.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn fixture() -> (TestEnv, std::path::PathBuf) {
    let env = TestEnv::new();
    assert!(Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(env.work())
        .output()
        .unwrap()
        .status
        .success());
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &env.work(), true)
        .unwrap();
    let controls = env.agent_dir().join("board-control");
    fs::create_dir(&controls).unwrap();
    fs::set_permissions(&controls, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(env.tmp(), fs::Permissions::from_mode(0o700)).unwrap();
    (env, controls)
}

#[test]
fn refill_startup_retains_pause_and_refuses_worker_writable_authority() {
    let (env, controls) = fixture();
    let config = controls.join("refill.json");
    let pause = controls.join("paused");
    let queue = controls.join("queue");
    let claims = controls.join("claims");
    for path in [&queue, &claims] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let sentinel = claims.join("preserved");
    fs::write(&sentinel, b"synthetic imported attempts and holds").unwrap();
    let write_config = |value: serde_json::Value| {
        fs::write(&config, value.to_string()).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    };
    write_config(json!({"enabled":false}));
    assert!(invoke(&env, &config, "", false).status.success());
    write_config(json!({"enabled":true,"queue":queue,"pause":pause,"claims":claims}));
    fs::write(&pause, b"owner paused").unwrap();
    // Separate real processes simulate a service starting again while the pause persists.
    for _ in 0..2 {
        assert!(invoke(&env, &config, "", false).status.success());
    }
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"synthetic imported attempts and holds"
    );
    assert_eq!(fs::read_dir(&claims).unwrap().count(), 1);
    assert_eq!(fs::read_dir(&queue).unwrap().count(), 0);
    assert_eq!(invoke(&env, &config, "", true).status.code(), Some(2));
    let outside = env.tmp().join("untrusted-control.json");
    fs::write(&outside, b"{\"enabled\":false}").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(invoke(&env, &outside, "", false).status.code(), Some(2));
}

#[test]
fn private_token_and_exact_task_ref_precede_one_durable_publication() {
    for pull_request in [false, true] {
        durable_publication(pull_request, false);
    }
}

#[test]
fn live_fix_packet_uses_current_app_context_and_the_original_durable_binding() {
    durable_publication(true, true);
}

fn durable_publication(pull_request: bool, live: bool) {
    use oxa_work::board::{assignment::ManualAuthority, AttemptState, Claims, ImportState};
    use std::collections::BTreeMap;
    let (env, controls) = fixture();
    let queue = controls.join("queue");
    let root = controls.join("claims");
    for path in [&queue, &root] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let claims = Claims::open(&root).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    claims
        .import_snapshot(
            Some(ManualAuthority::Owner),
            &BTreeMap::from([(
                44,
                ImportState {
                    state: AttemptState::default(),
                    excluded: false,
                },
            )]),
            now,
        )
        .unwrap();
    let task = controls.join("task.txt");
    let revision = if pull_request {
        "a".repeat(40)
    } else {
        "revision".into()
    };
    let live_task = format!(
        "c44 local/fake agent/44 agent/44\n{}Address pull request #44 at {revision}: Synthetic live PR\n\nCurrent protected source\n\\---\nNext section\n\nReview and conversation data\n\nConversation comment by fixture:\nFeedback\r\n\\ \t--- \t\r\nMore feedback\n\n",
        oxa_work::github_runner_guidance("pull request")
    );
    let valid = if live {
        live_task.as_str()
    } else if pull_request {
        "c44 local/fake agent/44 agent/44\nSynthetic prepared packet.\n"
    } else {
        "i44 local/fake agent/44 main\nSynthetic prepared packet.\n"
    };
    fs::write(&task, valid).unwrap();
    fs::set_permissions(&task, fs::Permissions::from_mode(0o600)).unwrap();
    let config = controls.join("refill.json");
    let mut settings = json!({"enabled":true,"host_id":"host-a","intake":true,"queue":queue,"claims":root,
        "pause":controls.join("paused"),"log":controls.join("audit.jsonl"),
        "worker":{"id":"oxa:node/helper","kind":"oxa","roles":["builder"],"skills":[]},
        "partition":{"modulus":22,"residues":[0]},
        "packets":[{"number":44,"revision":revision,"action":if pull_request {"resolve_conflict"} else {"build_issue"},"branch":"agent/44","taskfile":task}]
    });
    if live {
        settings.as_object_mut().unwrap().remove("packets");
        settings["live"] = json!(true);
        settings["intake"] = json!(false);
        settings["model"] = json!("local/fake");
        fs::write(
            &task,
            b"Unused manual packet; live mode must fetch current context.",
        )
        .unwrap();
    }
    fs::write(&config, settings.to_string()).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let gh = env.write(
        "tmp/gh",
        r#"#!/bin/sh
[ "$GH_TOKEN" = synthetic ] || exit 93
printf 'read\n' >> "$0.calls"
printf 'HTTP/2.0 200 OK\r\n\r\n'
cat "$0.reply"
"#,
    );
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
    let rows = if pull_request {
        vec![json!({"number":44,"headRefOid":revision,"headRefName":"agent/44",
        "closingIssuesReferences":{"totalCount":0,"nodes":[]}})]
    } else {
        vec![]
    };
    fs::write(env.tmp().join("gh.reply"), json!({"data":{"repository":{
        "mergeQueue":null,"pullRequests":{"totalCount":rows.len(),"nodes":rows,"pageInfo":{"hasNextPage":false,"endCursor":"last"}},
        "i44":{"number":44,"state":"OPEN","updatedAt":"revision"},"b44":null,
        "pullRequest":{"title":"Synthetic live PR","body":"Current protected source\n---\nNext section",
            "headRefOid":revision,"headRefName":"agent/44","isCrossRepository":false,
            "comments":{"totalCount":1,"nodes":[{"author":{"login":"fixture"},"body":"Feedback\r\n \t--- \t\r\nMore feedback"}]},"reviews":{"totalCount":0,"nodes":[]},
            "reviewThreads":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},
            "commits":{"nodes":[{"commit":{"oid":revision,"statusCheckRollup":null}}]}},
        "p44":{"number":44,"headRefOid":revision,"baseRefName":"main","state":"OPEN","body":"",
            "isDraft":false,"reviewDecision":null,"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY",
            "baseRef":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},
            "labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},
            "reviewThreads":{"totalCount":0,"nodes":[]},"latestOpinionatedReviews":{"totalCount":0,"nodes":[]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}
    }}}).to_string()).unwrap();
    let missing = invoke(&env, &config, "", false);
    assert_eq!(missing.status.code(), Some(2));
    if !live {
        assert!(String::from_utf8_lossy(&missing.stderr).contains("App token"));
    }
    assert!(!env.tmp().join("gh.calls").exists());
    if !live {
        fs::write(
            &task,
            b"i46 local/fake agent/44 main\nSynthetic prepared packet.\n",
        )
        .unwrap();
        let mismatch = invoke(&env, &config, "synthetic\n", false);
        assert_eq!(mismatch.status.code(), Some(2));
        assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
        assert_eq!(fs::read_dir(&queue).unwrap().count(), 0);
        fs::write(&task, valid).unwrap();
    }
    let original_config = fs::read(&config).unwrap();
    let mut hostless: serde_json::Value = serde_json::from_slice(&original_config).unwrap();
    hostless.as_object_mut().unwrap().remove("host_id");
    fs::write(&config, hostless.to_string()).unwrap();
    let refused = invoke(&env, &config, "synthetic\n", false);
    assert_eq!(refused.status.code(), Some(2), "native refill requires a host");
    assert!(!env.tmp().join("gh.calls").exists());
    fs::write(&config, original_config).unwrap();
    if live {
        let reply = env.tmp().join("gh.reply");
        let original = fs::read(&reply).unwrap();
        // Item 66 is inside the same partition but has no imported attempt row.
        let mut unknown: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let repository = &mut unknown["data"]["repository"];
        repository["pullRequests"]["nodes"][0]["number"] = json!(66);
        repository["p66"] = repository["p44"].take();
        repository["p66"]["number"] = json!(66);
        fs::write(&reply, unknown.to_string()).unwrap();
        let refused = invoke(&env, &config, "synthetic\n", false);
        assert!(
            refused.status.success(),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        let rows = claims.assignments().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            !rows.contains_key(&66),
            "unknown attempts must not become a zero-count claim"
        );
        assert_eq!(rows[&44].state.attempts, 0);
        assert!(fs::read_dir(&queue)
            .unwrap()
            .all(|entry| entry.unwrap().path().is_dir()));
        for (field, value) in [
            ("headRefOid", json!("b".repeat(40))),
            ("isCrossRepository", json!(true)),
        ] {
            let mut response: serde_json::Value = serde_json::from_slice(&original).unwrap();
            response["data"]["repository"]["pullRequest"][field] = value;
            fs::write(&reply, response.to_string()).unwrap();
            assert_eq!(invoke(&env, &config, "synthetic\n", false).status.code(), Some(2));
            assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
            assert!(fs::read_dir(&queue)
                .unwrap()
                .all(|entry| entry.unwrap().path().is_dir()));
        }
        fs::write(&reply, original).unwrap();
    }
    if !pull_request {
        let reply = env.tmp().join("gh.reply");
        let original = fs::read(&reply).unwrap();
        for observed in [
            Some(json!({"name":"agent/44","target":{"committer":{"user":{"login":"worker-a[bot]"}}}})),
            None,
        ] {
            let expected = if observed.is_some() {
                "worker-a[bot]"
            } else {
                "branch observation"
            };
            let mut response: serde_json::Value = serde_json::from_slice(&original).unwrap();
            if let Some(branch) = observed {
                response["data"]["repository"]["b44"] = branch;
            } else {
                response["data"]["repository"]
                    .as_object_mut()
                    .unwrap()
                    .remove("b44");
            }
            fs::write(&reply, response.to_string()).unwrap();
            let refused = invoke(&env, &config, "synthetic\n", false);
            assert_eq!(refused.status.code(), Some(2));
            let reason = String::from_utf8_lossy(&refused.stderr);
            assert!(reason.contains(expected), "{reason}");
            assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
        }
        fs::write(&reply, original).unwrap();
    }
    let result = invoke(&env, &config, "synthetic\n", false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let tasks: Vec<_> = fs::read_dir(&queue)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(tasks.len(), 1);
    assert_eq!(fs::read_to_string(&tasks[0]).unwrap(), valid);
    let binding = claims.assignments().unwrap()[&44]
        .data
        .native_binding
        .clone()
        .expect("CLI must retain native receipt");
    assert_eq!(binding.number, 44);
    assert_eq!(binding.branch, "agent/44");
    assert_eq!(
        binding.reviewed_head.as_deref(),
        pull_request.then_some(revision.as_str())
    );
    assert!(binding.lease == claims.snapshot().unwrap()[&44].lease.clone().unwrap());
    assert_eq!(
        tasks[0].file_name().unwrap().to_str().unwrap(),
        format!("board-{}.txt", binding.key)
    );
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
    assert!(invoke(&env, &config, "synthetic\n", false).status.success());
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
    if !pull_request {
        let original_config = fs::read(&config).unwrap();
        let original_reply = fs::read(env.tmp().join("gh.reply")).unwrap();
        assert!(claims
            .release_for(
                44,
                &binding.lease.holder,
                &binding.lease,
                binding.lease.claimed_at.unwrap(),
                "verified completion"
            )
            .unwrap());
        let mut next: serde_json::Value = serde_json::from_slice(&original_config).unwrap();
        next["packets"][0]["branch"] = "agent/replacement".into();
        next["packets"][0]["revision"] = "revision-two".into();
        fs::write(&config, next.to_string()).unwrap();
        fs::write(
            &task,
            "i44 local/fake agent/replacement main\nSynthetic prepared packet.\n",
        )
        .unwrap();
        let mut response: serde_json::Value = serde_json::from_slice(&original_reply).unwrap();
        response["data"]["repository"]["i44"]["updatedAt"] = "revision-two".into();
        response["data"]["repository"]["r44"] = json!({"name":"agent/44"});
        fs::write(env.tmp().join("gh.reply"), response.to_string()).unwrap();
        let refused = invoke(&env, &config, "synthetic\n", false);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "retained branch must block a replacement"
        );
        assert!(String::from_utf8_lossy(&refused.stderr).contains(&binding.lease.holder));
        assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
        fs::write(&config, original_config).unwrap();
        fs::write(env.tmp().join("gh.reply"), original_reply).unwrap();
        fs::write(&task, valid).unwrap();
    }
    let peer = env
        .work()
        .parent()
        .unwrap()
        .join(".oxa-worktrees")
        .join(env.work().file_name().unwrap())
        .join("synthetic-task");
    fs::create_dir_all(&peer).unwrap();
    fs::set_permissions(&peer, fs::Permissions::from_mode(0o700)).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    for key in ["queue", "claims", "pause", "log"] {
        let path = peer.join(key);
        if matches!(key, "queue" | "claims") {
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut unsafe_config = original.clone();
        unsafe_config[key] = json!(path);
        fs::write(&config, unsafe_config.to_string()).unwrap();
        assert_eq!(
            invoke(&env, &config, "synthetic\n", false).status.code(),
            Some(2),
            "worker-writable {key} must be refused",
        );
    }
}
