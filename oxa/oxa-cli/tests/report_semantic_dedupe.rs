//! Reporting controls use fake GitHub/model responses only; no process or network access.
use oxa_cli::{submit, ScrubCtx};
use oxa_config::Settings;
use oxa_fake::TestEnv;
use serde_json::json;
use std::cell::RefCell;

#[test]
fn five_reworded_occurrences_create_once_then_comment_on_the_same_root() {
    let env = TestEnv::new();
    let settings = Settings::from_values(
        json!({"oxa":{"reportIssue":{"repo":"example/project","semanticDedupe":true}}}),
        json!({}),
    );
    let calls = RefCell::new(Vec::<Vec<String>>::new());
    let titles = [
        "Task packet requests a forbidden tool",
        "Worker instructions conflict with tool permissions",
        "Issue context missing while fetching is prohibited",
        "Packet cannot be completed without an unavailable fetch",
        "Helper lacks the issue body and cannot retrieve it",
    ];
    let mut runner = |program: &str, args: &[String]| {
        if program == "gh" {
            calls.borrow_mut().push(args.to_vec());
            if args.iter().any(|a| a == "create") {
                return Some((0, "https://github.com/example/project/issues/42\n".into()));
            }
            if args.iter().any(|a| a == "comment") {
                return Some((0, String::new()));
            }
            let created = calls.borrow().iter().any(|a| a.iter().any(|s| s == "create"));
            return Some((
                0,
                if created {
                    json!([{"number":42,"title":titles[0],"body":"Task packet forbids its required fetch.",
                        "labels":[{"name":"bug"}],"state":"OPEN"}])
                    .to_string()
                } else {
                    "[]".into()
                },
            ));
        }
        assert_eq!(
            program, "oxa",
            "only the existing native model runner is permitted"
        );
        let created = calls.borrow().iter().any(|a| a.iter().any(|s| s == "create"));
        Some((
            0,
            json!({"verified":true,"match":if created {Some(42)} else {None},
                         "rejected":[]})
            .to_string(),
        ))
    };
    for (index, title) in titles.iter().enumerate() {
        let draft = env.write(
            &format!("drafts/occurrence-{index}.md"),
            &format!(
                "Title: {title}\n\nTask: task-{index}\nWorker: synthetic-worker\nRun: run-{index}\n\
             Observed: command `oxa work` refused the fetch required by its task packet.\n\
             Evidence: oxa/oxa-work/src/worker.rs\n"
            ),
        );
        let mut out = Vec::new();
        let result = submit(
            &draft,
            &settings,
            &ScrubCtx::default(),
            &mut |_| Some(true),
            &mut runner,
            &mut out,
        )
        .unwrap();
        assert_eq!(result, 0, "{}", String::from_utf8_lossy(&out));
    }
    let calls = calls.borrow();
    let count = |verb: &str| calls.iter().filter(|a| a.iter().any(|s| s == verb)).count();
    assert_eq!(
        count("create"),
        1,
        "rewording must not bypass same-root dedupe: {calls:?}"
    );
    assert_eq!(
        count("comment"),
        4,
        "each recurrence belongs on the matched issue"
    );
}
