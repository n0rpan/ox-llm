use oxa_config::Dirs;
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use oxa_session::{FsStore, Goal, SessionStore};
use oxa_work::board::{Action, Board, Entry, Lease};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

/// The synthetic lead-panel record of one sample: a verified main merge at
/// `merged_seconds_before`, with main itself green.
fn panels_record(observed_at: u64, merged_seconds_before: u64) -> serde_json::Value {
    let merged = (observed_at.saturating_sub(merged_seconds_before)) * 1000;
    let merged = format!("{}Z", &oxa_session::iso_from_ms(merged)[..19]);
    let head = "c".repeat(40);
    json!({
        "id":"example-repo",
        "main":{
            "refUpdateRule":{"requiredStatusCheckContexts":["gate"]},
            "rules":{"totalCount":0,"nodes":[]},
            "target":{
                "oid":head,
                "statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},
                    "nodes":[{"name":"gate","status":"COMPLETED","conclusion":"SUCCESS"}]}},
                "associatedPullRequests":{"pageInfo":{"hasNextPage":false},
                    "nodes":[{"repository":{"id":"example-repo"},"baseRefName":"main",
                        "mergeCommit":{"oid":head},"mergedAt":merged}]}
            }
        },
        "mergeQueue":null
    })
}

fn write_sample(
    path: &Path,
    observed_at: u64,
    rows: &[(u64, Action, bool, Option<u64>, Option<&str>)],
    panels: Option<serde_json::Value>,
) {
    let board = Board {
        observed_at,
        entries: rows
            .iter()
            .map(|&(number, action, eligible, expires_at, holder)| Entry {
                number,
                head: "a".repeat(40),
                release_pinned: false,
                action,
                lease: expires_at.map(|expires_at| Lease {
                    holder: holder.unwrap_or("fixture").into(),
                    expires_at,
                    claimed_at: Some(expires_at.saturating_sub(1)),
                    generation: (expires_at / 86_400, 1),
                }),
                attempts_today: 0,
                eligible,
                evidence_incomplete: None,
            })
            .collect(),
    };
    let mut sample = json!({"schema_version":1,"observed_at":observed_at,"board":board});
    if let Some(panels) = panels {
        sample["panels"] = panels;
    }
    fs::write(path, format!("{sample}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn write_board(path: &Path, observed_at: u64, rows: &[(u64, Action, bool, Option<u64>)]) {
    let rows: Vec<_> = rows
        .iter()
        .map(|&(number, action, eligible, expires_at)| (number, action, eligible, expires_at, None))
        .collect();
    write_sample(path, observed_at, &rows, None);
}

/// One Lead binding and command factory over a private TestEnv.
fn lead_test() -> (
    TestEnv,
    Dirs,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
    impl Fn() -> Command,
) {
    let test = TestEnv::new();
    let dirs = Dirs::under(test.root());
    fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let uid = fs::metadata(&dirs.agent_dir).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    );
    fs::set_permissions(settings, fs::Permissions::from_mode(0o600)).unwrap();
    let board = dirs.agent_dir.join("board.jsonl");
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let command = {
        let board = board.clone();
        let test_envs = test.envs(&server.url());
        let project = project.clone();
        move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
            command
                .env_clear()
                .envs(test_envs.clone())
                .current_dir(&project.clone())
                .env("OXA_LEAD_BOARD_LOG", &board)
                .args(["--offline", "lead"]);
            command
        }
    };
    let situation = dirs.agent_dir.join("situation.json");
    (test, dirs, board, situation, id, command)
}

