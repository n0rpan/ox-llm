use oxa_fake::TestEnv;
use oxa_proto::{
    Access, Answer, CancelToken, ExecResult, ExecSpec, HeadlessUi, Level, Mode, OxaError, Placement, Sandbox,
    Tool, ToolCtx, Ui, Value,
};
use oxa_tools::AskTool;
use serde_json::json;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

struct NoSandbox;
impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
        vec![]
    }
    fn run(&self, _: &ExecSpec, _: &CancelToken, _: &mut dyn FnMut(&[u8])) -> ExecResult {
        panic!("ask must not execute anything")
    }
}

fn question() -> Value {
    json!({"question":"Which format?", "options":[
        {"label":"JSON", "detail":"Machine readable"},
        {"label":"Text", "detail":"Easy to read"}
    ], "recommended":"Text"})
}

fn context<'a>(env: &'a TestEnv, cancel: &'a CancelToken) -> ToolCtx<'a> {
    ToolCtx {
        cwd: env.root(),
        spill_dir: env.root(),
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel,
        env: &[],
        tune: Default::default(),
        checked: &[],
        hidden: None,
    }
}

#[derive(Default)]
struct Owner {
    pick: Option<usize>,
    calls: Vec<(String, Vec<String>)>,
}
impl Ui for Owner {
    fn interactive(&self) -> bool {
        true
    }
    fn select(&mut self, title: &str, options: &[String], _: Option<u32>) -> Option<usize> {
        self.calls.push((title.into(), options.to_vec()));
        self.pick
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        panic!("not a permission prompt")
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, _: &str, _: Level) {}
    fn set_status(&mut self, _: &str, _: Option<&str>) {}
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        panic!("ask never grants access")
    }
}

#[test]
fn owner_selects_with_details_and_recommendation() {
    let env = TestEnv::new();
    let file = env.root().join("asks.jsonl");
    let ask = AskTool::new(file.clone());
    let cancel = CancelToken::new();
    let cx = context(&env, &cancel);
    let mut ui = Owner {
        pick: Some(0),
        ..Owner::default()
    };
    assert!(ask.needs_ui());
    assert!(ask.access(&question(), &cx).unwrap().is_empty());
    let out = ask.run_with_ui(&question(), &cx, &mut |_| {}, &mut ui);
    assert!(!out.is_error);
    assert_eq!(out.text, "Owner selected: JSON");
    assert_eq!(
        ui.calls,
        vec![(
            "Which format?".into(),
            vec![
                "JSON: Machine readable".into(),
                "Text (recommended): Easy to read".into()
            ]
        )]
    );
    assert!(
        !file.exists(),
        "answered questions need no unattended journal entry"
    );
}

#[test]
fn dismissed_question_is_not_recommendation_or_consent() {
    let env = TestEnv::new();
    let file = env.root().join("asks.jsonl");
    let ask = AskTool::new(file.clone());
    let cancel = CancelToken::new();
    let out = ask.run_with_ui(
        &question(),
        &context(&env, &cancel),
        &mut |_| {},
        &mut Owner::default(),
    );
    assert!(out.text.contains("dismissed"));
    assert!(!out.text.contains("proceed"));
    assert!(!file.exists());
}

#[test]
fn headless_appends_private_jsonl_and_cancel_does_not_ask() {
    let env = TestEnv::new();
    let file = env.root().join("asks.jsonl");
    let ask = AskTool::new(file.clone());
    let cancel = CancelToken::new();
    let cx = context(&env, &cancel);
    for _ in 0..2 {
        let out = ask.run_with_ui(&question(), &cx, &mut |_| {}, &mut HeadlessUi::default());
        assert!(!out.is_error);
        assert_eq!(out.text, "no owner; proceed on the recommended option: Text");
    }
    let contents = fs::read_to_string(&file).unwrap();
    assert_eq!(contents.lines().count(), 2);
    for line in contents.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["options"], question()["options"]);
        assert_eq!(v["recommended"], "Text");
    }
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    cancel.cancel();
    let mut owner = Owner::default();
    assert!(
        ask.run_with_ui(&question(), &cx, &mut |_| {}, &mut owner)
            .is_error
    );
    assert!(owner.calls.is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), contents);
}

#[test]
fn malformed_questions_do_not_show_ui_or_write_journal() {
    let env = TestEnv::new();
    let file = env.root().join("asks.jsonl");
    let ask = AskTool::new(file.clone());
    let cancel = CancelToken::new();
    let cx = context(&env, &cancel);
    let mut cases = vec![
        Value::Null,
        json!({}),
        question(),
        question(),
        question(),
        question(),
        question(),
        question(),
    ];
    cases[2]["question"] = json!("  ");
    cases[3]["recommended"] = json!("Unknown");
    cases[4]["options"] = json!([{"label":"Only", "detail":"one"}]);
    cases[5]["options"] = json!(vec![json!({"label":"x", "detail":"d"}); 5]);
    cases[6]["options"][1]["label"] = json!("JSON");
    cases[7]["options"][0]["detail"] = json!(false);
    let mut owner = Owner::default();
    for args in cases {
        assert!(ask.access(&args, &cx).is_err(), "{args}");
        assert!(
            ask.run_with_ui(&args, &cx, &mut |_| {}, &mut owner).is_error,
            "{args}"
        );
    }
    assert!(owner.calls.is_empty());
    assert!(!file.exists());
}

#[test]
fn journal_failure_is_visible_without_blocking_or_following_links() {
    let env = TestEnv::new();
    let target = env.root().join("target");
    fs::write(&target, "untouched").unwrap();
    let file = env.root().join("asks.jsonl");
    symlink(&target, &file).unwrap();
    let cancel = CancelToken::new();
    let out = AskTool::new(file).run(&question(), &context(&env, &cancel), &mut |_| {});
    assert!(!out.is_error);
    assert!(out.text.contains("no owner; proceed on the recommended option"));
    assert!(out.text.contains("could not record"));
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
}
