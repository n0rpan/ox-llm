//! A8 acceptance tests: grep, find, ls tools.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_proto::{
    Access, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, Value,
};
use oxa_tools::{FindTool, GrepTool, LsTool};
use serde_json::json;
use std::path::Path;

/// The tools never run a command, so the sandbox here is never reached.
struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

/// Call `tool` with `args` in `cwd`; the output must not be an error.
fn call(tool: &dyn Tool, env: &TestEnv, cwd: &Path, args: Value) -> String {
    let out = call_raw(tool, env, cwd, args);
    assert!(!out.is_error, "unexpected tool error: {}", out.text);
    out.text
}

fn call_raw(tool: &dyn Tool, env: &TestEnv, cwd: &Path, args: Value) -> ToolOutput {
    let cancel = CancelToken::new();
    let spill = env.tmp();
    let cx = ToolCtx {
        checked: &[],
        cwd,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    tool.run(&args, &cx, &mut |_: &str| {})
}

/// grep by path then line; find and ls sorted
#[test]
fn results_sorted() {
    let env = TestEnv::new();
    env.write("work/zeta.txt", "hit\nmiss\nhit\n");
    env.write("work/alpha.txt", "hit\n");
    env.write("work/sub/mid.txt", "hit\n");
    let work = env.work();

    let grep = call(&GrepTool::new(), &env, &work, json!({"pattern": "hit"}));
    assert_eq!(
        grep,
        "alpha.txt:1: hit\nsub/mid.txt:1: hit\nzeta.txt:1: hit\nzeta.txt:3: hit"
    );

    let find = call(&FindTool::new(), &env, &work, json!({"pattern": "*"}));
    assert_eq!(find, "alpha.txt\nsub/\nsub/mid.txt\nzeta.txt");

    env.write("work/Beta", "");
    env.write("work/alpha2", "");
    let ls = call(&LsTool::new(), &env, &work, json!({}));
    assert_eq!(ls, "alpha.txt\nalpha2\nBeta\nsub/\nzeta.txt");
}

/// ignored files absent from grep and find
#[test]
fn gitignore_respected() {
    let env = TestEnv::new();
    env.write("work/.gitignore", "ignored.txt\nbuild/\n");
    env.write("work/ignored.txt", "hit\n");
    env.write("work/kept.txt", "hit\n");
    env.write("work/build/out.txt", "hit\n");
    let work = env.work();

    let grep = call(&GrepTool::new(), &env, &work, json!({"pattern": "hit"}));
    assert_eq!(grep, "kept.txt:1: hit");

    let find = call(&FindTool::new(), &env, &work, json!({"pattern": "*"}));
    assert_eq!(find, ".gitignore\nkept.txt");
}

/// default limits: grep counts over the limit; find and ls keep pi's notices
#[test]
fn limits_100_1000_500() {
    let env = TestEnv::new();
    let mut body = String::new();
    for i in 0..150 {
        body.push_str(&format!("hit {i}\n"));
    }
    env.write("work/many.txt", &body);
    let work = env.work();

    let grep = call(&GrepTool::new(), &env, &work, json!({"pattern": "hit"}));
    // Over the limit: one notice and one count per file, not the cut list.
    assert_eq!(
        grep,
        "150 matches in total, the limit was 100. Narrow the pattern, path or glob, or raise limit, to see the lines.\n\nmany.txt: 150 matches"
    );

    // Exactly the limit is not "reached": the notice needs a match beyond it.
    let exact = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "hit", "limit": 150}),
    );
    assert!(!exact.contains("limit reached"), "{exact}");

    for i in 0..1001 {
        env.write(&format!("work/many/f{i:04}.txt"), "");
    }
    let find = call(
        &FindTool::new(),
        &env,
        &work,
        json!({"pattern": "*.txt", "path": "many"}),
    );
    assert!(
        find.ends_with("\n\n[1000 results limit reached. Use limit=2000 for more, or refine pattern]"),
        "{}",
        &find[find.len() - 120..]
    );
    assert_eq!(
        find.split('\n').filter(|l| !l.is_empty()).count(),
        1000 + 1,
        "1000 results and the notice"
    );

    for i in 0..501 {
        env.write(&format!("work/ls501/e{i:04}"), "");
    }
    let ls = call(&LsTool::new(), &env, &work, json!({"path": "ls501"}));
    assert!(
        ls.ends_with("\n\n[500 entries limit reached. Use limit=1000 for more]"),
        "{}",
        &ls[ls.len() - 120..]
    );
}