fn run_once(command: &impl Fn() -> Command, situation: Option<&Path>) -> Value {
    let mut invocation = command();
    invocation.args(["run", "--once"]);
    if let Some(path) = situation {
        invocation.env("OXA_LEAD_SITUATION", path);
    }
    let output = invocation.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stall_report(state: &Value) -> Vec<&Value> {
    state["stuck"]["report"].as_array().unwrap().iter().collect()
}

#[test]
fn merge_stall_states_per_pr_reasons_and_assigns_red_work_before_waiting() {
    // #1451: approved work is red and unowned while nothing has merged. The pass
    // reports the reason per PR and assigns a worker, and escalates only when
    // nobody can take the item. It never waits in silence.
    let (_test, dirs, board, _situation, id, command) = lead_test();
    let now = oxa_proto::time::now_ms() / 1000;

    // No worker exists at all: the red unowned PR is escalated to the owner.
    write_sample(
        &board,
        now,
        &[
            (66, Action::FixRequiredCi, true, None, None),
            (71, Action::FixFindings, true, Some(now + 60), Some("reviewer-a")),
            (72, Action::Ready, true, None, None),
        ],
        Some(panels_record(now, 46 * 60)),
    );
    let state = run_once(&command, None);
    assert_eq!(state["state"], "stuck_escalated");
    assert!(state["stuck"]["signals"]
        .as_array()
        .unwrap()
        .contains(&json!("merge_stalled")));
    let row = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(66))
        .expect("the red PR is on the stall report");
    assert_eq!(row["reason"], json!("required CI failed"));
    assert_eq!(row["assignee"], Value::Null);
    assert_eq!(
        state["stuck"]["escalated"],
        json!(["lead:stuck:ci:66"]),
        "no worker can take it, so the owner is asked"
    );
    let findings = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(71))
        .expect("the unresolved-finding PR is on the stall report");
    assert_eq!(findings["reason"], json!("unresolved review findings"));

    let store = FsStore::new(dirs.sessions_dir.clone());
    let session = store.resolve(&id).unwrap();
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    let asks = saved.lead.unwrap().open_asks;
    assert!(asks.iter().any(|ask| ask.id == "lead:stuck:ci:66"));

    // An idle worker (its lease expired) takes the next red PR: an assignment,
    // not an owner ask.
    write_sample(
        &board,
        now,
        &[
            (67, Action::FixRequiredCi, true, None, None),
            (70, Action::Ready, true, Some(now - 60), Some("idle-worker")),
        ],
        Some(panels_record(now, 46 * 60)),
    );
    let state = run_once(&command, None);
    let row = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(67))
        .unwrap();
    assert_eq!(row["assignee"], json!("idle-worker"));
    let escalated = state["stuck"]["escalated"].as_array().unwrap();
    assert!(
        !escalated.contains(&json!("lead:stuck:ci:67")),
        "an idle worker took it, so the owner is not asked: {state}"
    );

    // A red PR with its own holder assigns that worker, active lease or not.
    write_sample(
        &board,
        now,
        &[(
            68,
            Action::FixRequiredCi,
            true,
            Some(now + 60),
            Some("own-worker"),
        )],
        Some(panels_record(now, 46 * 60)),
    );
    let state = run_once(&command, None);
    let row = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(68))
        .unwrap();
    assert_eq!(row["assignee"], json!("own-worker"));
    assert!(!state["stuck"]["escalated"]
        .as_array()
        .unwrap()
        .contains(&json!("lead:stuck:ci:68")));

    // A current record without a stall stays silent about stalls.
    write_sample(
        &board,
        now,
        &[(66, Action::FixRequiredCi, true, None, None)],
        Some(panels_record(now, 10 * 60)),
    );
    let state = run_once(&command, None);
    assert!(state.get("stuck").is_none(), "{state}");
    assert_eq!(state["state"], "awaiting_owner");
}

