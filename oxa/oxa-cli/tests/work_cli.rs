//! Real child oxa with fake engine and local bare git remote; no network.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn storage(env: &TestEnv) -> PathBuf {
    use sha2::{Digest, Sha256};
    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key: String = Sha256::digest(common.as_os_str().as_encoded_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    env.agent_dir().join("work").join(key)
}
fn git(env: &TestEnv, dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().into()
}
fn setup(env: &TestEnv, done: &str) {
    git(env, &env.work(), &["init", "--bare", "remote.git"]);
    git(env, &env.work(), &["init", "-b", "main", "project"]);
    let project = env.work().join("project");
    git(env, &project, &["config", "user.name", "Test"]);
    git(env, &project, &["config", "user.email", "test@example.invalid"]);
    env.write(
        "work/project/.oxa/settings.json",
        &format!(r#"{{"done":{done}}}"#),
    );
    git(env, &project, &["add", "."]);
    git(env, &project, &["commit", "-m", "fixture"]);
    git(env, &project, &["remote", "add", "origin", "../remote.git"]);
    git(env, &project, &["push", "origin", "main"]);
    // Approve the source repository only. The separate worker worktree still
    // uses the normal project-resource decision of its own child session.
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &project, true)
        .unwrap();
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task"}]"#,
    );
}
// Host-tool lookup correctly rejects group/world-writable PATH entries. Build
// trusted fixtures with explicit permissions, independent of the runner's umask.
fn trusted_tools(env: &TestEnv) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let tools = env.tmp().join("trusted-tools");
    std::fs::DirBuilder::new().mode(0o700).create(&tools).unwrap();
    tools
}
fn enable_test_subagent(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"subagent":{"enabled":true}}}"#,
    );
    env.write(
        "home/.oxa/agent/agents/scout.md",
        "---\nname: scout\ndescription: Reads local files\ntools: read\n---\nRead local files.\n",
    );
}
fn run(env: &TestEnv, turns: Vec<ScriptTurn>) -> serde_json::Value {
    run_args(env, turns, &["work", "../tasks.json"], None)
}
fn run_args(env: &TestEnv, turns: Vec<ScriptTurn>, args: &[&str], path: Option<String>) -> serde_json::Value {
    let mut rows = run_records(env, turns, args, path, &[]);
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}
fn run_records(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
    extra: &[(&str, &str)],
) -> Vec<serde_json::Value> {
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    run_server_records(env, &server, args, path, extra, true)
}
fn run_server_records(
    env: &TestEnv,
    server: &FakeServer,
    args: &[&str],
    path: Option<String>,
    extra: &[(&str, &str)],
    expect_output: bool,
) -> Vec<serde_json::Value> {
    let out = run_server(env, server, args, path, extra);
    assert!(
        !expect_output || !out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{e}: {line} / {}", String::from_utf8_lossy(&out.stderr)))
        })
        .collect()
}