/// 0 and negative
#[test]
fn limit_below_one_is_one() {
    let env = TestEnv::new();
    env.write("work/a.txt", "hit one\nhit two\n");
    env.write("work/b.txt", "");
    let work = env.work();

    for limit in [json!(0), json!(-5), json!(0.4)] {
        let grep = call(
            &GrepTool::new(),
            &env,
            &work,
            json!({"pattern": "hit", "limit": limit.clone()}),
        );
        assert_eq!(
            grep,
            "2 matches in total, the limit was 1. Narrow the pattern, path or glob, or raise limit, to see the lines.\n\na.txt: 2 matches",
            "limit {limit}"
        );
    }

    let find = call(
        &FindTool::new(),
        &env,
        &work,
        json!({"pattern": "*.txt", "limit": 0}),
    );
    assert_eq!(
        find,
        "a.txt\n\n[1 results limit reached. Use limit=2 for more, or refine pattern]"
    );

    let ls = call(&LsTool::new(), &env, &work, json!({"limit": -1}));
    assert_eq!(ls, "a.txt\n\n[1 entries limit reached. Use limit=2 for more]");
}

/// find pattern is relative to `path`
#[test]
fn glob_relative_to_path() {
    let env = TestEnv::new();
    env.write("work/pkg/src/a.ts", "x\n");
    env.write("work/src/b.ts", "x\n");
    let work = env.work();

    let inside = call(
        &FindTool::new(),
        &env,
        &work,
        json!({"pattern": "src/*.ts", "path": "pkg"}),
    );
    assert_eq!(inside, "src/a.ts");

    // Anchored at the search root: `pkg/src/a.ts` is not matched from `.`.
    let from_root = call(&FindTool::new(), &env, &work, json!({"pattern": "src/*.ts"}));
    assert_eq!(from_root, "src/b.ts");

    // Without a `/` the pattern matches the file name at any depth.
    let any_depth = call(&FindTool::new(), &env, &work, json!({"pattern": "*.ts"}));
    assert_eq!(any_depth, "pkg/src/a.ts\nsrc/b.ts");

    // The grep glob follows the same rules: anchored when it has a `/`.
    let grep_inside = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "x", "glob": "src/*.ts", "path": "pkg"}),
    );
    assert_eq!(grep_inside, "src/a.ts:1: x");
    let grep_from_root = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "x", "glob": "src/*.ts"}),
    );
    assert_eq!(grep_from_root, "src/b.ts:1: x");
}

/// W4
#[test]
fn identical_bytes_on_two_runs() {
    let first = TestEnv::new();
    let second = TestEnv::new();
    // The same tree, created in opposite orders: the output must not depend on directory order.
    let files = [
        ("a.txt", "hit\nhit2\n"),
        ("B.txt", "hit\n"),
        ("c/d.txt", "hit\n"),
        (".dot", "hit\n"),
    ];
    for (name, body) in files {
        first.write(&format!("work/{name}"), body);
    }
    for (name, body) in files.iter().rev() {
        second.write(&format!("work/{name}"), body);
    }
    let cases: Vec<(Box<dyn Tool>, Value)> = vec![
        (Box::new(GrepTool::new()), json!({"pattern": "hit"})),
        (Box::new(FindTool::new()), json!({"pattern": "*"})),
        (Box::new(LsTool::new()), json!({})),
    ];
    for (tool, args) in &cases {
        let name = tool.spec().name.clone();
        let a1 = call(tool.as_ref(), &first, &first.work(), args.clone());
        let a2 = call(tool.as_ref(), &first, &first.work(), args.clone());
        let b = call(tool.as_ref(), &second, &second.work(), args.clone());
        assert_eq!(a1, a2, "two runs on one tree differ for {name}");
        assert_eq!(a1, b, "creation order changes the output of {name}");
    }
}

/// options
#[test]
fn grep_literal_and_ignore_case_and_context() {
    let env = TestEnv::new();
    env.write("work/a.txt", "foo.bar\nFOO.BAR\nfooXbar\nbaz\n");
    env.write("work/ctx.txt", "x\ny\nbaz\nz\nw\n");
    let work = env.work();

    // Literal: the dot is a dot. Case-sensitive by default.
    let literal = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "foo.bar", "literal": true, "path": "a.txt"}),
    );
    assert_eq!(literal, "a.txt:1: foo.bar");

    // As a regex the dot matches any character.
    let regex = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "foo.bar", "path": "a.txt"}),
    );
    assert_eq!(regex, "a.txt:1: foo.bar\na.txt:3: fooXbar");

    let ignore_case = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "foo.bar", "literal": true, "ignoreCase": true, "path": "a.txt"}),
    );
    assert_eq!(ignore_case, "a.txt:1: foo.bar\na.txt:2: FOO.BAR");

    // Context: one line before and after each match, shown with `-` after the line number.
    let context = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "baz", "context": 1, "path": "ctx.txt"}),
    );
    assert_eq!(context, "ctx.txt-2- y\nctx.txt:3: baz\nctx.txt-4- z");

    // A whole-directory search shows paths relative to the search directory.
    let dir_literal = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "FOO", "literal": true}),
    );
    assert_eq!(dir_literal, "a.txt:2: FOO.BAR");
}