#[test]
fn a_worker_in_publish_pending_is_escalated_in_the_same_pass() {
    // #1575: a locally accepted commit with no publication is never a silent
    // wait. The pass writes the escalation ask durably before it waits again.
    let (_test, _dirs, board, situation, _id, command) = lead_test();
    let now_ms = oxa_proto::time::now_ms();
    let now = now_ms / 1000;
    write_sample(&board, now, &[], Some(panels_record(now, 10 * 60)));

    let snapshot = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl")
        .lines()
        .find_map(|line| oxa_proto::work::Snapshot::from_json(line.as_bytes()).ok())
        .unwrap();
    let mut snapshot = snapshot;
    snapshot.at_ms = now_ms;
    snapshot.local_accepted = vec![oxa_proto::work::LocalAcceptance {
        task: "lander-1".into(),
        commit: "d".repeat(40),
        evidence: oxa_proto::work::Evidence {
            kind: oxa_proto::work::EvidenceKind::LocalCheck,
            id: "evidence-example".into(),
        },
    }];
    snapshot.published.clear();
    fs::write(&situation, snapshot.to_json().unwrap()).unwrap();
    fs::set_permissions(&situation, fs::Permissions::from_mode(0o600)).unwrap();

    let state = run_once(&command, Some(&situation));
    assert_eq!(state["state"], "stuck_escalated");
    assert_eq!(state["stuck"]["signals"], json!(["publish_pending:lander-1"]));
    assert_eq!(
        state["stuck"]["escalated"],
        json!(["lead:stuck:publish:lander-1"])
    );
    assert_eq!(
        state["asks"][0]["id"],
        json!("lead:stuck:publish:lander-1"),
        "the ask is the action of this pass"
    );

    // The next pass keeps the standing ask and never repeats it.
    let state = run_once(&command, Some(&situation));
    assert_eq!(state["state"], "awaiting_owner");
    assert_eq!(state["asks"][0]["id"], json!("lead:stuck:publish:lander-1"));
    assert_eq!(state["stuck"]["escalated"], json!([]));

    // Without the runner situation the signal is unobserved, never invented.
    let state = run_once(&command, None);
    assert_eq!(state["state"], "awaiting_owner");
    assert!(state.get("stuck").is_none(), "{state}");
}

/// The sample's merge queue (exact heads) in its panels record.
fn queue_of(mut panels: Value, prs: &[(u64, String)]) -> Value {
    let nodes: Vec<Value> = prs
        .iter()
        .map(|(number, head)| json!({"pullRequest": {"number": number, "headRefOid": head}}))
        .collect();
    panels["mergeQueue"] = json!({"entries": {"totalCount": nodes.len(), "nodes": nodes}});
    panels
}

/// Add one Reader record (`landing`, `reviews`, ...) to an already written sample.
fn add_field(path: &Path, key: &str, value: Value) {
    let mut sample: Value = serde_json::from_str(fs::read_to_string(path).unwrap().trim()).unwrap();
    sample[key] = value;
    fs::write(path, format!("{sample}\n")).unwrap();
}

fn add_landing(path: &Path, landing: Value) {
    add_field(path, "landing", landing);
}