fn run_server(
    env: &TestEnv,
    server: &FakeServer,
    args: &[&str],
    path: Option<String>,
    extra: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .args(args)
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.envs(extra.iter().copied());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(command.output().unwrap()).unwrap();
    });
    rx.recv_timeout(std::time::Duration::from_secs(90))
        .expect("runner finished")
}
#[test]
fn task_usage_reads_one_checkpoint_without_exposing_the_task_record() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![ScriptTurn::text("Measured worker")]);
    assert_eq!(record["task_tokens"]["complete"], true);
    let dir = storage(&env);
    std::fs::write(dir.join("unrelated.json"), "not a checkpoint").unwrap();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let usage = |id| {
        let mut rows = run_server_records(&env, &server, &["work", "status", "--usage", id], None, &[], true);
        assert_eq!(rows.len(), 1);
        rows.remove(0)
    };
    let revision = &record["checkpoint_revision"];
    assert_eq!(
        usage("test"),
        serde_json::json!({"task":"test","checkpoint_revision":revision,"task_tokens":record["task_tokens"]})
    );
    assert!(usage("missing").is_null());

    let file = dir.join("test.json");
    let mut checkpoint: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let partial = serde_json::json!({
        "attempts":2,"complete_attempts":1,"prompt":null,"completion":0,"cached":null,"complete":false
    });
    checkpoint["record"]["task_tokens"] = partial.clone();
    std::fs::write(&file, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    assert_eq!(
        usage("test"),
        serde_json::json!({"task":"test","checkpoint_revision":revision,"task_tokens":partial})
    );
    checkpoint["record"]
        .as_object_mut()
        .unwrap()
        .remove("task_tokens");
    std::fs::write(file, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    assert_eq!(
        usage("test"),
        serde_json::json!({"task":"test","checkpoint_revision":revision,"task_tokens":null})
    );
    assert!(server.requests().is_empty());
}

#[test]
fn external_checks_run_without_edits_and_retain_worktree() {
    let env = TestEnv::new();
    setup(&env, r#"["printf checked > receipt"]"#);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(tree.join("receipt")).unwrap(), "checked");
    assert!(storage(&env).join("test.json").exists());
}
#[test]
fn clean_task_is_nothing_to_do() {
    let env = TestEnv::new();
    setup(&env, "[]");
    assert_eq!(
        run(&env, vec![ScriptTurn::text("nothing")])["outcome"],
        "nothing_to_do"
    );
}

#[test]
fn native_refill_runs_one_bound_attempt_through_real_cli_completion() {
    native_refill_completion(NativeSource::Issue);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeSource {
    Issue,
    Reviewed,
    LiveReviewed,
    StaleModel,
    MissingHead,
    MismatchedPacket,
    AdvancedBranch,
}

#[test]
fn native_refill_executes_the_exact_reviewed_pull_request() {
    native_refill_completion(NativeSource::Reviewed);
}

#[test]
fn native_refill_delivers_live_review_context_to_the_bound_runner() {
    native_refill_completion(NativeSource::LiveReviewed);
}

#[test]
fn native_refill_refuses_a_stale_model_without_trying_a_similar_alias() {
    native_refill_completion(NativeSource::StaleModel);
}

#[test]
fn native_refill_refuses_unverified_pull_request_sources_before_model_use() {
    for source in [
        NativeSource::MissingHead,
        NativeSource::MismatchedPacket,
        NativeSource::AdvancedBranch,
    ] {
        native_refill_completion(source);
    }
}

fn native_refill_completion(source: NativeSource) {
    use oxa_work::board::{assignment::ManualAuthority, AttemptState, Claims, ImportState};
    use std::{collections::BTreeMap, fs, io::Write, os::unix::fs::PermissionsExt, process::Stdio};
    let env = TestEnv::new();
    setup(&env, r#"["printf checked > native-acceptance"]"#);
    let project = env.work().join("project");
    let reviewed_head = (source != NativeSource::Issue).then(|| {
        git(&env, &project, &["switch", "-c", "agent/44"]);
        env.write("work/project/reviewed.txt", "reviewed fixture\n");
        git(&env, &project, &["add", "reviewed.txt"]);
        git(&env, &project, &["commit", "-m", "reviewed fixture"]);
        let head = git(&env, &project, &["rev-parse", "HEAD"]);
        git(&env, &project, &["switch", "main"]);
        head
    });
    let controls = env.agent_dir().join("native-controls");
    let queue = controls.join("queue");
    let root = controls.join("claims");
    for path in [&controls, &queue, &root] {
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
    let live = matches!(source, NativeSource::LiveReviewed | NativeSource::StaleModel);
    let taskfile = controls.join("task.txt");
    let header = if reviewed_head.is_some() {
        "c44 fake agent/44 agent/44"
    } else {
        "i44 fake agent/44 main"
    };
    fs::write(&taskfile, format!("{header}\nSynthetic admitted task.\n")).unwrap();
    fs::set_permissions(&taskfile, fs::Permissions::from_mode(0o600)).unwrap();
    let config = controls.join("refill.json");
    let mut settings = serde_json::json!({
            "enabled":true,"host_id":"host-a","nativeExecution":true,"intake":true,"queue":queue,"claims":root,
            "pause":controls.join("paused"),"log":controls.join("audit.jsonl"),
            "worker":{"id":"oxa:node/helper","kind":"oxa","roles":["builder"],"skills":[]},
            "partition":{"modulus":22,"residues":[0]},
            "packets":[{"number":44,"revision":reviewed_head.as_deref().unwrap_or("revision"),
                "action":if reviewed_head.is_some() { "resolve_conflict" } else { "build_issue" },
                "branch":"agent/44","taskfile":taskfile}]
    });
    if live {
        settings.as_object_mut().unwrap().remove("packets");
        settings["live"] = true.into();
        settings["intake"] = false.into();
        settings["model"] = "fake".into();
    }
    fs::write(&config, settings.to_string()).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let tools = trusted_tools(&env);
    let gh = tools.join("gh");
    fs::write(&gh, "#!/bin/sh\nprintf 'call\\n' >> \"$0.calls\"\n[ \"$GH_TOKEN\" = synthetic ] || exit 93\nprintf 'HTTP/2.0 200 OK\\r\\n\\r\\n'\ncat \"$0.reply\"\n").unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
    let rows: Vec<_> = reviewed_head
        .as_ref()
        .map(|head| {
            serde_json::json!({"number":44,"headRefOid":head,"headRefName":"agent/44",
            "closingIssuesReferences":{"totalCount":0,"nodes":[]}})
        })
        .into_iter()
        .collect();
    fs::write(tools.join("gh.reply"), serde_json::json!({"data":{"repository":{
        "mergeQueue":null,"pullRequests":{"totalCount":rows.len(),"nodes":rows,
            "pageInfo":{"hasNextPage":false,"endCursor":"last"}},
        "i44":{"number":44,"state":"OPEN","updatedAt":"revision"},"b44":null,
        "pullRequest":{"title":"Synthetic live PR","body":"Synthetic live PR body.",
            "headRefOid":reviewed_head,"headRefName":"agent/44","isCrossRepository":false,
            "comments":{"totalCount":1,"nodes":[{"author":{"login":"fixture"},"body":"Synthetic live review note."}]},
            "reviews":{"totalCount":0,"nodes":[]},"reviewThreads":{"totalCount":0,"nodes":[]},
            "files":{"totalCount":0,"nodes":[]},"commits":{"nodes":[{"commit":{"oid":reviewed_head,"statusCheckRollup":null}}]}},
        "p44":{"number":44,"headRefOid":reviewed_head,"baseRefName":"main","state":"OPEN","body":"",
            "isDraft":false,"reviewDecision":null,"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY",
            "baseRef":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},
            "labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},
            "reviewThreads":{"totalCount":0,"nodes":[]},"latestOpinionatedReviews":{"totalCount":0,"nodes":[]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}
    }}}).to_string()).unwrap();
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("PATH", &path)
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
        .stderr(Stdio::piped());
    let mut refill = || {
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"synthetic\n").unwrap();
        let published = child.wait_with_output().unwrap();
        assert!(
            published.status.success(),
            "{}",
            String::from_utf8_lossy(&published.stderr)
        );
    };
    refill();
    let binding = claims.assignments().unwrap()[&44]
        .data
        .native_binding
        .clone()
        .unwrap();
    let name = format!("board-{}.txt", binding.key);
    let live_app_reads = live.then(|| fs::read(tools.join("gh.calls")).unwrap());
    let packet = fs::read_to_string(queue.join(&name)).unwrap();
    if live {
        assert!(packet.contains("Synthetic live review note."));
        assert!(!packet.contains("Synthetic admitted task."));
    } else {
        assert_eq!(packet, fs::read_to_string(&taskfile).unwrap());
    }
    assert!(claims.snapshot().unwrap()[&44].lease.as_ref() == Some(&binding.lease));
    assert_eq!(binding.reviewed_head, reviewed_head);
    let rows = claims.assignments().unwrap();
    assert_eq!(rows[&44].data.item_owner.as_ref().unwrap().host_id, "host-a");
    let refusal = match source {
        NativeSource::MissingHead => {
            // Exercise a legacy persisted receipt with no reviewed-source field.
            let saved = root.join("44.board-claim.json");
            let mut row: serde_json::Value = serde_json::from_slice(&fs::read(&saved).unwrap()).unwrap();
            assert!(row["data"]["native_binding"]
                .as_object_mut()
                .unwrap()
                .remove("reviewed_head")
                .is_some());
            fs::write(saved, serde_json::to_vec(&row).unwrap()).unwrap();
            Some("retained reviewed head")
        }
        NativeSource::MismatchedPacket => {
            fs::write(
                queue.join(&name),
                "c45 fake agent/44 agent/44\nMismatched PR source.\n",
            )
            .unwrap();
            Some("bound source")
        }
        NativeSource::AdvancedBranch => {
            git(&env, &project, &["switch", "agent/44"]);
            git(
                &env,
                &project,
                &["commit", "--allow-empty", "-m", "unreviewed fixture"],
            );
            git(&env, &project, &["switch", "main"]);
            Some("not at the reviewed head")
        }
        NativeSource::Issue
        | NativeSource::Reviewed
        | NativeSource::LiveReviewed
        | NativeSource::StaleModel => None,
    };
    let args = ["work", "--native-queue", "--config", config.to_str().unwrap()];
    if source == NativeSource::StaleModel {
        let server = FakeServer::start(
            vec![ScriptTurn::text("unexpected substitute")],
            FakeOpts {
                model: "fake-alternative".into(),
                ..FakeOpts::default()
            },
        )
        .unwrap();
        let _result = run_server(&env, &server, &args, Some(path.clone()), &[]);
        let requested: Vec<_> = server.requests().into_iter().map(|call| call.model).collect();
        assert!(
            requested.is_empty(),
            "stale model attempted these aliases: {requested:?}"
        );
        let checkpoint: serde_json::Value = serde_json::from_slice(
            &fs::read(storage(&env).join(format!("{}.json", binding.native_task_id))).unwrap(),
        )
        .unwrap();
        let detail = checkpoint["record"]["detail"].as_str().unwrap();
        assert!(detail.contains("absent from the current catalog"), "{detail}");
        let ordinary = run_server(
            &env,
            &server,
            &[
                "--model",
                "fake",
                "-p",
                "--no-session",
                "Ordinary model matching.",
            ],
            Some(path),
            &[],
        );
        assert!(ordinary.status.success(), "{ordinary:?}");
        assert_eq!(server.requests()[0].model, "fake-alternative");
        return;
    }
    let server = FakeServer::start(
        vec![ScriptTurn::text("finished")],
        FakeOpts {
            model: "fake".into(),
            ..FakeOpts::default()
        },
    )
    .unwrap();
    if let Some(reason) = refusal {
        let result = run_server(&env, &server, &args, Some(path), &[]);
        assert!(!result.status.success(), "{source:?}: {result:?}");
        let detail = if source == NativeSource::AdvancedBranch {
            let receipt = storage(&env).join(format!("{}.json", binding.native_task_id));
            let checkpoint: serde_json::Value = serde_json::from_slice(&fs::read(receipt).unwrap()).unwrap();
            checkpoint["record"]["detail"].as_str().unwrap().to_owned()
        } else {
            format!(
                "{}{}",
                String::from_utf8_lossy(&result.stderr),
                String::from_utf8_lossy(&result.stdout)
            )
        };
        assert!(detail.contains(reason), "{source:?}: {detail} / {result:?}");
        assert!(server.requests().is_empty(), "{source:?} reached the model");
        let state = &claims.assignments().unwrap()[&44].state;
        assert!(state.lease.as_ref() == Some(&binding.lease));
        assert_eq!(state.attempts, 1);
        return;
    }
    let records = run_server_records(&env, &server, &args, Some(path.clone()), &[], true);
    assert!(records
        .iter()
        .any(|row| row["event"] == "task" && row["id"] == binding.native_task_id));
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &fs::read(storage(&env).join(format!("{}.json", binding.native_task_id))).unwrap(),
    )
    .unwrap();
    if live {
        assert_eq!(checkpoint["task"]["model"], "fake");
    }
    let record = &checkpoint["record"];
    let launch_revision = record["worker_launch_revision"].as_u64().unwrap();
    assert!(launch_revision > 0 && launch_revision < record["checkpoint_revision"].as_u64().unwrap());
    assert_eq!(record["id"], binding.native_task_id);
    assert_eq!(record["branch"], binding.branch);
    assert_eq!(record["step"], "finished");
    assert_eq!(record["checkpoint_kind"], "task_transition");
    assert!(record["checkpoint_revision"].as_u64().unwrap() > 0);
    if let Some(head) = reviewed_head {
        assert_eq!(checkpoint["task"]["reviewed_pull_request"]["head"], head);
        assert_eq!(checkpoint["task"]["reviewed_pull_request"]["number"], 44);
        assert_eq!(checkpoint["execution"]["initial"], head);
        assert_eq!(
            fs::read_to_string(Path::new(record["worktree"].as_str().unwrap()).join("reviewed.txt")).unwrap(),
            "reviewed fixture\n"
        );
    }
    assert_eq!(
        fs::read_to_string(Path::new(record["worktree"].as_str().unwrap()).join("native-acceptance"))
            .unwrap(),
        "checked"
    );
    let completed = &claims.assignments().unwrap()[&44];
    assert!(completed.state.lease.is_none());
    assert_eq!(completed.state.attempts, 1);
    assert!(completed.data.native_binding.as_ref() == Some(&binding));
    let started = queue
        .join("started")
        .join(format!("native-board-{}", binding.key))
        .join(format!("{}.txt", binding.native_task_id));
    assert_eq!(fs::read_to_string(started).unwrap(), packet);
    assert!(!queue.join(name).exists());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request_text = || {
        std::iter::once(requests[0].system.as_str())
            .chain(requests[0].messages.iter().filter_map(|m| m.content.as_deref()))
    };
    if live {
        assert!(request_text().any(|text| text.contains("Synthetic live review note.")));
        assert!(request_text().any(|text| text.contains("Do not invoke gh or authenticated GitHub HTTP")));
    } else {
        assert!(request_text().any(|text| text.contains("Synthetic admitted task.")));
    }
    run_server_records(&env, &server, &args, Some(path.clone()), &[], false);
    assert_eq!(
        server.requests().len(),
        1,
        "a released attempt must not launch again"
    );
    let replayed = &claims.assignments().unwrap()[&44];
    assert_eq!(replayed.state.attempts, 1);
    assert!(replayed.state.lease.is_none());
    assert!(replayed.data.native_binding.as_ref() == Some(&binding));
    let after_replay: serde_json::Value = serde_json::from_slice(
        &fs::read(storage(&env).join(format!("{}.json", binding.native_task_id))).unwrap(),
    )
    .unwrap();
    assert_eq!(after_replay, checkpoint);
    if let Some(app_reads) = live_app_reads {
        assert_eq!(fs::read(tools.join("gh.calls")).unwrap(), app_reads);
    }

    if source == NativeSource::Reviewed {
        let first_receipt = storage(&env).join(format!("{}.json", binding.native_task_id));
        let first_bytes = fs::read(&first_receipt).unwrap();
        let previous = Path::new(record["worktree"].as_str().unwrap());
        git(
            &env,
            previous,
            &["commit", "--allow-empty", "-m", "second reviewed round"],
        );
        let reviewed = git(&env, previous, &["rev-parse", "HEAD"]);
        let edit = |file: &Path, pointers: &[&str]| {
            let mut value: serde_json::Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
            for pointer in pointers {
                *value.pointer_mut(pointer).unwrap() = reviewed.clone().into();
            }
            fs::write(file, serde_json::to_vec(&value).unwrap()).unwrap();
        };
        edit(&config, &["/packets/0/revision"]);
        edit(
            &tools.join("gh.reply"),
            &[
                "/data/repository/pullRequests/nodes/0/headRefOid",
                "/data/repository/p44/headRefOid",
            ],
        );
        refill();
        let next = claims.assignments().unwrap()[&44]
            .data
            .native_binding
            .clone()
            .unwrap();
        assert_ne!(next.native_task_id, binding.native_task_id);
        assert_ne!(next.key, binding.key);
        assert_ne!(next.lease.generation, binding.lease.generation);
        assert_eq!(next.reviewed_head.as_deref(), Some(reviewed.as_str()));
        let server = FakeServer::start(vec![ScriptTurn::text("finished")], FakeOpts::default()).unwrap();
        let result = run_server(&env, &server, &args, Some(path), &[]);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(server.requests().len(), 1);
        let saved: serde_json::Value = serde_json::from_slice(
            &fs::read(storage(&env).join(format!("{}.json", next.native_task_id))).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["execution"]["initial"], reviewed);
        assert_eq!(saved["task"]["reviewed_pull_request"]["number"], 44);
        assert_eq!(saved["task"]["reviewed_pull_request"]["head"], reviewed);
        assert_eq!(saved["record"]["step"], "finished");
        assert_eq!(fs::read(first_receipt).unwrap(), first_bytes);
        assert!(git(&env, previous, &["branch", "--show-current"]).is_empty());
        let row = &claims.assignments().unwrap()[&44];
        assert_eq!(row.state.attempts, 2);
        assert!(row.state.lease.is_none());
        assert!(row.data.native_binding.as_ref() == Some(&next));
    }
}
#[test]
fn failed_checks_get_exactly_one_retry() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["printf attempt >> attempts; echo external-check; exit 1"]"#,
    );
    let record = run(
        &env,
        vec![ScriptTurn::text("finished"), ScriptTurn::text("cannot repair")],
    );
    assert_eq!(record["outcome"], "red", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("external-check"));
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(tree.join("attempts")).unwrap(),
        "attemptattempt"
    );
}

