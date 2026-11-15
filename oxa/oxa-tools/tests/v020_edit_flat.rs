//! T1 EDIT-FLAT (OXA-LOCAL-FIRST): the `edit` tool accepts the `edits[]` form and the flat
//! `{path, oldText, newText}` form (the flat one is declared; `oxa-core` tests the declaration).
//! Both forms give the same texts.
//!
//! Capability mapping (pi form -> flat form), each checked below:
//! * one replacement: `edits: [{oldText, newText}]` -> one flat call; same result text, file,
//!   error texts.
//! * several disjoint replacements in one file: one call with `edits: [a, b, ..]` -> several flat
//!   calls in one reply, run in order (`oxa-core` tests the ordering through the loop);
//!   disjoint changes give the same final file.
//! * uniqueness, not found, empty oldText, no change: per call, pi's single-edit texts.
//! * BOM / line endings preserved: the same code path (`apply_edits`) for both forms.
//! * pi's legacy input (`edits` as a JSON string, as one object, top-level oldText/newText
//!   appended): unchanged.
//!
//! Atomicity: one `edits[]` call is all-or-nothing and every oldText is matched against the
//! original file (overlap is an error). N flat calls are N atomic edits: each is matched against
//! the file as the previous call left it, a failed call changes nothing and the calls before it
//! stay applied; every call reports its own result, so the model sees exactly which landed.

use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, Value,
};
use oxa_tools::descriptions::{default_edit, errors};
use oxa_tools::EditTool;
use serde_json::json;
use std::path::Path;

struct NoBash;

impl Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NO_SANDBOX, "edit tests run no commands"))
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

fn run_tool(cwd: &Path, args: &Value) -> ToolOutput {
    let sandbox = NoBash;
    let env: Vec<(String, String)> = Vec::new();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        cwd,
        spill_dir: cwd,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &env,
        tune: Default::default(),
    };
    EditTool::new().run(args, &cx, &mut |_: &str| {})
}

/// Parse `raw` the way the core parses `args_raw` (the engine's JSON arguments).
fn args(raw: &str) -> Value {
    serde_json::from_str(raw).expect("valid JSON arguments")
}

const TRICKY_OLD: &str = "let s = \"a \\\"q\\\"\";\nlet t = '</parameter>';\n";
const TRICKY_NEW: &str = "let s = r#\"C:\\path\\to\" \"#;\n\tlet t = \"</parameter>\\n\";\n";

#[test]
fn flat_form_with_quotes_backslashes_newlines_and_closing_tags() {
    let env = TestEnv::new();
    let body = format!("fn main() {{\n{TRICKY_OLD}}}\n");
    env.write("work/a.rs", &body);
    // The arguments as the engine hands them over: a JSON object of three strings.
    let raw = serde_json::to_string(&json!({"path": "a.rs", "oldText": TRICKY_OLD, "newText": TRICKY_NEW}))
        .unwrap();
    let out = run_tool(&env.work(), &args(&raw));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, errors::edit_ok(1, "a.rs"));
    let got = std::fs::read_to_string(env.work().join("a.rs")).unwrap();
    assert_eq!(got, format!("fn main() {{\n{TRICKY_NEW}}}\n"));
    // A literal `</parameter>` line in the replacement is plain text to oxa.
    let raw = r#"{"path":"a.rs","oldText":"\tlet t","newText":"\n</parameter>\n\tlet t"}"#;
    let out = run_tool(&env.work(), &args(raw));
    assert!(!out.is_error, "{}", out.text);
    assert!(std::fs::read_to_string(env.work().join("a.rs"))
        .unwrap()
        .contains("\n</parameter>\n\tlet t"));
}

#[test]
fn result_text_and_file_are_identical_in_both_forms() {
    let a = TestEnv::new();
    let b = TestEnv::new();
    let body = "one\r\ntwo \"x\" \\ y\r\nthree\r\n";
    a.write("work/f.txt", &format!("\u{feff}{body}"));
    b.write("work/f.txt", &format!("\u{feff}{body}"));
    let pi = run_tool(
        &a.work(),
        &json!({"path": "f.txt", "edits": [{"oldText": "two \"x\" \\ y\n", "newText": "2\n"}]}),
    );
    let flat = run_tool(
        &b.work(),
        &json!({"path": "f.txt", "oldText": "two \"x\" \\ y\n", "newText": "2\n"}),
    );
    assert_eq!(pi, flat);
    assert!(!flat.is_error, "{}", flat.text);
    let fa = std::fs::read(a.work().join("f.txt")).unwrap();
    let fb = std::fs::read(b.work().join("f.txt")).unwrap();
    assert_eq!(fa, fb);
    assert_eq!(String::from_utf8(fb).unwrap(), "\u{feff}one\r\n2\r\nthree\r\n");
}