fn escalated(state: &Value) -> Vec<String> {
    state["stuck"]["escalated"]
        .as_array()
        .map(|ids| ids.iter().map(|id| id.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default()
}

#[test]
fn a_queue_age_alarm_escalates_only_while_the_pr_is_queued_at_that_head() {
    // #2091: the alarm and the queue must agree on the exact (number, head).
    let head = "a".repeat(40);
    let alarm = json!({"80": {"queue_alarm": true, "queue": {"enqueued_at": 1}}});
    let cases: [(&str, Option<Vec<(u64, String)>>, bool); 4] = [
        ("queued at the alarmed head", Some(vec![(80, head.clone())]), true),
        ("absent from the queue", Some(vec![]), false),
        ("queued at another head", Some(vec![(80, "b".repeat(40))]), false),
        ("queue unobserved", None, false),
    ];
    for (name, queue, expected) in cases {
        let (_test, _dirs, board, _situation, _id, command) = lead_test();
        let now = oxa_proto::time::now_ms() / 1000;
        let panels = queue.map(|queue| queue_of(panels_record(now, 10 * 60), &queue));
        write_sample(
            &board,
            now,
            &[(80, Action::WaitRequiredCi, true, None, None)],
            panels,
        );
        add_landing(&board, alarm.clone());
        let state = run_once(&command, None);
        assert_eq!(
            escalated(&state).contains(&"lead:stuck:queue:80".to_owned()),
            expected,
            "{name}: {state}"
        );
    }
}

#[test]
fn a_publication_of_another_commit_does_not_publish_the_accepted_one() {
    // #2090: a task counts as published only when the PR head is the accepted commit.
    let now_ms = oxa_proto::time::now_ms();
    let now = now_ms / 1000;
    for (published_head, expected) in [("e", true), ("d", false)] {
        let (_test, _dirs, board, situation, _id, command) = lead_test();
        write_sample(&board, now, &[], Some(panels_record(now, 10 * 60)));
        let mut snapshot = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl")
            .lines()
            .find_map(|line| oxa_proto::work::Snapshot::from_json(line.as_bytes()).ok())
            .unwrap();
        snapshot.at_ms = now_ms;
        snapshot.local_accepted = vec![oxa_proto::work::LocalAcceptance {
            task: "lander-1".into(),
            commit: "d".repeat(40),
            evidence: oxa_proto::work::Evidence {
                kind: oxa_proto::work::EvidenceKind::LocalCheck,
                id: "evidence-example".into(),
            },
        }];
        snapshot.published = vec![oxa_proto::work::Publication {
            task: "lander-1".into(),
            pr: oxa_proto::work::PullRequest {
                number: 7,
                head: published_head.repeat(40),
            },
        }];
        fs::write(&situation, snapshot.to_json().unwrap()).unwrap();
        fs::set_permissions(&situation, fs::Permissions::from_mode(0o600)).unwrap();
        let state = run_once(&command, Some(&situation));
        assert_eq!(
            escalated(&state).contains(&"lead:stuck:publish:lander-1".to_owned()),
            expected,
            "{state}"
        );
    }
}

#[test]
fn a_stall_with_only_ready_rows_proposes_the_next_action() {
    // #2088: no report row and no escalation must not end the pass silently.
    let (_test, _dirs, board, _situation, _id, command) = lead_test();
    let now = oxa_proto::time::now_ms() / 1000;
    write_sample(
        &board,
        now,
        &[(72, Action::Ready, true, None, None)],
        Some(panels_record(now, 46 * 60)),
    );
    let state = run_once(&command, None);
    assert_eq!(state["state"], "awaiting_owner", "{state}");
    assert_eq!(state["asks"][0]["id"], json!("lead:board:72"));
    assert!(state["stuck"]["signals"]
        .as_array()
        .unwrap()
        .contains(&json!("merge_stalled")));
}

#[test]
fn a_thread_held_pr_is_reported_with_its_head_age_and_owner() {
    // #1549: green CI and one optional failure still leave an unresolved thread
    // held; the report names the exact head, the thread age and who takes it.
    let (_test, _dirs, board, _situation, _id, command) = lead_test();
    let now = oxa_proto::time::now_ms() / 1000;
    write_sample(
        &board,
        now,
        &[
            (90, Action::Ready, true, None, None),
            (91, Action::Ready, true, Some(now - 60), Some("idle-worker")),
        ],
        Some(panels_record(now, 10 * 60)),
    );
    let held = |incomplete| {
        json!({"queue_alarm": false, "threads": {
            "unresolved": 2, "current": 1, "oldest_at": now - 3600,
            "authors": ["reviewer"], "incomplete": incomplete,
        }})
    };
    add_landing(&board, json!({"90": held(false)}));
    let state = run_once(&command, None);
    let row = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(90))
        .unwrap_or_else(|| panic!("{state}"));
    assert_eq!(row["head"], json!("a".repeat(40)));
    assert_eq!(row["threads"], json!(2));
    assert_eq!(row["thread_authors"], json!(["reviewer"]));
    assert_eq!(row["assignee"], json!("idle-worker"));
    assert!((3600..3700).contains(&row["oldest_thread_age_secs"].as_u64().unwrap()));
    assert_eq!(row["incomplete"], json!(false));
    assert_eq!(state["state"], "stuck_assigned", "{state}");

    add_landing(&board, json!({"90": held(true)}));
    let state = run_once(&command, None);
    let row = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(90))
        .unwrap();
    assert_eq!(row["incomplete"], json!(true), "an incomplete page is reported");
}