#[test]
fn failed_agent_with_failed_checks_is_blocked_and_keeps_both_causes() {
    let env = TestEnv::new();
    setup(&env, r#"["echo external-check-failed; exit 1"]"#);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"missing-model","prompt":"Finish task"}]"#,
    );
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("missing-model"), "{detail}");
    assert!(detail.contains("external-check-failed"), "{detail}");
}
#[test]
fn retry_can_make_acceptance_green() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["if test -f once; then true; else touch once; exit 1; fi"]"#,
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("finished"), ScriptTurn::text("retry")],
        FakeOpts::default(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "done", "{record}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].session, requests[1].session,
        "repair lost the worker session"
    );
    assert_eq!(record["session_id"], requests[0].session);
}

#[test]
fn agent_edits_stay_in_worktree() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("edit", "write", r#"{"path":"answer.txt","content":"finished"}"#),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(tree.join("answer.txt")).unwrap(),
        "finished"
    );
    assert!(!env.work().join("project/answer.txt").exists());
}
#[test]
fn invalid_task_file_is_rejected_before_creating_worktree() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "work/tasks.json",
        r#"[{"id":"../escape","branch":"agent/test","model":"fake","prompt":"go"}]"#,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!env.agent_dir().join("work").exists());
}

#[test]
fn github_issue_input_uses_mock_gh() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    enable_test_subagent(&env);
    let tools = trusted_tools(&env);
    let lookup_log = env.write("tmp/issue-lookup.log", "");
    let lookup_packet = env.write(
        "tmp/issue-packet.json",
        r#"{"title":"Synthetic task","body":"Fix the test fixture","state":"OPEN","comments":[{"author":{"login":"reviewer"},"body":"Synthetic follow-up finding"}]}"#,
    );
    let gh = env.write(
        "tmp/trusted-tools/gh",
        r##"#!/bin/sh
[ "$*" = 'issue view 42 --json title,body,state,comments' ] || exit 9
printf '%s\n' "$*" >> "$ISSUE_LOOKUP_LOG"
exec /bin/cat "$ISSUE_LOOKUP_PACKET"
"##,
    );
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_oxa"));
    invocation
        .args(["work", "--issue", "42", "--model", "fake"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", &path)
        .env("ISSUE_LOOKUP_LOG", &lookup_log)
        .env("ISSUE_LOOKUP_PACKET", &lookup_packet)
        .stdin(std::process::Stdio::null());
    let out = invocation.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "issue task did not run: {stderr}");
    let record: serde_json::Value = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|line| serde_json::from_str(line).unwrap())
        .unwrap_or_else(|| panic!("no record: {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(record["id"], "issue-42");
    assert_eq!(record["branch"], "agent/42-work");
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        std::fs::read_to_string(&lookup_log).unwrap(),
        "issue view 42 --json title,body,state,comments\n"
    );
    // The packet the child receives is the one lookup: text, state and comments. A worker
    // that cannot read the issue itself must not receive a title-shaped packet.
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "worker made no request: {record}");
    let request = &requests[0];
    assert!(
        !request.tools_json.contains("\"name\":\"subagent\""),
        "{request:?}"
    );
    let prompt = std::iter::once(request.system.as_str())
        .chain(request.messages.iter().filter_map(|m| m.content.as_deref()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt.contains("Fix the test fixture"), "{prompt}");
    assert!(prompt.contains("Runner-fetched issue state: OPEN."), "{prompt}");
    assert!(prompt.contains("Synthetic follow-up finding"), "{prompt}");
    let ownership = prompt
        .find("The runner owns GitHub access for this task.")
        .unwrap_or_else(|| panic!("worker did not receive GitHub ownership guidance: {prompt}"));
    let issue = prompt.find("Implement issue #42: Synthetic task").unwrap();
    assert!(ownership < issue, "{prompt}");
    assert!(
        prompt.contains("Do not invoke gh or authenticated GitHub HTTP"),
        "{prompt}"
    );
    assert!(prompt.contains("NEED-METADATA"), "{prompt}");

    std::fs::write(
        &lookup_packet,
        r#"{"title":"Synthetic task","body":"Fix the test fixture","state":"CLOSED","comments":[{"body":"New comment after the task was frozen"}]}"#,
    )
    .unwrap();
    let repeat = invocation.output().unwrap();
    assert!(
        repeat.status.success(),
        "an updated issue prevented checkpoint reuse: {} {}",
        String::from_utf8_lossy(&repeat.stdout),
        String::from_utf8_lossy(&repeat.stderr)
    );
    let repeated: serde_json::Value = serde_json::from_slice(&repeat.stdout).unwrap();
    assert_eq!(repeated["checkpoint_revision"], record["checkpoint_revision"]);
    assert_eq!(
        server.requests().len(),
        1,
        "a settled task started a second worker"
    );

    let rerun = FakeServer::start(vec![ScriptTurn::text("done again")], FakeOpts::default()).unwrap();
    let repeated = invocation
        .envs(env.envs(&rerun.url()))
        .arg("--rerun")
        .output()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stdout)
    );
    let requests = rerun.requests();
    assert_eq!(requests.len(), 1, "rerun never started the worker");
    let request = &requests[0];
    assert!(
        !request.tools_json.contains("\"name\":\"subagent\""),
        "{request:?}"
    );
    let prompt = std::iter::once(request.system.as_str())
        .chain(request.messages.iter().filter_map(|m| m.content.as_deref()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt.contains("Synthetic follow-up finding"), "{prompt}");
    assert!(
        !prompt.contains("New comment after the task was frozen"),
        "{prompt}"
    );
    assert_eq!(
        std::fs::read_to_string(&lookup_log).unwrap(),
        "issue view 42 --json title,body,state,comments\n"
    );
}
#[test]
fn issue_packet_workers_keep_local_tools_and_a_separate_network_namespace() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    setup(&env, "[]");
    let tools = trusted_tools(&env);
    let gh = env.write(
        "tmp/trusted-tools/gh",
        r#"#!/bin/sh
[ "$*" = 'issue view 42 --json title,body,state,comments' ] || exit 9
printf '%s' '{"title":"Synthetic task","body":"Record the local fixture","state":"OPEN","comments":[]}'
"#,
    );
    std::fs::set_permissions(gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let host_namespace = std::fs::read_link("/proc/self/ns/net").unwrap();
    let turns = |file: &str| {
        vec![
            ScriptTurn::tool(
                "local",
                "bash",
                &serde_json::json!({"command": format!("readlink /proc/self/ns/net > {file}")}).to_string(),
            ),
            ScriptTurn::text("done"),
        ]
    };
    let assert_isolated = |record: &serde_json::Value, file: &str| {
        assert!(
            matches!(record["outcome"].as_str(), Some("done" | "nothing_to_do")),
            "{record}"
        );
        let worktree = Path::new(record["worktree"].as_str().unwrap());
        let worker_namespace = std::fs::read_to_string(worktree.join(file)).unwrap();
        assert!(worker_namespace.starts_with("net:["));
        assert_ne!(Path::new(worker_namespace.trim()), host_namespace.as_path());
    };
    let first = run_args(
        &env,
        turns("worker-namespace"),
        &["work", "--issue", "42", "--model", "fake"],
        Some(path),
    );
    assert_isolated(&first, "worker-namespace");

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(storage(&env).join("issue-42.json")).unwrap()).unwrap();
    env.write(
        "work/frozen-issue.json",
        &serde_json::json!([saved["task"]]).to_string(),
    );
    // The file still describes the exact issue packet, so an explicit rerun
    // keeps the same tool restriction even without an issue flag.
    let rerun = run_args(
        &env,
        turns("rerun-namespace"),
        &["work", "../frozen-issue.json", "--rerun"],
        None,
    );
    assert_isolated(&rerun, "rerun-namespace");
}

