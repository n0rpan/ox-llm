//! A7 acceptance tests: edit tool and edit-diff port.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, Value,
};
use oxa_tools::{apply_edits, detect_line_ending, strip_bom, unified_diff, Edit, EditTool};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A sandbox that never runs anything: the edit tool does not use it.
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

fn edit(old: &str, new: &str) -> Edit {
    Edit {
        old_text: old.to_string(),
        new_text: new.to_string(),
    }
}

/// Run the tool with `args` in `cwd`, with a fresh context.
fn run_tool(cwd: &Path, args: &Value) -> ToolOutput {
    run_tool_with_cancel(cwd, args, &CancelToken::new())
}

fn run_tool_with_cancel(cwd: &Path, args: &Value, cancel: &CancelToken) -> ToolOutput {
    let sandbox = NoBash;
    let env: Vec<(String, String)> = Vec::new();
    let cx = ToolCtx {
        cwd,
        spill_dir: cwd,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel,
        env: &env,
        tune: Default::default(),
    };
    EditTool::new().run(args, &cx, &mut |_: &str| {})
}

/// The content of `rel` below the test root, as bytes-exact text.
fn read(env: &TestEnv, rel: &str) -> String {
    std::fs::read_to_string(env.root().join(rel)).expect("read back")
}

/// the cases of pi's edit tests (exact, fuzzy whitespace, smart quotes)
#[test]
fn pi_edit_cases_ported() {
    // exact
    let out = apply_edits("Hello, world!", &[edit("world", "testing")], "f.txt").unwrap();
    assert_eq!(out.content, "Hello, testing!");

    // fuzzy: trailing whitespace in the file, none in oldText
    let out = apply_edits(
        "line one   \nline two  \nline three\n",
        &[edit("line one\nline two\n", "replaced\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "replaced\nline three\n");

    // fuzzy: fullwidth punctuation in the file, ASCII in oldText
    let out = apply_edits(
        "你好，世界\n你好（世界）\n",
        &[edit("你好,世界\n你好(世界)\n", "你好，pi\n你好(pi)\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "你好，pi\n你好(pi)\n");

    // smart single quotes
    let out = apply_edits(
        "console.log(\u{2018}hello\u{2019});\n",
        &[edit("console.log('hello');", "console.log('world');")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "console.log('world');\n");

    // smart double quotes
    let out = apply_edits(
        "const msg = \u{201C}Hello World\u{201D};\n",
        &[edit("const msg = \"Hello World\";", "const msg = \"Goodbye\";")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "const msg = \"Goodbye\";\n");

    // unicode dashes
    let out = apply_edits(
        "range: 1\u{2013}5\nbreak\u{2014}here\n",
        &[edit("range: 1-5\nbreak-here", "range: 10-50\nbreak--here")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "range: 10-50\nbreak--here\n");

    // non-breaking space
    let out = apply_edits(
        "hello\u{00A0}world\n",
        &[edit("hello world", "hello universe")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "hello universe\n");

    // an exact match wins over a fuzzy one
    let out = apply_edits(
        "const x = 'exact';\nconst y = 'other';\n",
        &[edit("const x = 'exact';", "const x = 'changed';")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "const x = 'changed';\nconst y = 'other';\n");

    // a fuzzy match that still finds nothing is not found
    let err = apply_edits(
        "completely different content\n",
        &[edit("this does not exist", "x")],
        "f.txt",
    )
    .unwrap_err();
    assert!(err.starts_with("Could not find the exact text"), "{err}");

    // two lines equal after trailing whitespace is a duplicate
    let err = apply_edits(
        "hello world   \nhello world\n",
        &[edit("hello world", "replaced")],
        "f.txt",
    )
    .unwrap_err();
    assert!(err.starts_with("Found 2 occurrences"), "{err}");

    // fuzzy in multi-edit mode
    let out = apply_edits(
        "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n",
        &[
            edit("console.log('hello');\n", "console.log('world');\n"),
            edit("hello world\n", "hello universe\n"),
        ],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "console.log('world');\nhello universe\n");

    // the replacement of a fuzzy match does not touch an identical neighbouring line
    let original = "replace me   \nafter   \n";
    let out = apply_edits(original, &[edit("replace me\n", "after\n")], "f.txt").unwrap();
    assert_eq!(out.content, "after\nafter   \n");

    // multi-edit fuzzy: untouched lines keep their trailing spaces
    let original = "keep before  \nfirst target  \nfirst after\nkeep middle   \nsecond target  \nsecond after\nkeep after  \n";
    let out = apply_edits(
        original,
        &[
            edit("first target\nfirst after", "FIRST\nFIRST2"),
            edit("second target\nsecond after", "SECOND\nSECOND2"),
        ],
        "f.txt",
    )
    .unwrap();
    assert_eq!(
        out.content,
        "keep before  \nFIRST\nFIRST2\nkeep middle   \nSECOND\nSECOND2\nkeep after  \n"
    );
}

/// pi's text with the occurrence count
#[test]
fn non_unique_error_text() {
    let err = apply_edits("foo foo foo", &[edit("foo", "bar")], "f.txt").unwrap_err();
    assert_eq!(
        err,
        "Found 3 occurrences of the text in f.txt. The text must be unique. Please provide more context to make it unique."
    );
    let err = apply_edits("a\nb\na\n", &[edit("b\n", "B\n"), edit("a\n", "A\n")], "f.txt").unwrap_err();
    assert_eq!(
        err,
        "Found 2 occurrences of edits[1] in f.txt. Each oldText must be unique. Please provide more context to make it unique."
    );
}

/// pi's text
#[test]
fn not_found_error_text() {
    let err = apply_edits("hello\n", &[edit("nope", "x")], "f.txt").unwrap_err();
    assert_eq!(
        err,
        "Could not find the exact text in f.txt. The old text must match exactly including all whitespace and newlines."
    );
    let err = apply_edits("hello\n", &[edit("hello\n", "H\n"), edit("nope", "x")], "f.txt").unwrap_err();
    assert_eq!(
        err,
        "Could not find edits[1] in f.txt. The oldText must match exactly including all whitespace and newlines."
    );
}

/// pi's text
#[test]
fn overlap_error_text() {
    let err = apply_edits(
        "one\ntwo\nthree\n",
        &[
            edit("one\ntwo\n", "ONE\nTWO\n"),
            edit("two\nthree\n", "TWO\nTHREE\n"),
        ],
        "f.txt",
    )
    .unwrap_err();
    assert_eq!(
        err,
        "edits[0] and edits[1] overlap in f.txt. Merge them into one edit or target disjoint regions."
    );
}

/// file with BOM keeps it
#[test]
fn bom_preserved() {
    let out = apply_edits(
        "\u{feff}first\r\nsecond\r\nthird\r\n",
        &[edit("second\n", "REPLACED\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "\u{feff}first\r\nREPLACED\r\nthird\r\n");

    let env = TestEnv::new();
    env.write("work/bom.txt", "\u{feff}first\nsecond\n");
    let out = run_tool(
        &env.work(),
        &json!({"path": "bom.txt", "edits": [{"oldText": "second\n", "newText": "2\n"}]}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(read(&env, "work/bom.txt"), "\u{feff}first\n2\n");
}

/// CRLF file stays CRLF
#[test]
fn crlf_preserved() {
    let out = apply_edits(
        "first\r\nsecond\r\nthird\r\n",
        &[edit("second\n", "REPLACED\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "first\r\nREPLACED\r\nthird\r\n");

    // inserted lines take the CRLF of the line they replace
    let out = apply_edits(
        "first\r\nsecond\r\nthird",
        &[edit("second\nthird", "2a\r\n2b\nthird!")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "first\r\n2a\r\n2b\r\nthird!");

    let out = apply_edits(
        "first\nsecond\nthird",
        &[edit("second\nthird", "2a\r\n2b\nthird!")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "first\n2a\n2b\nthird!");
}

/// only edited lines change
#[test]
fn mixed_endings_untouched_lines_kept() {
    // first line LF, second CRLF
    let out = apply_edits("a\nb\r\nX = 1\r\n", &[edit("X = 1", "X = 2")], "f.txt").unwrap();
    assert_eq!(out.content, "a\nb\r\nX = 2\r\n");

    // first line CRLF, last line LF
    let out = apply_edits("a\r\nb\nc", &[edit("c", "C")], "f.txt").unwrap();
    assert_eq!(out.content, "a\r\nb\nC");

    // a lone CR in an untouched line survives
    let out = apply_edits("a\rb\nX = 1\n", &[edit("X = 1", "X = 2")], "f.txt").unwrap();
    assert_eq!(out.content, "a\rb\nX = 2\n");

    // multi-edit: each inserted line takes the ending of the line it replaces
    let out = apply_edits(
        "lf1\nlf2\ncr1\r\ncr2\r\n",
        &[edit("lf1\n", "A\nB\n"), edit("cr1\n", "C\nD\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "A\nB\nlf2\nC\r\nD\r\ncr2\r\n");

    // fuzzy widening rewrites only the touched line
    let out = apply_edits("a\r\nfoo   \r\nb\n", &[edit("foo\n", "FOO\n")], "f.txt").unwrap();
    assert_eq!(out.content, "a\r\nFOO\r\nb\n");

    // a kept lone CR and an inserted LF do not merge into one CRLF
    let out = apply_edits("a\rb\nc", &[edit("b", "\nB")], "f.txt").unwrap();
    assert_eq!(out.content, "a\r\n\nB\nc");
    let out = apply_edits("a\rb\nc", &[edit("b", "")], "f.txt").unwrap();
    assert_eq!(out.content, "a\r\n\nc");
    assert_eq!(out.diff, unified_diff("a\nb\nc", "a\n\nc", 4));
}

/// not sequential
#[test]
fn multi_edit_matches_against_original() {
    let out = apply_edits(
        "foo\nbar\nbaz\n",
        &[edit("foo\n", "foo bar\n"), edit("bar\n", "BAR\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "foo bar\nBAR\nbaz\n");
}

/// line numbers and context
#[test]
fn diff_format_matches_pi() {
    let out = apply_edits(
        "alpha\nbeta\ngamma\ndelta\n",
        &[edit("alpha\n", "ALPHA\n"), edit("gamma\n", "GAMMA\n")],
        "f.txt",
    )
    .unwrap();
    assert_eq!(out.content, "ALPHA\nbeta\nGAMMA\ndelta\n");
    assert_eq!(out.first_changed_line, 1);
    assert_eq!(
        out.diff,
        "-1 alpha\n+1 ALPHA\n 2 beta\n-3 gamma\n+3 GAMMA\n 4 delta"
    );

    // A change further down: first changed line is its number in the new file.
    let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
    let out = apply_edits(&old, &[edit("l10\n", "L10\n")], "f.txt").unwrap();
    assert_eq!(out.first_changed_line, 10);
    // Four context lines each side (pi's default), width 2.
    assert_eq!(
        out.diff,
        "    ...\n  6 l6\n  7 l7\n  8 l8\n  9 l9\n-10 l10\n+10 L10\n 11 l11\n 12 l12\n 13 l13\n 14 l14\n    ..."
    );

    // Long unchanged gaps between edits collapse to a marker.
    let lines: Vec<String> = (1..=600).map(|i| format!("line {i:03}\n")).collect();
    let big = lines.concat();
    let out = apply_edits(
        &big,
        &[
            edit("line 100\n", "LINE 100\n"),
            edit("line 300\n", "LINE 300\n"),
            edit("line 500\n", "LINE 500\n"),
        ],
        "f.txt",
    )
    .unwrap();
    assert!(out.diff.contains("LINE 100") && out.diff.contains("LINE 300") && out.diff.contains("LINE 500"));
    assert!(out.diff.contains("..."));
    assert!(!out.diff.contains("line 250"));
    assert!(out.diff.split('\n').count() < 50);
}

// ---------------------------------------------------------------------------------------------
// Extra coverage: helpers, error texts, and the tool end to end.

#[test]
fn strip_bom_and_line_ending_helpers() {
    assert_eq!(strip_bom("\u{feff}x"), ("\u{feff}", "x"));
    assert_eq!(strip_bom("x"), ("", "x"));
    assert_eq!(detect_line_ending("a\r\nb\n"), "\r\n");
    assert_eq!(detect_line_ending("a\nb\r\n"), "\n");
    assert_eq!(detect_line_ending("plain"), "\n");
}

#[test]
fn no_change_error_text() {
    let err = apply_edits("same\n", &[edit("same", "same")], "f.txt").unwrap_err();
    assert_eq!(
        err,
        "No changes made to f.txt. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
    );
    let err = apply_edits("a\nb\n", &[edit("a\n", "a\n"), edit("b\n", "b\n")], "f.txt").unwrap_err();
    assert_eq!(
        err,
        "No changes made to f.txt. The replacements produced identical content."
    );
}

#[test]
fn empty_old_text_error_text() {
    let err = apply_edits("abc", &[edit("", "x")], "f.txt").unwrap_err();
    assert_eq!(err, "oldText must not be empty in f.txt.");
    let err = apply_edits("abc", &[edit("a", "A"), edit("", "x")], "f.txt").unwrap_err();
    assert_eq!(err, "edits[1].oldText must not be empty in f.txt.");
}

#[test]
fn tool_replaces_text_and_reports_diff() {
    let env = TestEnv::new();
    env.write("work/edit-test.txt", "Hello, world!");
    let out = run_tool(
        &env.work(),
        &json!({"path": "edit-test.txt", "edits": [{"oldText": "world", "newText": "testing"}]}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "Successfully replaced 1 block(s) in edit-test.txt.");
    assert_eq!(read(&env, "work/edit-test.txt"), "Hello, testing!");
    assert_eq!(out.ui["firstChangedLine"], json!(1));
    assert_eq!(out.ui["diff"], json!("-1 Hello, world!\n+1 Hello, testing!"));
}

#[test]
fn tool_multi_edit_and_counts() {
    let env = TestEnv::new();
    env.write("work/multi.txt", "alpha\nbeta\ngamma\ndelta\n");
    let out = run_tool(
        &env.work(),
        &json!({"path": "multi.txt", "edits": [{"oldText": "alpha\n", "newText": "ALPHA\n"}, {"oldText": "gamma\n", "newText": "GAMMA\n"}]}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "Successfully replaced 2 block(s) in multi.txt.");
    assert_eq!(read(&env, "work/multi.txt"), "ALPHA\nbeta\nGAMMA\ndelta\n");
}

#[test]
fn tool_legacy_and_string_edit_forms() {
    let env = TestEnv::new();
    env.write("work/legacy.txt", "one\ntwo\nthree\n");
    // top-level oldText/newText
    let out = run_tool(
        &env.work(),
        &json!({"path": "legacy.txt", "oldText": "one\n", "newText": "ONE\n"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(read(&env, "work/legacy.txt"), "ONE\ntwo\nthree\n");

    // edits given as a JSON string holding a single edit object, plus a legacy pair appended
    let args = json!({"path": "legacy.txt", "edits": "{\"oldText\":\"two\\n\",\"newText\":\"TWO\\n\"}", "oldText": "three\n", "newText": "THREE\n"});
    let out = run_tool(&env.work(), &args);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "Successfully replaced 2 block(s) in legacy.txt.");
    assert_eq!(read(&env, "work/legacy.txt"), "ONE\nTWO\nTHREE\n");
}

#[test]
fn tool_error_texts() {
    let env = TestEnv::new();
    env.write("work/a.txt", "hello\n");

    let out = run_tool(&env.work(), &json!({"path": "a.txt", "edits": []}));
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "Edit tool input is invalid. edits must contain at least one replacement."
    );

    let out = run_tool(
        &env.work(),
        &json!({"path": "missing.txt", "edits": [{"oldText": "hello", "newText": "x"}]}),
    );
    assert!(out.is_error);
    assert_eq!(out.text, "Could not edit file: missing.txt. Error code: ENOENT.");

    let out = run_tool(
        &env.work(),
        &json!({"path": "a.txt", "edits": [{"oldText": "nope", "newText": "x"}]}),
    );
    assert!(out.is_error);
    assert!(
        out.text.starts_with("Could not find the exact text in a.txt."),
        "{}",
        out.text
    );

    // a failed edit writes nothing
    assert_eq!(read(&env, "work/a.txt"), "hello\n");

    let out = run_tool(&env.work(), &json!({"edits": [{"oldText": "a", "newText": "b"}]}));
    assert!(out.is_error);
    assert_eq!(out.text, "Edit tool input is invalid. path must be a string.");

    let out = run_tool(
        &env.work(),
        &json!({"path": "a.txt", "edits": [{"oldText": "a"}]}),
    );
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "Edit tool input is invalid. edits[0] must contain string oldText and newText."
    );
}

#[cfg(unix)]
#[test]
fn tool_read_only_file_is_eacces() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let path = env.write("work/ro.txt", "hello\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = run_tool(
        &env.work(),
        &json!({"path": "ro.txt", "edits": [{"oldText": "hello", "newText": "world"}]}),
    );
    // Root can write read-only files; only assert the text when the write check actually fails.
    if out.is_error {
        assert_eq!(out.text, "Could not edit file: ro.txt. Error code: EACCES.");
    } else {
        assert_eq!(read(&env, "work/ro.txt"), "world\n");
    }
}

#[test]
fn tool_cancelled_before_run_changes_nothing() {
    let env = TestEnv::new();
    env.write("work/c.txt", "keep\n");
    let cancel = CancelToken::new();
    cancel.cancel();
    let out = run_tool_with_cancel(
        &env.work(),
        &json!({"path": "c.txt", "edits": [{"oldText": "keep", "newText": "gone"}]}),
        &cancel,
    );
    assert!(out.is_error);
    assert_eq!(out.text, "Operation aborted");
    assert_eq!(read(&env, "work/c.txt"), "keep\n");
}

#[test]
fn access_is_write_of_the_normalised_path() {
    let env = TestEnv::new();
    let sandbox = NoBash;
    let cancel = CancelToken::new();
    let no_env: Vec<(String, String)> = Vec::new();
    let cx = ToolCtx {
        cwd: &env.work(),
        spill_dir: &env.tmp(),
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &no_env,
        tune: Default::default(),
    };
    let tool = EditTool::new();
    let accesses = tool
        .access(
            &json!({"path": "./sub/../a.txt", "edits": [{"oldText": "a", "newText": "b"}]}),
            &cx,
        )
        .unwrap();
    assert_eq!(accesses, vec![oxa_proto::Access::Write(env.work().join("a.txt"))]);
    assert!(tool.access(&json!({"edits": []}), &cx).is_err());
}

#[test]
fn spec_is_pis_edit_text() {
    let spec = EditTool::new().spec().clone();
    assert_eq!(spec.name, "edit");
    assert!(spec.schema_json.contains("oldText"));
}

/// The edit replaces the file (temp + rename) instead of truncating it in place: another hard link
/// to the old inode keeps the old, whole content, so a full disk or a kill mid-write cannot lose it.
#[test]
fn edit_replaces_the_file_atomically_and_keeps_its_mode() {
    let env = TestEnv::new();
    let path = env.work().join("a.txt");
    std::fs::write(&path, "hello world\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let old_inode = env.work().join("old-inode");
    std::fs::hard_link(&path, &old_inode).unwrap();
    let out = run_tool(
        &env.work(),
        &json!({"path": "a.txt", "edits": [{"oldText": "world", "newText": "there"}]}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
    assert_eq!(std::fs::read_to_string(&old_inode).unwrap(), "hello world\n");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    let leftovers: Vec<_> = std::fs::read_dir(env.work())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("oxa-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp file left behind");
}

/// A symlink stays a symlink; its target gets the edit.
#[test]
fn edit_writes_through_a_symlink() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("real.txt"), "one\n").unwrap();
    std::os::unix::fs::symlink("real.txt", env.work().join("link.txt")).unwrap();
    let out = run_tool(
        &env.work(),
        &json!({"path": "link.txt", "edits": [{"oldText": "one", "newText": "two"}]}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(std::fs::symlink_metadata(env.work().join("link.txt"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(env.work().join("real.txt")).unwrap(),
        "two\n"
    );
}

/// A writable file in a directory that cannot take new files is still editable (in place).
#[test]
fn edit_works_in_a_directory_without_write_permission() {
    let env = TestEnv::new();
    let dir = env.work().join("ro");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = run_tool(
        &env.work(),
        &json!({"path": "ro/f.txt", "edits": [{"oldText": "x", "newText": "y"}]}),
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "y\n");
}