#[test]
fn a_refused_review_identity_is_escalated_never_waited_on() {
    // #1457: the reviewer refused the requester; the Lead says so instead of waiting.
    let (_test, _dirs, board, _situation, _id, command) = lead_test();
    let now = oxa_proto::time::now_ms() / 1000;
    write_sample(
        &board,
        now,
        &[(50, Action::WaitReview, true, None, None)],
        Some(panels_record(now, 10 * 60)),
    );
    add_field(
        &board,
        "reviews",
        json!({"50": {"decision": {"requester_rejected": {"reviewer": "codex"}}, "requests": []}}),
    );
    add_field(
        &board,
        "quota_deferred",
        json!([{"work": "queued_run_pages", "resource": "core", "awaiting_observation": true}]),
    );
    let state = run_once(&command, None);
    assert_eq!(state["state"], "stuck_escalated", "{state}");
    assert_eq!(escalated(&state), ["lead:stuck:review-identity:50"]);
    assert!(state["stuck"]["signals"]
        .as_array()
        .unwrap()
        .contains(&json!("review_rejected:50")));
    assert_eq!(state["quota_deferred"][0]["work"], "queued_run_pages");
}

#[test]
fn an_assignee_with_a_dead_worker_is_escalated_instead_of_reported_assigned() {
    // #1382: a lease holder whose worker process is gone cannot take the red PR.
    let (_test, dirs, board, _situation, id, command) = lead_test();
    let now = oxa_proto::time::now_ms() / 1000;
    let gone = oxa_cli::workers::registry::WorkerRecord {
        name: "gone-worker".into(),
        session_id: "child-gone".into(),
        pid: u32::MAX - 1,
        pgid: u32::MAX - 1,
        ..Default::default()
    };
    oxa_cli::workers::registry::save(&dirs.agent_dir, &id, &[gone]).unwrap();
    write_sample(
        &board,
        now,
        &[(
            68,
            Action::FixRequiredCi,
            true,
            Some(now + 60),
            Some("gone-worker"),
        )],
        Some(panels_record(now, 46 * 60)),
    );
    let state = run_once(&command, None);
    assert_eq!(state["state"], "stuck_escalated", "{state}");
    assert!(state["stuck"]["signals"]
        .as_array()
        .unwrap()
        .contains(&json!("worker_gone:gone-worker")));
    assert_eq!(escalated(&state), ["lead:stuck:ci:68"]);
    let row = stall_report(&state)
        .into_iter()
        .find(|row| row["pr"] == json!(68))
        .unwrap();
    assert_eq!(row["assignee"], Value::Null);
}

#[test]
fn a_queued_pr_with_a_current_thread_is_escalated_for_dequeue() {
    // #1673: a queued head with an unresolved current thread asks the owner to dequeue.
    let (_test, _dirs, board, _situation, _id, command) = lead_test();
    let now = oxa_proto::time::now_ms() / 1000;
    let panels = queue_of(panels_record(now, 10 * 60), &[(95, "a".repeat(40))]);
    write_sample(
        &board,
        now,
        &[(95, Action::Ready, true, None, None)],
        Some(panels),
    );
    add_landing(
        &board,
        json!({"95": {"queue_alarm": false, "queue": {"enqueued_at": now - 60}, "threads": {
            "unresolved": 1, "current": 1, "oldest_at": now - 120, "authors": ["reviewer"], "incomplete": false,
        }}}),
    );
    let state = run_once(&command, None);
    assert_eq!(state["state"], "stuck_escalated", "{state}");
    assert_eq!(escalated(&state), ["lead:stuck:queued-findings:95"]);
    assert!(state["stuck"]["signals"]
        .as_array()
        .unwrap()
        .contains(&json!("queued_findings:95")));
}