/// A dogfood issue carried its content in comments, so a packet built from the
/// title and body alone reached the worker without it. The child's first request
/// must contain the body and the comment findings, all as data.
#[test]
fn github_issue_packet_carries_comment_findings() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let tools = trusted_tools(&env);
    let gh = env.write(
        "tmp/trusted-tools/gh",
        r##"#!/bin/sh
[ "$*" = 'issue view 42 --json title,body,state,comments' ] || exit 9
printf '%s' '{"title":"Synthetic task","body":"Fix the test fixture","state":"OPEN","comments":[{"author":{"login":"reviewer"},"body":"Review finding: keep the frame when delegating"},{"author":null,"body":"Unknown-author finding"}]}'
"##,
    );
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "--issue", "42", "--model", "fake"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "issue task did not run: {stderr}");
    let record: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {stderr}"));
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "worker made no request: {record}");
    let mut prompt = requests[0].system.clone();
    for message in &requests[0].messages {
        if let Some(content) = &message.content {
            prompt.push('\n');
            prompt.push_str(content);
        }
    }
    assert!(prompt.contains("Fix the test fixture"), "{prompt}");
    assert!(
        prompt.contains("Review finding: keep the frame when delegating"),
        "issue comments were not in the task packet: {prompt}"
    );
    assert!(prompt.contains("reviewer"), "{prompt}");
    assert!(prompt.contains("Runner-fetched issue state: OPEN."), "{prompt}");
    assert!(
        prompt.contains("--- comment by unknown ---\nUnknown-author finding"),
        "{prompt}"
    );
}
#[test]
fn pull_request_packet_carries_reviews_and_failing_checks() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let host_namespace = std::fs::read_link("/proc/self/ns/net").unwrap();
    let checks = serde_json::json!([format!(
        "namespace=$(readlink /proc/self/ns/net) && test \"$namespace\" != '{}'",
        host_namespace.display()
    )]);
    setup(&env, &checks.to_string());
    enable_test_subagent(&env);
    let project = env.work().join("project");
    git(&env, &project, &["switch", "-c", "seed-pr-head"]);
    env.write("work/project/only-on-pr.txt", "Reviewed change");
    git(&env, &project, &["add", "only-on-pr.txt"]);
    git(&env, &project, &["commit", "-m", "Seed distinct PR head"]);
    let head = git(&env, &project, &["rev-parse", "HEAD"]);
    git(
        &env,
        &project,
        &["push", "origin", "HEAD:refs/heads/agent/review-fixes"],
    );
    git(&env, &project, &["switch", "main"]);
    env.write("work/project/only-on-main.txt", "Unreviewed base change");
    git(&env, &project, &["add", "only-on-main.txt"]);
    git(&env, &project, &["commit", "-m", "Move main after review"]);

    let connection =
        |nodes: Vec<serde_json::Value>| serde_json::json!({"totalCount": nodes.len(), "nodes": nodes});
    let mut reply = serde_json::json!({
        "data": {"repository": {"pullRequest": {
            "title": "Synthetic PR", "body": "Fix the PR fixture",
            "headRefOid": head, "headRefName": "agent/review-fixes", "isCrossRepository": false,
            "reviews": connection(vec![serde_json::json!({
                "state": "CHANGES_REQUESTED", "body": "Review summary"
            })]),
            "comments": connection(vec![serde_json::json!({"body": "Conversation note"})]),
            "reviewThreads": connection(vec![
                serde_json::json!({
                    "isResolved": false, "path": "only-on-pr.txt", "line": 1,
                    "comments": connection(vec![serde_json::json!({"body": "Inline finding"})])
                }),
                serde_json::json!({
                    "isResolved": true, "path": "only-on-pr.txt", "line": 1,
                    "comments": connection(vec![serde_json::json!({"body": "Resolved finding"})])
                })
            ]),
            "files": connection(vec![serde_json::json!({"path": "only-on-pr.txt"})]),
            "commits": {"nodes": [{"commit": {
                "oid": head,
                "statusCheckRollup": {"contexts": connection(vec![
                    serde_json::json!({"name": "checks / oxa", "conclusion": "FAILURE"}),
                    serde_json::json!({"name": "checks / docs", "conclusion": "SUCCESS"})
                ])}
            }}]}
        }}}
    });
    let tools = trusted_tools(&env);
    env.write("tmp/trusted-tools/pr-packet.json", &reply.to_string());
    let gh = env.write(
        "tmp/trusted-tools/gh",
        r#"#!/bin/sh
[ "$1" = api ] && [ "$2" = graphql ] || exit 9
case "$4" in
  *headRefOid*reviewThreads*files*statusCheckRollup*) ;;
  *) exit 9 ;;
