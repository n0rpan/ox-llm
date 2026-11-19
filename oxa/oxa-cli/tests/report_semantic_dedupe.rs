//! Reporting controls use fake GitHub/model responses only; no process or network access.
use oxa_cli::{submit, ScrubCtx};
use oxa_config::Settings;
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::io::Write;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const TITLE: &str = "Task packet requests a forbidden tool";
const DESCRIPTION: &str = "Reproduction: retry with the runner-owned issue body missing.";
const SYSTEM_ONLY: &str = "synthetic-controller-status";

fn settings() -> Settings {
    Settings::from_values(
        json!({"oxa":{"reportIssue":{"repo":"example/project","semanticDedupe":true}}}),
        json!({}),
    )
}

fn settings_with_cap(limit: u64) -> Settings {
    Settings::from_values(
        json!({"oxa":{"reportIssue":{"repo":"example/project","semanticDedupe":true,"dailyNew":limit}}}),
        json!({}),
    )
}

fn file_report(
    env: &TestEnv,
    index: usize,
    title: &str,
    task: usize,
    run: &mut Fixture,
    out: &mut dyn Write,
) -> i32 {
    let mut confirm_all = |_: &str| Some(true);
    file_report_with(env, index, title, task, run, out, &settings(), &mut confirm_all)
}

