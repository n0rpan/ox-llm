#[path = "common/work_repo.rs"]
mod work_repo;

use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn trusted_ox_llm_profile_classifies_each_core_family() {
    let env = TestEnv::new();
    work_repo::setup(&env, "[]");
    let project = env.work().join("project");
    let git = |args: &[&str]| work_repo::git(&env, &project, args);
    let base = git(&["rev-parse", "HEAD"]);
    fs::set_permissions(env.tmp(), fs::Permissions::from_mode(0o700)).unwrap();
    let gh = env.write(
        "tmp/gh",
        r#"#!/bin/sh
case " $* " in *' --include '*) printf 'HTTP/2.0 200 OK\r\n\r\n' ;; esac
case "$*" in
  *'p42:pullRequest(number:42)'*|*'pullRequests(first:100'*) cat snapshot.json ;;
  *) exit 92 ;;
esac
"#,
    );
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();

    let profile: Value =
        serde_json::from_str(include_str!("../../docs/examples/ox-llm-board-review-core.json")).unwrap();
    let mut settings = json!({
        "requester": "review-bot",
        "codex": "code-reviewer",
        "security": "security-reviewer",
        "owner": "owner-reviewer",
        "core": [],
        "protected": []
    });
    let cases = [
        ("oxa/oxa-rpc/src/a2a_control.rs", "core"),
        ("oxa/oxa-work/src/managed_steering.rs", "core"),
        ("oxa/oxa-cli/src/work_cmd/native_queue.rs", "core"),
        ("oxa/oxa-cli/src/lead_access.rs", "core"),
        ("oxa/oxa-cli/src/lead_access/proxy.rs", "core"),
        ("oxa/oxa-cli/src/lead_peer.rs", "core"),
        ("oxa/oxa-cli/src/share_cmd.rs", "core"),
        ("oxa/oxc-gateway/src/share/native.rs", "core"),
        ("oxa/oxa-rpc/src/server.rs", "core"),
        ("oxa/oxc-gateway/src/share_extra/lib.rs", "product"),
        ("oxa/oxa-rpc/src/client.rs", "product"),
        ("oxa/oxc-gateway/src/share/README.md", "docs_and_tests"),
    ];
    for (path, tier) in cases {
        git(&["reset", "--hard", &base]);
        env.write(&format!("work/project/{path}"), "synthetic change\n");
        git(&["add", "--", path]);
        git(&["commit", "-m", "fixture"]);
        let head = git(&["rev-parse", "HEAD"]);
        let snapshot = json!({"data":{"repository":{
            "pullRequests":{"totalCount":1,"nodes":[{"number":42,"headRefOid":head}],
                "pageInfo":{"hasNextPage":false,"endCursor":"synthetic-end"}},
            "p42":{
                "number":42,"headRefOid":head,"baseRefName":"main","baseRefOid":base,
                "state":"OPEN","body":"","isDraft":false,"reviewDecision":null,
                "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN",
                "authorAssociation":"MEMBER","isCrossRepository":false,
                "labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}},
                "comments":{"totalCount":0,"nodes":[]},"reviewThreads":{"totalCount":0,"nodes":[]},
                "latestOpinionatedReviews":{"totalCount":0,"nodes":[]},
                "baseRef":{"refUpdateRule":{"requiredStatusCheckContexts":["ci"]},"rules":{"totalCount":0,"nodes":[]}},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[
                    {"name":"ci","isRequired":true,"status":"COMPLETED","conclusion":"SUCCESS"}
                ]}}}}]}
            }
        }}});
        env.write("work/project/snapshot.json", &snapshot.to_string());

        // Run one case both with the current profile and with an empty core list.
        let without_profile = (path == cases[0].0).then_some((json!([]), "product"));
        for (core, expected_tier) in without_profile
            .into_iter()
            .chain([(profile["core"].clone(), tier)])
        {
            settings["core"] = core;
            let policy = env.write("home/review.json", &settings.to_string());
            let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
                .args([
                    "work",
                    "board",
                    "--shadow",
                    "--repo",
                    "example/repo",
                    "--once",
                    "--review-policy",
                ])
                .arg(policy)
                .current_dir(&project)
                .env_clear()
                .envs(env.envs("http://127.0.0.1:1"))
                .env("GH_TOKEN", "synthetic")
                .env(
                    "PATH",
                    format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
                )
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let project_state = fs::read_dir(env.agent_dir().join("work"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let log =
                fs::read_to_string(project_state.join("board/example/repo/board-shadow.jsonl")).unwrap();
            let sample: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
            let decision = &sample["reviews"]["42"]["decision"]["review"];
            let reviewers = if expected_tier == "core" {
                json!(["codex", "security"])
            } else {
                json!(["codex"])
            };
            assert_eq!(decision["tier"], expected_tier, "{path}");
            assert_eq!(decision["request"], reviewers, "{path}");
            assert_eq!(decision["outstanding"], reviewers, "{path}");
        }
    }
}