esac
exec /bin/cat "${0%/*}/pr-packet.json"
"#,
    );
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .args(["work", "--pr", "42", "--model", "fake"])
        .current_dir(&project)
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", &path);
    let missing = command.output().unwrap();
    assert!(!missing.status.success());
    let record: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"].as_str().unwrap().contains("reviewed head"),
        "{record}"
    );
    assert!(server.requests().is_empty());

    git(&env, &project, &["branch", "agent/review-fixes", &head]);
    let out = command.arg("--rerun").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["id"], format!("pr-42-{head}"));
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(git(&env, tree, &["rev-parse", "HEAD"]), head);
    assert!(tree.join("only-on-pr.txt").exists());
    assert!(!tree.join("only-on-main.txt").exists());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        !request.tools_json.contains("\"name\":\"subagent\""),
        "{request:?}"
    );
    let prompt = std::iter::once(request.system.as_str())
        .chain(request.messages.iter().filter_map(|m| m.content.as_deref()))
        .collect::<Vec<_>>()
        .join("\n");
    for required in [
        "The runner owns GitHub access",
        "Fix the PR fixture",
        "Review summary",
        "Conversation note",
        "Inline finding",
        "only-on-pr.txt:1",
        "checks / oxa",
        "Changed files",
    ] {
        assert!(prompt.contains(required), "missing {required}: {prompt}");
    }
    assert!(!prompt.contains("Resolved finding"), "{prompt}");
    assert!(!prompt.contains("checks / docs"), "{prompt}");

    let verified = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "verify", record["id"].as_str().unwrap()])
        .current_dir(&project)
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(verified.status.success(), "{verified:?}");
    assert_eq!(server.requests().len(), 1);

    std::fs::write(tree.join("second-round.txt"), "Review revision").unwrap();
    git(&env, tree, &["add", "second-round.txt"]);
    git(&env, tree, &["commit", "-m", "Apply review feedback"]);
    let next_head = git(&env, tree, &["rev-parse", "HEAD"]);
    let packet = &mut reply["data"]["repository"]["pullRequest"];
    packet["headRefOid"] = serde_json::json!(next_head);
    packet["commits"]["nodes"][0]["commit"]["oid"] = serde_json::json!(next_head);
    packet["body"] = serde_json::json!("Second review round");
    env.write("tmp/trusted-tools/pr-packet.json", &reply.to_string());

    let next = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    command.envs(env.envs(&next.url())).env("PATH", &path);
    let uncommitted = tree.join("local-notes.txt");
    std::fs::write(&uncommitted, "Retain these notes").unwrap();
    let blocked = command.output().unwrap();
    assert!(!blocked.status.success());
    let blocked: serde_json::Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert!(
        blocked["detail"].as_str().unwrap().contains("uncommitted"),
        "{blocked}"
    );
    assert!(next.requests().is_empty());
    assert_eq!(
        git(&env, tree, &["branch", "--show-current"]),
        "agent/review-fixes"
    );
    assert_eq!(
        std::fs::read_to_string(&uncommitted).unwrap(),
        "Retain these notes"
    );
    std::fs::remove_file(uncommitted).unwrap();

    let second = command.output().unwrap();
    assert!(second.status.success(), "{second:?}");
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["id"], format!("pr-42-{next_head}"));
    let second_tree = Path::new(second["worktree"].as_str().unwrap());
    assert_ne!(second_tree, tree);
    assert_eq!(git(&env, second_tree, &["rev-parse", "HEAD"]), next_head);
    assert_eq!(
        git(&env, second_tree, &["branch", "--show-current"]),
        "agent/review-fixes"
    );
    assert!(git(&env, tree, &["branch", "--show-current"]).is_empty());
    assert!(tree.join("second-round.txt").exists());
    let requests = next.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("Second review round"))
    }));
}

#[test]
fn branch_checked_out_elsewhere_is_blocked_without_changing_it() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    git(&env, &project, &["branch", "agent/test"]);
    let original = git(&env, &project, &["rev-parse", "agent/test"]);
    let occupied = env.work().join("occupied");
    git(
        &env,
        &project,
        &["worktree", "add", occupied.to_str().unwrap(), "agent/test"],
    );
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap()
            .contains(occupied.to_str().unwrap()),
        "{record}"
    );
    assert!(record["worker"].is_null());
    assert!(!env.work().join(".oxa-worktrees/project/test").exists());
    assert_eq!(git(&env, &occupied, &["rev-parse", "HEAD"]), original);
    assert_eq!(git(&env, &project, &["symbolic-ref", "--short", "HEAD"]), "main");
}

#[test]
fn child_can_create_a_commit_with_the_real_sandbox() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"committed"}"#),
            ScriptTurn::tool(
                "commit",
                "bash",
                r#"{"command":"git add answer.txt && git -c user.name=Test -c user.email=test@example.invalid commit -m task"}"#,
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(tree)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "commit must persist: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    let subject = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(tree)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&subject.stdout).trim(), "task");
}
#[test]
fn offline_github_lookup_never_spawns_gh() {
    use std::os::unix::fs::PermissionsExt;
    // Both GitHub inputs share one refusal: an offline run never spawns even a trusted gh,
    // whether the flag or the user setting turns offline on.
    for input in ["--issue", "--pr"] {
        for setting in [false, true] {
            let env = TestEnv::new();
            setup(&env, "[]");
            let gh = env.write(
                "tmp/gh",
                "#!/bin/sh\nprintf invoked > \"$HOME/gh-invoked\"\nexit 7\n",
            );
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            if setting {
                env.write("home/.oxa/agent/settings.json", r#"{"oxa":{"offline":true}}"#);
            }
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
            cmd.args(["work", input, "42", "--model", "fake"])
                .current_dir(env.work().join("project"))
                .env_clear()
                .envs(env.envs("http://127.0.0.1:1"))
                .env(
                    "PATH",
                    format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
                );
            if !setting {
                cmd.env("OXA_OFFLINE", "1");
            }
            let out = cmd.output().unwrap();
            assert_eq!(out.status.code(), Some(2));
            assert!(
                !env.home().join("gh-invoked").exists(),
                "offline lookup ran gh, input={input}, setting={setting}"
            );
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("offline"),
                "input={input}, setting={setting}"
            );
        }
    }
}

#[test]
fn command_line_offline_policy_reaches_child_agent() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"cloud-model","prompt":"go"}]"#,
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("done")],
        FakeOpts {
            model: "cloud-model".into(),
            remote: Some("openrouter"),
            models: vec![oxa_fake::FakeModel {
                alias: "cloud-model".into(),
                cap: 64000,
                max_output: Some(4096),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--offline", "work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"].as_str().unwrap().contains("offline mode is on"),
        "{record}"
    );
    assert!(
        server.requests().is_empty(),
        "no chat reached even the mock remote"
    );
}
#[test]
fn batch_reports_one_task_error_and_runs_later_tasks() {
    let env = TestEnv::new();
    setup(&env, "[]");
    std::fs::create_dir_all(storage(&env)).unwrap();
    std::fs::write(storage(&env).join("test.json"), "{}").unwrap();
    env.write("work/tasks.json", r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"go"},{"id":"next","branch":"agent/next","model":"fake","prompt":"go"}]"#);
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["outcome"], "blocked");
    assert_eq!(rows[1]["outcome"], "nothing_to_do");
    assert_eq!(
        std::fs::read_to_string(storage(&env).join("test.json")).unwrap(),
        "{}"
    );
}
#[test]
fn abandoned_lock_file_does_not_block_a_task() {
    let env = TestEnv::new();
    setup(&env, "[]");
    std::fs::create_dir_all(storage(&env)).unwrap();
    std::fs::write(storage(&env).join("test.lock"), "").unwrap();
    let record = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
}