fn file_report_with(
    env: &TestEnv,
    index: usize,
    title: &str,
    task: usize,
    run: &mut Fixture,
    out: &mut dyn Write,
    settings: &Settings,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> i32 {
    let evidence = run.evidence.unwrap_or("oxa/oxa-work/src/worker.rs");
    let draft = env.write(
        &format!("drafts/occurrence-{index}.md"),
        &format!(
            "Title: {title}\n\nTask: task-{task}\nWorker: synthetic-worker\nRun: run-{index}\n\n\
             Observed: command `oxa work` refused the fetch required by its task packet.\n\n\
             Evidence: {evidence}:2\n\n{DESCRIPTION}\n\n## System\n\n{SYSTEM_ONLY}\n"
        ),
    );
    let result = submit(
        &draft,
        settings,
        &ScrubCtx::default(),
        confirm,
        &mut |program, args| run.invoke(program, args),
        out,
    )
    .unwrap();
    assert!(draft.exists());
    result
}

#[derive(Default)]
struct Fixture {
    creates: usize,
    comments: usize,
    open_searches: usize,
    closed_searches: usize,
    judged: usize,
    match_issue: bool,
    unverified: bool,
    evidence: Option<&'static str>,
    verify_description: bool,
}

impl Fixture {
    fn issue(number: usize) -> Value {
        json!({"number":number, "title":TITLE, "body":"Task packet forbids its required fetch.",
               "labels":[{"name":"bug"}]})
    }

    fn invoke(&mut self, program: &str, args: &[String]) -> Option<(i32, String)> {
        let result = if program == "oxa" {
            self.judged += 1;
            let payload: Value = serde_json::from_str(&args[0]).unwrap();
            assert_eq!(payload["current_main"]["sha"], SHA);
            if self.verify_description {
                let description = payload["description"].as_str().expect("judge description");
                assert!(description.contains(DESCRIPTION));
                assert!(!description.contains(SYSTEM_ONLY));
            }
            assert!(payload["current_main"]["source"]
                .as_str()
                .unwrap()
                .contains("disabled"));
            let matches = self.match_issue && self.creates > 0;
            let selected = matches.then_some(42);
            let candidates = payload["candidates"].as_array().unwrap();
            if matches {
                let matched = candidates.iter().find(|row| row["number"] == 42).unwrap();
                assert_eq!(
                    matched,
                    &json!({
                        "number":42, "title":TITLE, "body":"Task packet forbids its required fetch.", "labels":["bug"]
                    })
                );
            }
            let rejected = candidates
                .iter()
                .filter_map(|row| row["number"].as_u64())
                .filter(|number| Some(*number) != selected)
                .collect::<Vec<_>>();
            json!({"verified":!self.unverified,"match":selected,"rejected":rejected}).to_string()
        } else {
            assert_eq!(program, "gh");
            match args[0].as_str() {
                "api" if args[1].ends_with("git/ref/heads/main") => SHA.to_string(),
                "api" if args[1] == "repos/example/project" => "public".into(),
                "api" => {
                    let evidence = self.evidence.unwrap_or("oxa/oxa-work/src/worker.rs");
                    assert!(args
                        .last()
                        .unwrap()
                        .ends_with(&format!("contents/{evidence}?ref={SHA}")));
                    "fn work() {\n    fetch_if_enabled(false); // disabled\n}\n".into()
                }
                "search" => {
                    let closed = args.windows(2).any(|a| a == ["--state", "closed"]);
                    if closed {
                        self.closed_searches += 1;
                        assert!(args
                            .windows(2)
                            .any(|a| a[0] == "--closed" && a[1].starts_with(">=")));
                    } else {
                        self.open_searches += 1;
                    }
                    // The third through fifth reports only find the issue in closed search.
                    if self.match_issue && self.creates > 0 && self.comments > 0 && closed {
                        json!([Self::issue(42)]).to_string()
                    } else {
                        json!([Self::issue(17)]).to_string()
                    }
                }
                "issue" if args[1] == "view" => Self::issue(args[2].parse().unwrap()).to_string(),
                "issue" if args[1] == "create" => {
                    // The approved body travels as the `--body` argument, never as a pathname.
                    let text = args.last().unwrap().clone();
                    assert!(text.contains("Candidates rejected as different root causes: #17"));
                    assert!(text.contains(SHA));
                    self.creates += 1;
                    format!(
                        "https://github.com/example/project/issues/{}\n",
                        41 + self.creates
                    )
                }
                "issue" if args[1] == "comment" => {
                    assert_eq!(args[2], "42");
                    // The approved body travels as the `--body` argument, never as a pathname.
                    let text = args.last().unwrap().clone();
                    assert!(text.contains(&format!("Run: run-{}", self.comments + 1)));
                    assert!(text.contains("oxa version:"));
                    assert!(text.contains("Observed:"));
                    self.comments += 1;
                    "https://github.com/example/project/issues/42#issuecomment-1".into()
                }
                _ => panic!("Unexpected GitHub call {args:?}"),
            }
        };
        Some((0, result))
    }
}

#[test]
fn five_reworded_occurrences_create_once_then_comment_on_the_same_root() {
    let env = TestEnv::new();
    let mut run = Fixture {
        match_issue: true,
        ..Fixture::default()
    };
    for (index, title) in [
        TITLE,
        "Worker instructions conflict with tool permissions",
        "Issue context missing while fetching is prohibited",
        "Packet cannot be completed without an unavailable fetch",
        "Helper lacks the issue body and cannot retrieve it",
    ]
    .into_iter()
    .enumerate()
    {
        let mut out = Vec::new();
        let code = file_report(&env, index, title, index, &mut run, &mut out);
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    }
    assert_eq!((run.creates, run.comments, run.judged), (1, 4, 5));
    assert_eq!((run.open_searches, run.closed_searches), (10, 10));
}

#[test]
fn new_issues_are_one_per_task_and_unverified_observations_stay_local() {
    let env = TestEnv::new();
    let mut run = Fixture {
        unverified: true,
        ..Fixture::default()
    };
    let report = |index, task, run: &mut Fixture| {
        let mut out = Vec::new();
        let result = file_report(&env, index, TITLE, task, run, &mut out);
        (result, String::from_utf8(out).unwrap())
    };
    assert!(report(0, 0, &mut run).1.contains("not verified"));
    run.unverified = false;
    assert_eq!(report(1, 0, &mut run).0, 0);
    assert!(report(2, 0, &mut run).1.contains("already used"));
    assert_eq!(report(3, 1, &mut run).0, 0);
    assert_eq!(report(4, 2, &mut run).0, 0);
    // No daily cap unless the owner sets `oxa.reportIssue.dailyNew`.
    assert_eq!(report(5, 3, &mut run).0, 0);
    assert_eq!((run.creates, run.comments), (4, 0));
}

#[test]
fn semantic_no_cap_by_default() {
    let env = TestEnv::new();
    let mut run = Fixture::default();
    for task in 0..5 {
        let mut out = Vec::new();
        let mut questions = Vec::new();
        let code = file_report_with(
            &env,
            task,
            TITLE,
            task,
            &mut run,
            &mut out,
            &settings(),
            &mut |question: &str| {
                questions.push(question.to_string());
                Some(true)
            },
        );
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
        assert!(!questions.iter().any(|question| question.contains("Warning")));
    }
    assert_eq!((run.creates, run.comments), (5, 0));
}

#[test]
fn semantic_cap_warns_when_set() {
    let env = TestEnv::new();
    let mut run = Fixture::default();
    let mut warned = Vec::new();
    for task in 0..3 {
        let mut out = Vec::new();
        let mut questions = Vec::new();
        let code = file_report_with(
            &env,
            task,
            TITLE,
            task,
            &mut run,
            &mut out,
            &settings_with_cap(2),
            &mut |question: &str| {
                questions.push(question.to_string());
                Some(true)
            },
        );
        // Over the cap the post still goes through on the owner's yes, with one warning.
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
        warned.push(questions.iter().any(|question| {
            question.contains("Warning") && question.contains("oxa.reportIssue.dailyNew = 2")
        }));
    }
    assert_eq!(warned, vec![false, false, true]);
    assert_eq!((run.creates, run.comments), (3, 0));
}

#[test]
fn semantic_report_accepts_tracked_dot_directory_evidence() {
    let env = TestEnv::new();
    let mut run = Fixture {
        evidence: Some(".github/workflows/checks.yml"),
        ..Fixture::default()
    };
    let mut out = Vec::new();
    let code = file_report(&env, 0, TITLE, 0, &mut run, &mut out);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    assert_eq!(run.judged, 1);
}

#[test]
fn semantic_report_judge_reads_reproduction_without_controller_status() {
    let env = TestEnv::new();
    let mut run = Fixture {
        verify_description: true,
        ..Fixture::default()
    };
    let mut out = Vec::new();
    let code = file_report(&env, 0, TITLE, 0, &mut run, &mut out);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    assert_eq!(run.judged, 1);
}