#[test]
fn error_texts_per_call_match_in_both_forms() {
    let env = TestEnv::new();
    env.write("work/d.txt", "x\nx\ny\n");
    let cases = [
        (
            json!({"path": "d.txt", "oldText": "zzz", "newText": "q"}),
            errors::edit_not_found("d.txt", 0, 1),
        ),
        (
            json!({"path": "d.txt", "oldText": "x", "newText": "q"}),
            default_edit::not_unique("d.txt", &[1, 2]),
        ),
        (
            json!({"path": "d.txt", "oldText": "", "newText": "q"}),
            errors::edit_empty_old_text("d.txt", 0, 1),
        ),
        (
            json!({"path": "d.txt", "oldText": "y", "newText": "y"}),
            errors::edit_no_change("d.txt", 1),
        ),
    ];
    for (flat, want) in cases {
        let out = run_tool(&env.work(), &flat);
        assert!(out.is_error);
        assert_eq!(out.text, want);
        // The same edit in the `edits[]` form gives the same text.
        let pi =
            json!({"path": "d.txt", "edits": [{"oldText": flat["oldText"], "newText": flat["newText"]}]});
        assert_eq!(run_tool(&env.work(), &pi), out);
    }
    assert_eq!(
        std::fs::read_to_string(env.work().join("d.txt")).unwrap(),
        "x\nx\ny\n"
    );
    // A flat call without newText has no edit: pi's "no edits" text.
    let out = run_tool(&env.work(), &json!({"path": "d.txt", "oldText": "y"}));
    assert_eq!(out.text, errors::EDIT_NO_EDITS);
}

#[test]
fn several_disjoint_edits_map_to_several_flat_calls() {
    let a = TestEnv::new();
    let b = TestEnv::new();
    let body = "alpha\nbeta\ngamma\ndelta\n";
    a.write("work/m.txt", body);
    b.write("work/m.txt", body);
    let pi = run_tool(
        &a.work(),
        &json!({"path": "m.txt", "edits": [{"oldText": "alpha", "newText": "A"}, {"oldText": "gamma", "newText": "G"}]}),
    );
    assert!(!pi.is_error, "{}", pi.text);
    for (old, new) in [("alpha", "A"), ("gamma", "G")] {
        let out = run_tool(
            &b.work(),
            &json!({"path": "m.txt", "oldText": old, "newText": new}),
        );
        assert!(!out.is_error, "{}", out.text);
    }
    assert_eq!(
        std::fs::read_to_string(a.work().join("m.txt")).unwrap(),
        std::fs::read_to_string(b.work().join("m.txt")).unwrap()
    );
}

#[test]
fn sequential_flat_calls_see_the_previous_result() {
    // The difference in atomicity: the second call matches the file as the first left it, so a
    // change that builds on the first one works, and one that targets replaced text reports
    // pi's not-found text while the first change stays applied.
    let env = TestEnv::new();
    env.write("work/s.txt", "a b c\n");
    assert!(
        !run_tool(
            &env.work(),
            &json!({"path": "s.txt", "oldText": "b", "newText": "BB"})
        )
        .is_error
    );
    assert!(
        !run_tool(
            &env.work(),
            &json!({"path": "s.txt", "oldText": "a BB", "newText": "x"})
        )
        .is_error
    );
    let out = run_tool(
        &env.work(),
        &json!({"path": "s.txt", "oldText": "b", "newText": "y"}),
    );
    assert_eq!(out.text, errors::edit_not_found("s.txt", 0, 1));
    assert_eq!(
        std::fs::read_to_string(env.work().join("s.txt")).unwrap(),
        "x c\n"
    );
}

#[test]
fn pis_legacy_inputs_still_work() {
    let env = TestEnv::new();
    env.write("work/l.txt", "1\n2\n3\n");
    // `edits` as a JSON string and top-level oldText/newText appended as the last edit.
    let out = run_tool(
        &env.work(),
        &json!({"path": "l.txt", "edits": "[{\"oldText\":\"1\",\"newText\":\"one\"}]", "oldText": "3", "newText": "three"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, errors::edit_ok(2, "l.txt"));
    // `edits` as one object.
    let out = run_tool(
        &env.work(),
        &json!({"path": "l.txt", "edits": {"oldText": "2", "newText": "two"}}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        std::fs::read_to_string(env.work().join("l.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
}