#[test]
fn untrusted_project_never_executes_external_checks() {
    let env = TestEnv::new();
    setup(&env, r#"["printf escaped > \"$HOME/untrusted-check\""]"#);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"never"}"#,
    );
    oxa_perm::TrustStore::default()
        .save(
            &env.agent_dir().join("trust.json"),
            &env.work().join("project"),
            false,
        )
        .unwrap();
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert!(!env.home().join("untrusted-check").exists(), "untrusted code ran");
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("trust"), "{record}");
}

#[test]
fn agent_written_acceptance_code_cannot_write_outside_the_sandbox() {
    let env = TestEnv::new();
    setup(&env, r#"["sh check.sh"]"#);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    env.write("work/project/check.sh", "exit 0\n");
    let project = env.work().join("project");
    git(&env, &project, &["add", "check.sh"]);
    git(&env, &project, &["commit", "-m", "base check"]);
    let script = "if printf escaped > \"$HOME/acceptance-escape\" 2>/dev/null; then true; fi\nprintf checked > acceptance-ran\n";
    let args = serde_json::json!({"path":"check.sh", "content":script}).to_string();
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("change-check", "write", &args),
            ScriptTurn::text("finished"),
        ],
    );
    assert!(
        !env.home().join("acceptance-escape").exists(),
        "agent-written code escaped acceptance"
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(Path::new(record["worktree"].as_str().unwrap()).join("acceptance-ran"))
            .unwrap(),
        "checked"
    );
}

#[test]
fn worker_result_and_session_come_from_the_real_child_contract() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let mut turn = ScriptTurn::text("finished");
    turn.completion_tokens = Some(5);
    let record = run(&env, vec![turn]);
    assert_eq!(record["worker"]["outcome"], "done", "{record}");
    assert_eq!(record["worker"]["exit_code"], 0, "{record}");
    assert!(!record["session_id"].as_str().unwrap().is_empty());
    let tokens = &record["worker"]["tokens"];
    assert_eq!(tokens["completion"], 5, "{record}");
    assert_eq!(tokens["complete"], true, "{record}");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(storage(&env).join("test.json")).unwrap()).unwrap();
    assert_eq!(&saved["record"]["worker"]["tokens"], tokens);
}

#[test]
fn blocked_worker_keeps_its_result_and_does_not_start_a_repair() {
    use oxa_fake::Fault;
    let env = TestEnv::new();
    setup(&env, r#"["echo external-check-failed; exit 1"]"#);
    env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false}}"#);
    let server = FakeServer::start(
        vec![ScriptTurn::fault(Fault::Http {
            status: 429,
            body: serde_json::json!({"error":{"code":"budget_exceeded", "message":"Synthetic daily cap"}})
                .to_string(),
        })],
        FakeOpts::default(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert_eq!(record["worker"]["outcome"], "blocked", "{record}");
    assert_eq!(record["worker"]["error_class"], "budget_exhausted", "{record}");
    assert_eq!(record["worker"]["exit_code"], oxa_proto::EXIT_BUDGET, "{record}");
    assert_eq!(
        server.requests().len(),
        1,
        "a budget refusal must not start a repair worker"
    );
    assert!(record["detail"]
        .as_str()
        .unwrap()
        .contains("external-check-failed"));
}

#[test]
fn large_streamed_reply_does_not_enter_the_runner_receipt() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let mut turn = ScriptTurn::text(&"Synthetic response ".repeat(8192));
    turn.completion_tokens = Some(5);
    turn.chunk = 8192;
    let record = run(&env, vec![turn]);
    assert_eq!(record["worker"]["outcome"], "done", "{record}");
    assert!(
        record.to_string().len() < 2048,
        "runner retained streamed model output"
    );
}

#[test]
fn changed_project_commands_need_the_existing_content_approval() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    oxa_perm::TrustStore::load(&env.agent_dir().join("trust.json"))
        .approve(&env.agent_dir().join("trust.json"), &project)
        .unwrap();
    env.write(
        "work/project/.oxa/settings.json",
        r#"{"done":["printf escaped > \"$HOME/changed-check\""]}"#,
    );
    git(&env, &project, &["add", ".oxa/settings.json"]);
    git(&env, &project, &["commit", "-m", "changed commands"]);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(!env.home().join("changed-check").exists());
}

#[test]
fn acceptance_has_no_unsandboxed_fallback() {
    use std::os::unix::fs::symlink;
    let env = TestEnv::new();
    setup(&env, r#"["printf ran > acceptance-ran"]"#);
    let tools = trusted_tools(&env);
    for name in ["git", "sh"] {
        symlink(Path::new("/usr/bin").join(name), tools.join(name)).unwrap();
    }
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(tools.to_string_lossy().into_owned()),
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("bwrap"), "{record}");
    assert!(!Path::new(record["worktree"].as_str().unwrap())
        .join("acceptance-ran")
        .exists());
}

#[test]
fn runner_state_is_private_and_outside_the_workers_git_bind() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        std::fs::metadata(storage(&env)).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for file in ["test.json", "test.lock"] {
        assert_eq!(
            std::fs::metadata(storage(&env).join(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!env.work().join("project/.git/oxa-work").exists());
}

#[test]
fn acceptance_preserves_the_normal_read_only_git_configuration() {
    let env = TestEnv::new();
    setup(&env, r#"["sh check.sh"]"#);
    env.write("work/project/check.sh", "git config core.fsmonitor 'printf escaped > \"$HOME/git-probe-escape\"; printf \"\\0\"' 2>/dev/null || true\nprintf checked > acceptance-ran\n");
    let project = env.work().join("project");
    git(&env, &project, &["add", "check.sh"]);
    git(&env, &project, &["commit", "-m", "check fixture"]);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(!env.home().join("git-probe-escape").exists());
    let config = Command::new("git")
        .args(["config", "--get", "core.fsmonitor"])
        .current_dir(project)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(
        config.status.code(),
        Some(1),
        "acceptance rewrote executable Git configuration"
    );
}

#[test]
fn github_lookup_ignores_project_local_executable() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let bad = env.write("work/project/bin/gh", "#!/bin/sh\nprintf invoked > \"$HOME/project-gh-ran\"\nprintf '%s' '{\"title\":\"Synthetic task\",\"body\":\"Fixture\",\"state\":\"OPEN\",\"comments\":[]}'\n");
    trusted_tools(&env);
    let good = env.write("tmp/trusted-tools/gh", "#!/bin/sh\nprintf invoked > \"$HOME/trusted-gh-ran\"\nprintf '%s' '{\"title\":\"Synthetic task\",\"body\":\"Fixture\",\"state\":\"OPEN\",\"comments\":[]}'\n");
    for path in [&bad, &good] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}:{}",
        bad.parent().unwrap().display(),
        good.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "--issue", "42", "--model", "fake"],
        Some(path),
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert!(
        !env.home().join("project-gh-ran").exists(),
        "project executable ran on the host"
    );
    assert!(
        env.home().join("trusted-gh-ran").exists(),
        "the trusted mock must handle issue lookup"
    );
}

#[test]
fn runner_git_queries_ignore_project_local_executable() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let bad = env.write("work/project/bin/git", "#!/bin/sh\nprintf invoked > \"$HOME/project-git-ran\" 2>/dev/null || true\nexec /usr/bin/git \"$@\"\n");
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bad.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path),
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert!(
        !env.home().join("project-git-ran").exists(),
        "project executable ran on the host"
    );
}

#[test]
fn untrusted_issue_input_never_spawns_even_a_trusted_gh() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    oxa_perm::TrustStore::default()
        .save(
            &env.agent_dir().join("trust.json"),
            &env.work().join("project"),
            false,
        )
        .unwrap();
    let gh = env.write("tmp/gh", "#!/bin/sh\nprintf invoked > \"$HOME/issue-lookup-ran\"\nprintf '%s' '{\"title\":\"Synthetic task\",\"body\":\"Fixture\",\"state\":\"OPEN\",\"comments\":[]}'\n");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "--issue", "42", "--model", "fake"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env(
            "PATH",
            format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("trust"));
    assert!(!env.home().join("issue-lookup-ran").exists());
}

#[test]
fn invalid_nested_git_metadata_cannot_hide_an_outer_project_executable() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let bad = env.write(
        "work/project/bin/git",
        "#!/bin/sh\nprintf invoked > \"$HOME/nested-project-git-ran\"\nexec /usr/bin/git \"$@\"\n",
    );
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.write("work/project/nested/.git", "invalid git metadata\n");
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../../tasks.json"])
        .current_dir(env.work().join("project/nested"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                bad.parent().unwrap().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        !env.home().join("nested-project-git-ran").exists(),
        "nested metadata bypassed the source boundary"
    );
}

