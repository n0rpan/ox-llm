use oxa_work::{
    board::{
        check_recovery::{Current, Trigger},
        Board,
    },
    shadow::{
        self,
        rerun::{run_current_once, CurrentSample, Policy, Status},
    },
    RunPolicy,
};
use serde_json::{json, Value};
use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};

#[path = "support/private_dir.rs"]
mod private_dir;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const WORKFLOW: &str = ".github/workflows/checks.yml";

fn repository(attempt: u64, status: &str) -> Value {
    json!({
        "id":"R_repo", "nameWithOwner":"sample/repo",
        "pr":{"number":42,"state":"OPEN","headRefOid":HEAD},
        "checked":{"oid":HEAD,"statusCheckRollup":{"commit":{"oid":HEAD},
            "contexts":{"totalCount":1,"nodes":[{
                "__typename":"CheckRun", "databaseId":30+attempt, "isRequired":true,
                "name":"gate","status":status,"conclusion":"SUCCESS",
                "completedAt":"1970-01-02T00:00:00Z",
                "checkSuite":{"databaseId":19,"commit":{"oid":HEAD},
                    "workflowRun":{"databaseId":12,"runAttempt":attempt,"event":"pull_request"}}
            }]}
        }}
    })
}

#[test]
fn current_reader_reconciles_only_an_acknowledged_job_and_never_reissues_a_pending_send() {
    for acknowledged in [true, false] {
        let temp = private_dir::PrivateDir::new("rerun-reader-conformance");
        let repo = temp.path();
        let journal = repo.join("owner.jsonl");
        let git = repo.join("git");
        let gh = repo.join("gh");
        fs::write(
            &git,
            "#!/bin/sh\nprintf '%s\\n' https://example.invalid/sample/repo\n",
        )
        .unwrap();
        fs::write(
            &gh,
            r#"#!/bin/sh
case "$*" in
  'repo view https://example.invalid/sample/repo --json id,nameWithOwner,url')
    printf '%s\n' '{"id":"R_repo","nameWithOwner":"sample/repo","url":"https://example.invalid/sample/repo"}' ;;
  'api graphql --hostname example.invalid -f query=query{viewer{id}}')
    printf '%s\n' '{"data":{"viewer":{"id":"U_publisher"}}}' ;;
  'api --hostname example.invalid --include --method POST repos/sample/repo/actions/jobs/23/rerun')
    printf '%s\n' 23 >> calls
    if test -f accepted; then printf 'HTTP/2 201\r\ncontent-length: 0\r\n\r\n'; else exit 1; fi ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        for executable in [&git, &gh] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        if acknowledged {
            fs::write(repo.join("accepted"), "").unwrap();
        }
        let board = Board {
            observed_at: 87_002,
            ..Default::default()
        };
        {
            let mut log = shadow::open_log(&journal).unwrap();
            shadow::run(&mut log, true, || Ok(board.clone()), |_| unreachable!()).unwrap();
        }
        let policy = Policy {
            enabled: true,
            publisher_id: "U_publisher".into(),
        };
        let run = RunPolicy {
            git_binary: git,
            host_path: Default::default(),
            terminate_binary: None,
            offline: false,
            runner_owns_github: false,
            storage: repo.to_owned(),
        };

        let apply = |policy: &Policy, attempt: u64, run_id, job_head: &str, changed_job| {
            let attempt_reads = Cell::new(0);
            run_current_once(
                policy,
                &journal,
                repo,
                &run,
                &gh,
                CurrentSample {
                    board: &board,
                    target: Current {
                        repository: "sample/repo",
                        repository_id: "R_repo",
                        number: 42,
                        pr_head: HEAD,
                        head: HEAD,
                        workflow_path: WORKFLOW,
                        required_name: "gate",
                        trigger: Trigger::PullRequest,
                    },
                },
                (
                    |fields: &str| {
                        assert!(fields.contains("pullRequest(number:42)"));
                        assert!(
                            shadow::open_log(&journal).is_err(),
                            "Reader needs the journal lease"
                        );
                        Ok(repository(
                            attempt,
                            if attempt == 2 { "IN_PROGRESS" } else { "COMPLETED" },
                        ))
                    },
                    |address: &str| {
                        let prefix = format!("runs/12/attempts/{attempt}");
                        if address == prefix {
                            Ok(json!({
                                "id":run_id,"check_suite_id":19,"head_sha":HEAD,"run_attempt":attempt,
                                "event":"pull_request","path":WORKFLOW
                            }))
                        } else {
                            assert_eq!(address, format!("{prefix}/jobs?per_page=100"));
                            let prior = attempt_reads.replace(attempt_reads.get() + 1);
                            let job = 21 + attempt + u64::from(changed_job && prior > 0);
                            Ok(json!({"total_count":1,"jobs":[{
                                "id":job,"run_id":12,"run_attempt":attempt,"head_sha":job_head,"name":"gate",
                                "check_run_url":format!("https://api.github.com/repos/sample/repo/check-runs/{}",30+attempt)
                            }]}))
                        }
                    },
                ),
            )
        };

        assert_eq!(
            apply(&Policy::default(), 2, 12, HEAD, false).unwrap(),
            Status::Skipped
        );
        for (run_id, head, change) in [(13, HEAD, false), (12, OTHER_HEAD, false), (12, HEAD, true)] {
            assert_eq!(apply(&policy, 2, run_id, head, change).unwrap(), Status::Skipped);
            assert!(!repo.join("calls").exists());
        }
        let dispatched = apply(&policy, 2, 12, HEAD, false);
        if acknowledged {
            assert!(matches!(dispatched, Ok(Status::Acknowledged(_))));
            assert_eq!(apply(&policy, 2, 12, HEAD, false).unwrap(), dispatched.unwrap());
            let reconciled = apply(&policy, 3, 12, HEAD, false).unwrap();
            let Status::Reconciled {
                acknowledgment,
                replacement,
            } = &reconciled
            else {
                panic!("a later verified attempt should reconcile, got {reconciled:?}");
            };
            assert_eq!(acknowledgment.intent.job_id, 23);
            assert_eq!(replacement.job_id, 24);
            assert_eq!(replacement.check_run_id, 33);
            assert_eq!(replacement.run_attempt, 3);
            assert_eq!(apply(&policy, 3, 12, HEAD, false).unwrap(), reconciled);
        } else {
            assert!(dispatched.is_err());
            assert_eq!(apply(&policy, 2, 12, HEAD, false).unwrap(), Status::Reserved);
            assert_eq!(apply(&policy, 3, 12, HEAD, false).unwrap(), Status::Reserved);
        }
        assert_eq!(fs::read_to_string(repo.join("calls")).unwrap(), "23\n");
    }
}