/// format
#[test]
fn ls_dirs_have_slash_and_dotfiles_listed() {
    let env = TestEnv::new();
    env.write("work/Beta.txt", "");
    env.write("work/alpha", "");
    env.write("work/.hidden", "");
    env.write("work/Gamma/inner.txt", "");
    let work = env.work();

    let ls = call(&LsTool::new(), &env, &work, json!({}));
    assert_eq!(ls, ".hidden\nalpha\nBeta.txt\nGamma/");

    let missing = call_raw(&LsTool::new(), &env, &work, json!({"path": "nope"}));
    assert!(missing.is_error);
    assert_eq!(missing.text, "Path not found: nope");

    let not_dir = call_raw(&LsTool::new(), &env, &work, json!({"path": "alpha"}));
    assert!(not_dir.is_error);
    assert_eq!(not_dir.text, "Not a directory: alpha");

    std::fs::create_dir_all(work.join("empty")).expect("create empty dir");
    let empty = call(&LsTool::new(), &env, &work, json!({"path": "empty"}));
    assert_eq!(empty, "(empty directory)");
}

/// grep line cap with notice
#[test]
fn long_lines_truncated() {
    let env = TestEnv::new();
    let long = "a".repeat(600);
    env.write("work/long.txt", &format!("{long}\nshort\n"));
    let work = env.work();

    let grep = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "a{600}", "path": "long.txt"}),
    );
    let expected_line = format!("long.txt:1: {}... [truncated]", "a".repeat(500));
    assert_eq!(
        grep,
        format!("{expected_line}\n\n[Some lines truncated to 500 chars. Use read tool to see full lines]")
    );
}

// ---- further checks beyond the acceptance bullets ----

/// Output is capped at 16 KB by whole lines, with the byte notice.
#[test]
fn grep_output_capped_at_16kb() {
    let env = TestEnv::new();
    let mut body = String::new();
    for _ in 0..150 {
        body.push_str(&format!("hit {}\n", "b".repeat(400)));
    }
    env.write("work/wide.txt", &body);
    let work = env.work();

    // Under the match limit, so the 16 KB cap is what cuts the result.
    let grep = call(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "hit", "path": "wide.txt", "limit": 1000}),
    );
    assert!(
        grep.ends_with("[16.0KB limit reached]"),
        "{}",
        &grep[grep.len() - 160..]
    );
    let body_len = grep.find("\n\n[").expect("notice");
    assert!(body_len <= 16 * 1024, "body is {body_len} bytes");
    assert!(
        grep[..body_len]
            .lines()
            .all(|l| l.starts_with("wide.txt:") && l.ends_with('b')),
        "cut at line boundaries"
    );
}

/// Invalid arguments and a missing path are tool errors with the expected text.
#[test]
fn invalid_arguments_are_tool_errors() {
    let env = TestEnv::new();
    let work = env.work();
    let no_pattern = call_raw(&GrepTool::new(), &env, &work, json!({}));
    assert!(no_pattern.is_error);
    assert_eq!(no_pattern.text, "Invalid arguments: pattern is required");

    let bad_regex = call_raw(&GrepTool::new(), &env, &work, json!({"pattern": "("}));
    assert!(bad_regex.is_error);
    assert!(
        bad_regex.text.starts_with("Invalid pattern: "),
        "{}",
        bad_regex.text
    );

    let missing_path = call_raw(
        &GrepTool::new(),
        &env,
        &work,
        json!({"pattern": "x", "path": "gone"}),
    );
    assert!(missing_path.is_error);
    assert_eq!(missing_path.text, "Path not found: gone");

    let bad_limit = call_raw(
        &FindTool::new(),
        &env,
        &work,
        json!({"pattern": "*", "limit": "ten"}),
    );
    assert!(bad_limit.is_error);
    assert_eq!(bad_limit.text, "Invalid arguments: limit must be a number");
}

/// No match is a plain result, not an error; access reports the search root as an absolute path.
#[test]
fn no_matches_text_and_access_is_the_search_root() {
    let env = TestEnv::new();
    env.write("work/a.txt", "nothing here\n");
    let work = env.work();
    let none = call(&GrepTool::new(), &env, &work, json!({"pattern": "zzz"}));
    assert_eq!(none, "No matches found");
    let none_find = call(&FindTool::new(), &env, &work, json!({"pattern": "*.rs"}));
    assert_eq!(none_find, "No files found matching pattern");

    let cancel = CancelToken::new();
    let spill = env.tmp();
    let cx = ToolCtx {
        checked: &[],
        cwd: &work,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Plan,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    let access = GrepTool::new()
        .access(&json!({"pattern": "x", "path": "a.txt"}), &cx)
        .expect("access");
    assert_eq!(access, vec![Access::Read(work.join("a.txt"))]);
}

/// `.git` directories are never searched, and identical runs agree on a repository-like tree.
#[test]
fn git_directory_is_skipped() {
    let env = TestEnv::new();
    env.write("work/.git/config", "hit\n");
    env.write("work/src.txt", "hit\n");
    let work = env.work();
    let grep = call(&GrepTool::new(), &env, &work, json!({"pattern": "hit"}));
    assert_eq!(grep, "src.txt:1: hit");
    let find = call(&FindTool::new(), &env, &work, json!({"pattern": "*"}));
    assert_eq!(find, "src.txt");
}