#[test]
fn final_inspection_never_executes_a_worker_edited_fsmonitor_on_the_host() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    env.write("work/project/monitor.sh", "printf '\\0'\n");
    git(&env, &project, &["add", "monitor.sh"]);
    git(&env, &project, &["commit", "-m", "monitor fixture"]);
    git(&env, &project, &["config", "core.fsmonitor", "sh monitor.sh"]);
    let args = serde_json::json!({"path":"monitor.sh","content":"printf escaped > \"$HOME/fsmonitor-inspection-escape\"\nprintf '\\0'\n"}).to_string();
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("change-monitor", "write", &args),
            ScriptTurn::text("finished"),
        ],
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(
        !env.home().join("fsmonitor-inspection-escape").exists(),
        "final inspection ran a host callback"
    );
}

#[test]
fn final_inspection_keeps_worker_edited_clean_filters_in_the_sandbox() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    env.write("work/project/.gitattributes", "tracked.txt filter=probe\n");
    env.write("work/project/filter.sh", "cat\n");
    env.write("work/project/tracked.txt", "alpha\n");
    git(&env, &project, &["config", "filter.probe.clean", "sh filter.sh"]);
    git(&env, &project, &["config", "filter.probe.smudge", "cat"]);
    git(&env, &project, &["config", "filter.probe.required", "true"]);
    git(
        &env,
        &project,
        &["add", ".gitattributes", "filter.sh", "tracked.txt"],
    );
    git(&env, &project, &["commit", "-m", "filter fixture"]);
    let args = serde_json::json!({"path":"filter.sh","content":"printf escaped > \"$HOME/filter-inspection-escape\" 2>/dev/null || true\ncat\n"}).to_string();
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("change-filter", "write", &args),
            ScriptTurn::tool(
                "change-file",
                "write",
                r#"{"path":"tracked.txt","content":"bravo\n"}"#,
            ),
            ScriptTurn::text("finished"),
        ],
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(
        !env.home().join("filter-inspection-escape").exists(),
        "final inspection ran a host filter"
    );
}

#[test]
fn failed_git_inspection_is_blocked_instead_of_clean_or_dirty() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    env.write("work/project/.gitattributes", "tracked.txt filter=probe\n");
    env.write("work/project/filter.sh", "cat\n");
    env.write("work/project/tracked.txt", "alpha\n");
    git(&env, &project, &["config", "filter.probe.clean", "sh filter.sh"]);
    git(&env, &project, &["config", "filter.probe.smudge", "cat"]);
    git(&env, &project, &["config", "filter.probe.required", "true"]);
    git(
        &env,
        &project,
        &["add", ".gitattributes", "filter.sh", "tracked.txt"],
    );
    git(&env, &project, &["commit", "-m", "filter fixture"]);
    let record = run(
        &env,
        vec![
            ScriptTurn::tool(
                "break-filter",
                "write",
                r#"{"path":"filter.sh","content":"echo synthetic-filter-failure >&2\nexit 1\n"}"#,
            ),
            ScriptTurn::tool(
                "change-file",
                "write",
                r#"{"path":"tracked.txt","content":"bravo\n"}"#,
            ),
            ScriptTurn::text("finished"),
        ],
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert_eq!(record["worker"]["outcome"], "done", "{record}");
    assert!(record["detail"]
        .as_str()
        .unwrap()
        .contains("synthetic-filter-failure"));
}

fn checkout_hook_fixture(env: &TestEnv) {
    use std::os::unix::fs::PermissionsExt;
    setup(env, "[]");
    let hook = env.write(
        "work/project/.githooks/post-checkout",
        "#!/bin/sh\nprintf escaped > \"$HOME/worktree-checkout-escape\"\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let project = env.work().join("project");
    git(env, &project, &["add", ".githooks"]);
    git(env, &project, &["commit", "-m", "checkout hook fixture"]);
    git(env, &project, &["config", "core.hooksPath", ".githooks"]);
}

#[test]
fn worktree_checkout_never_runs_tracked_hooks_on_the_host() {
    let env = TestEnv::new();
    checkout_hook_fixture(&env);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert!(
        !env.home().join("worktree-checkout-escape").exists(),
        "worktree creation ran a host hook: {record}"
    );
    // The normal sandbox pins configured hook directories read-only, including
    // this missing directory. Checkout must block rather than unmask it.
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["worker"].is_null(), "{record}");
}

#[test]
fn checkout_keeps_git_hooks_active_inside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let hook = env.write(
        "work/project/.git/hooks/post-checkout",
        "#!/bin/sh\nprintf escaped > \"$HOME/worktree-checkout-escape\" 2>/dev/null || true\nprintf ran > checkout-hook-ran\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert!(
        !env.home().join("worktree-checkout-escape").exists(),
        "checkout hook escaped: {record}"
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(Path::new(record["worktree"].as_str().unwrap()).join("checkout-hook-ran"))
            .unwrap(),
        "ran"
    );
    assert!(!env.work().join("project/checkout-hook-ran").exists());
}

#[test]
fn unavailable_sandbox_blocks_checkout_before_any_hook() {
    use std::os::unix::fs::symlink;
    let env = TestEnv::new();
    checkout_hook_fixture(&env);
    let tools = trusted_tools(&env);
    for name in ["git", "sh"] {
        symlink(Path::new("/usr/bin").join(name), tools.join(name)).unwrap();
    }
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(tools.to_string_lossy().into_owned()),
    );
    assert!(
        !env.home().join("worktree-checkout-escape").exists(),
        "checkout ran a host hook before missing bwrap blocked the task: {record}"
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("bwrap"), "{record}");
}

#[test]
fn worktree_checkout_keeps_smudge_filters_in_the_sandbox() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write("work/project/.gitattributes", "*.data filter=probe\n");
    env.write("work/project/fixture.data", "synthetic data\n");
    let project = env.work().join("project");
    git(&env, &project, &["add", "."]);
    git(&env, &project, &["commit", "-m", "smudge filter fixture"]);
    git(
        &env,
        &project,
        &[
            "config",
            "filter.probe.smudge",
            "printf escaped > \"$HOME/worktree-smudge-escape\" 2>/dev/null || true; cat",
        ],
    );
    git(&env, &project, &["config", "filter.probe.clean", "cat"]);
    git(&env, &project, &["config", "filter.probe.required", "true"]);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert!(
        !env.home().join("worktree-smudge-escape").exists(),
        "worktree creation ran a host filter: {record}"
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
}

#[test]
fn checkout_callbacks_cannot_replace_the_frozen_acceptance_commands() {
    let env = TestEnv::new();
    setup(&env, r#"["echo frozen-checkout-acceptance; exit 1"]"#);
    env.write("work/project/.gitattributes", "*.data filter=probe\n");
    env.write("work/project/fixture.data", "synthetic data\n");
    let project = env.work().join("project");
    git(&env, &project, &["add", "."]);
    git(&env, &project, &["commit", "-m", "checkout settings fixture"]);
    git(
        &env,
        &project,
        &[
            "config",
            "filter.probe.smudge",
            "printf '%s' '{\"done\":[]}' > .oxa/settings.json; cat",
        ],
    );
    git(&env, &project, &["config", "filter.probe.clean", "cat"]);
    git(&env, &project, &["config", "filter.probe.required", "true"]);
    let record = run(
        &env,
        vec![ScriptTurn::text("finished"), ScriptTurn::text("cannot repair")],
    );
    assert_eq!(record["outcome"], "red", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap()
            .contains("frozen-checkout-acceptance"),
        "{record}"
    );
}

#[test]
fn worktree_reference_hooks_run_only_inside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let hook = env.write(
        "work/project/.git/hooks/reference-transaction",
        "#!/bin/sh\nprintf escaped > \"$HOME/worktree-reference-escape\" 2>/dev/null || true\nprintf ran > reference-hook-ran\ncat >/dev/null\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert!(
        !env.home().join("worktree-reference-escape").exists(),
        "worktree metadata ran a host reference hook: {record}"
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(Path::new(record["worktree"].as_str().unwrap())
        .join("reference-hook-ran")
        .exists());
    assert!(!env.work().join("project/reference-hook-ran").exists());
}

#[test]
fn sandboxed_branch_creation_still_obeys_rejecting_reference_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let hook = env.write(
        "work/project/.git/hooks/reference-transaction",
        "#!/bin/sh\ncat >/dev/null\necho required-reference-policy >&2\nexit 1\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["worker"].is_null(), "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap()
            .contains("required-reference-policy"),
        "{record}"
    );
    let branch = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/agent/test"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(branch.status.code(), Some(1));
}