#[test]
fn running_lead_reopens_the_same_owner_ask_after_process_kill() {
    let test = TestEnv::new();
    let dirs = Dirs::under(test.root());
    fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let uid = fs::metadata(&dirs.agent_dir).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    );
    fs::set_permissions(settings, fs::Permissions::from_mode(0o600)).unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    let session = store.resolve(&id).unwrap();
    let board = dirs.agent_dir.join("board.jsonl");
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
        command
            .env_clear()
            .envs(test.envs(&server.url()))
            .current_dir(&project)
            .env("OXA_LEAD_BOARD_LOG", &board)
            .args(["--offline", "lead"]);
        command
    };

    let help = command().arg("--help").output().unwrap();
    assert!(help.status.success(), "{help:?}");
    write_board(&board, 1, &[(42, Action::Ready, true, None)]);
    let agent = command()
        .env("AI_AGENT", "")
        .args(["run", "--once"])
        .output()
        .unwrap();
    assert!(!agent.status.success());
    assert_eq!(Goal::revision(&store.load(&session).unwrap()), 0);
    let stale = command().args(["run", "--once"]).output().unwrap();
    assert!(stale.status.success(), "{stale:?}");
    let stale: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(stale["state"], "board_unavailable");
    assert_eq!(stale["asks"], json!([]));

    let now = oxa_proto::time::now_ms() / 1000;
    write_board(
        &board,
        now,
        &[
            (8, Action::FixFindings, false, Some(now + 60)),
            (42, Action::Ready, true, None),
            (66, Action::FixRequiredCi, true, Some(now)),
        ],
    );
    let mut running = command().arg("run").stdout(Stdio::piped()).spawn().unwrap();
    let mut first = String::new();
    BufReader::new(running.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    running.kill().unwrap();
    assert!(!running.wait().unwrap().success());
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["state"], "awaiting_owner");
    assert_eq!(first["asks"].as_array().unwrap().len(), 1);
    assert_eq!(first["asks"][0]["id"], "lead:board:66");

    write_board(&board, now, &[(12, Action::BuildIssue, true, None)]);
    assert!(first["asks"][0].get("question").is_none());
    let reopened = command().args(["run", "--once"]).output().unwrap();
    assert!(reopened.status.success(), "{reopened:?}");
    let reopened: Value = serde_json::from_slice(&reopened.stdout).unwrap();
    assert_eq!(reopened["goal_revision"], first["goal_revision"]);
    assert_eq!(reopened["asks"], first["asks"]);

    let asks = command().arg("asks").output().unwrap();
    assert!(asks.status.success(), "{asks:?}");
    let asks: Value = serde_json::from_slice(&asks.stdout).unwrap();
    assert_eq!(asks["goal_revision"], first["goal_revision"]);
    assert_eq!(asks["asks"][0]["id"], first["asks"][0]["id"]);
    assert!(asks["asks"][0]["question"].as_str().unwrap().contains("#66"));
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert!(saved.lead.unwrap().held_leases.is_empty());
    assert!(server.requests().is_empty());
}

#[test]
fn a_missing_protected_board_observation_keeps_the_saved_ask_and_names_the_cause() {
    let test = TestEnv::new();
    let dirs = Dirs::under(test.root());
    fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let uid = fs::metadata(&dirs.agent_dir).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    );
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    let session = store.resolve(&id).unwrap();
    let board = dirs.agent_dir.join("board.jsonl");
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
        command
            .env_clear()
            .envs(test.envs(&server.url()))
            .current_dir(&project)
            .env("OXA_LEAD_BOARD_LOG", &board)
            .args(["--offline", "lead"]);
        command
    };

    // A fresh observation saves one owner ask in the protected Lead session.
    let now = oxa_proto::time::now_ms() / 1000;
    write_board(&board, now, &[(42, Action::Ready, true, None)]);
    let fresh = command().args(["run", "--once"]).output().unwrap();
    assert!(fresh.status.success(), "{fresh:?}");
    let fresh: Value = serde_json::from_slice(&fresh.stdout).unwrap();
    assert_eq!(fresh["state"], "awaiting_owner");
    assert_eq!(fresh["asks"][0]["id"], "lead:board:42");
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert_eq!(saved.lead.unwrap().open_asks.len(), 1);

    // The protected observation is now missing: the saved ask must survive
    // and the observation must report an actionable, path-free cause.
    fs::remove_file(&board).unwrap();
    let missing = command().args(["run", "--once"]).output().unwrap();
    assert!(missing.status.success(), "{missing:?}");
    let missing: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["asks"][0]["id"], "lead:board:42");
    assert_eq!(
        missing["cause"].as_str(),
        Some("the protected Board observation is missing"),
        "{missing}"
    );
    assert_eq!(
        missing["hint"].as_str(),
        Some("write a fresh protected Board observation and rerun the Lead loop"),
        "{missing}"
    );
    assert!(!missing.to_string().contains(&*dirs.agent_dir.to_string_lossy()));
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert_eq!(saved.lead.unwrap().open_asks.len(), 1);
    assert!(server.requests().is_empty());
}