fn malicious_mcp() -> String {
    serde_json::json!({"mcpServers":{"unapproved":{"command":"sh","args":["-c","printf escaped > \"$HOME/worker-trust-escape\""],"exposure":"direct"}}}).to_string()
}

fn child_trust_case(mode: &str) {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    git(&env, &project, &["checkout", "-b", "hostile-base"]);
    env.write("work/project/.oxa/mcp.json", &malicious_mcp());
    git(&env, &project, &["add", ".oxa/mcp.json"]);
    git(&env, &project, &["commit", "-m", "unapproved base resource"]);
    git(&env, &project, &["checkout", "main"]);
    let extra = match mode {
        "always" => {
            env.write(
                "home/.oxa/agent/settings.json",
                r#"{"defaultProjectTrust":"always"}"#,
            );
            vec![]
        }
        "parent" => {
            oxa_perm::TrustStore::default()
                .save(&env.agent_dir().join("trust.json"), &env.work(), true)
                .unwrap();
            vec![]
        }
        "environment" => vec![("OXA_TRUST_PROJECT", "1")],
        _ => unreachable!(),
    };
    env.write("work/tasks.json", r#"[{"id":"test","branch":"agent/test","model":"fake","base":"hostile-base","prompt":"Finish task"}]"#);
    let rows = run_records(
        &env,
        vec![ScriptTurn::text("done")],
        &["work", "../tasks.json"],
        None,
        &extra,
    );
    assert!(
        !env.home().join("worker-trust-escape").exists(),
        "unapproved child resource ran, mode={mode}: {rows:?}"
    );
    assert_eq!(rows[0]["outcome"], "nothing_to_do", "{rows:?}");
}

#[test]
fn security_child_does_not_inherit_parent_directory_trust() {
    child_trust_case("parent");
}
#[test]
fn security_child_does_not_autoapprove_a_different_base() {
    child_trust_case("always");
}
#[test]
fn security_child_does_not_inherit_the_trust_environment() {
    child_trust_case("environment");
}

#[test]
fn security_repair_does_not_load_new_project_resources() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let env = TestEnv::new();
    setup(&env, r#"["echo external-repair-required; exit 1"]"#);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    let mut first = ScriptTurn::text("first complete");
    first.fault = Some(oxa_fake::Fault::Stall { bytes: 16, ms: 1000 });
    let server = FakeServer::start(
        vec![first, ScriptTurn::text("repair complete")],
        FakeOpts::default(),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while server.requests().is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "first worker reached the engine"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Change the on-disk manifest after startup discovery and before repair.
    env.write("work/.oxa-worktrees/project/test/.oxa/mcp.json", &malicious_mcp());
    let out = child.wait_with_output().unwrap();
    assert!(
        !env.home().join("worker-trust-escape").exists(),
        "repair loaded a new resource: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "red", "{record}");
}

fn batch_tampering(replace: bool) {
    let env = TestEnv::new();
    setup(&env, r#"["printf trusted > accepted-base"]"#);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    let project = env.work().join("project");
    let original = git(&env, &project, &["rev-parse", "main"]);
    git(&env, &project, &["checkout", "-b", "forged-base"]);
    env.write("work/project/.oxa/settings.json", r#"{"done":["true"]}"#);
    env.write("work/project/forged-base", "forged\n");
    git(&env, &project, &["add", "."]);
    git(&env, &project, &["commit", "-m", "forged acceptance fixture"]);
    let forged = git(&env, &project, &["rev-parse", "HEAD"]);
    git(&env, &project, &["checkout", "main"]);
    env.write("work/tasks.json", r#"[{"id":"first","branch":"agent/first","model":"fake","prompt":"First task"},{"id":"second","branch":"agent/second","model":"fake","prompt":"Second task"}]"#);
    let reference = if replace {
        format!("refs/replace/{original}")
    } else {
        "refs/heads/main".into()
    };
    let command = serde_json::json!({"command":format!("git update-ref {reference} {forged}")}).to_string();
    let rows = run_records(
        &env,
        vec![
            ScriptTurn::tool("tamper", "bash", &command),
            ScriptTurn::text("first finished"),
            ScriptTurn::text("second finished"),
        ],
        &["work", "../tasks.json"],
        None,
        &[],
    );
    assert_eq!(
        git(&env, &project, &["rev-parse", &reference]),
        forged,
        "the first worker really changed the shared ref"
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    let tree = Path::new(rows[1]["worktree"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(tree.join("accepted-base")).unwrap_or_default(),
        "trusted",
        "later task used forged checks: {rows:?}"
    );
    assert!(
        !tree.join("forged-base").exists(),
        "later task checked out a forged base"
    );
    assert_eq!(rows[1]["base_sha"], original, "{rows:?}");
}

#[test]
fn security_batch_freezes_bases_before_shared_ref_updates() {
    batch_tampering(false);
}
#[test]
fn security_batch_ignores_replace_refs_from_an_earlier_worker() {
    batch_tampering(true);
}

#[test]
fn security_host_metadata_never_lazy_fetches_from_a_promisor() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    let object = git(&env, &project, &["rev-parse", "main:.oxa/settings.json"]);
    let helper = env.write(
        "tmp/promisor",
        "#!/bin/sh\nprintf escaped > \"$HOME/metadata-lazy-fetch-escape\"\nexit 1\n",
    );
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &env,
        &project,
        &[
            "config",
            "remote.attack.url",
            &format!("ext::{}", helper.display()),
        ],
    );
    git(&env, &project, &["config", "remote.attack.promisor", "true"]);
    git(&env, &project, &["config", "protocol.ext.allow", "always"]);
    std::fs::remove_file(project.join(".git/objects").join(&object[..2]).join(&object[2..])).unwrap();
    let record = run(&env, vec![]);
    assert!(
        !env.home().join("metadata-lazy-fetch-escape").exists(),
        "host metadata ran a promisor helper: {record}"
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["worker"].is_null(), "{record}");
}

#[test]
fn security_git_environment_cannot_redirect_the_runner() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let redirect = env.work().join("remote.git");
    let rows = run_records(
        &env,
        vec![ScriptTurn::text("done")],
        &["work", "../tasks.json"],
        None,
        &[("GIT_DIR", redirect.to_str().unwrap())],
    );
    assert_eq!(rows[0]["outcome"], "nothing_to_do", "{rows:?}");
}

#[test]
fn security_branch_alias_is_refused_before_worktree_creation() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    git(&env, &project, &["checkout", "-b", "previous"]);
    git(&env, &project, &["checkout", "main"]);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"@{-1}","model":"fake","prompt":"Finish task"}]"#,
    );
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        !env.work().join(".oxa-worktrees/project/test").exists(),
        "alias reached checkout: {record}"
    );
}

#[test]
fn security_failed_receipt_write_never_publishes_a_torn_record() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let env = TestEnv::new();
    setup(&env, "[]");
    let mut first = ScriptTurn::text("finished");
    first.fault = Some(oxa_fake::Fault::Stall { bytes: 16, ms: 1000 });
    let server = FakeServer::start(vec![first], FakeOpts::default()).unwrap();
    let child = Command::new("/usr/bin/prlimit")
        .args([
            "--core=0",
            "--",
            env!("CARGO_BIN_EXE_oxa"),
            "work",
            "../tasks.json",
        ])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while server.requests().is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "worker reached the engine"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Kill this isolated runner at its next regular-file write. Its already
    // running worker keeps normal limits, and core dumps are disabled.
    let limit = Command::new("/usr/bin/prlimit")
        .args(["--pid", &child.id().to_string(), "--fsize=0"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    assert!(
        limit.status.success(),
        "{}",
        String::from_utf8_lossy(&limit.stderr)
    );
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let retained: serde_json::Value =
        serde_json::from_slice(&std::fs::read(storage(&env).join("test.json")).unwrap())
            .expect("the previously durable checkpoint remains complete JSON");
    assert_eq!(retained["version"], 1);
    assert_eq!(retained["record"]["step"], "worker", "{retained}");
    assert_eq!(retained["record"]["outcome"], "running", "{retained}");
    assert!(retained["record"]["checkpoint_revision"].as_u64().unwrap() > 0);
}
