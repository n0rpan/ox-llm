//! V020-OPTIMIZE #2 (TOOL-DIET) and OXA-LOCAL-FIRST T8 for the read-class tools: default-tune
//! bytes are pi's, the diet grep shrinks the result and keeps every matching line, the first
//! page of `read` continues without gap or overlap, and the over-limit grep answer counts.
//! Offline and deterministic: every file is made in a `TestEnv`.

use oxa_fake::TestEnv;
use oxa_proto::{
    CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, ToolTune, Value,
};
use oxa_tools::{GrepTool, ReadTool};
use serde_json::json;
use std::path::Path;

/// The tools in this file never run a command, so the sandbox is never reached.
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

/// Run `tool` in `cwd` with `tune`; the output is returned whatever it is.
fn run(tool: &dyn Tool, env: &TestEnv, cwd: &Path, args: Value, tune: ToolTune) -> ToolOutput {
    let cancel = CancelToken::new();
    let spill = env.tmp();
    let cx = ToolCtx {
        cwd,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune,
    };
    tool.run(&args, &cx, &mut |_: &str| {})
}

/// The text of a call that must succeed.
fn ok(tool: &dyn Tool, env: &TestEnv, args: Value, tune: ToolTune) -> String {
    let out = run(tool, env, &env.work(), args, tune);
    assert!(!out.is_error, "unexpected tool error: {}", out.text);
    out.text
}

/// Profile `local` only.
fn local() -> ToolTune {
    ToolTune {
        local: true,
        ..ToolTune::default()
    }
}

/// TOOL-DIET only.
fn diet() -> ToolTune {
    ToolTune {
        diet: true,
        ..ToolTune::default()
    }
}

/// Path of fixture file `i`, relative to the search root `work/project`.
fn fixture_rel(i: usize) -> String {
    format!("module_{i:02}/src/components/nested/deeper/file_{i:02}.txt")
}

/// The measurement fixture: 30 files at long nested paths, 10 matching lines in each (40 lines,
/// a match on every line with index 2 mod 4).
fn write_tree(env: &TestEnv) {
    for i in 0..30 {
        let body: String = (0..40)
            .map(|k| {
                if k % 4 == 2 {
                    format!("needle item {i:02}-{k:02} in the component\n")
                } else {
                    format!("filler text line {k:02} of file {i:02}\n")
                }
            })
            .collect();
        env.write(&format!("work/project/{}", fixture_rel(i)), &body);
    }
}

#[test]
fn default_tune_grep_bytes_are_pi_bytes() {
    let env = TestEnv::new();
    env.write("work/x.txt", "a\nhit\nb\n");
    env.write("work/y.txt", "hit\n");
    let args = json!({"pattern": "hit", "context": 1});
    let out = ok(&GrepTool::new(), &env, args.clone(), ToolTune::default());
    assert_eq!(out, "x.txt-1- a\nx.txt:2: hit\nx.txt-3- b\ny.txt:1: hit");
    // The local profile changes nothing under the limit.
    assert_eq!(ok(&GrepTool::new(), &env, args, local()), out);
}

#[test]
fn default_tune_read_bytes_are_pi_bytes() {
    let env = TestEnv::new();
    env.write("work/f.txt", "one\ntwo\nthree\n");
    let out = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "f.txt"}),
        ToolTune::default(),
    );
    assert_eq!(out, "one\ntwo\nthree\n");
    // A first-page tune on a file that fits one page changes nothing either.
    let paged = ToolTune {
        read_page_lines: 100,
        local: true,
        ..ToolTune::default()
    };
    assert_eq!(ok(&ReadTool::new(), &env, json!({"path": "f.txt"}), paged), out);
}

#[test]
fn diet_grep_writes_each_path_once_exactly() {
    let env = TestEnv::new();
    env.write("work/x.txt", "a\nhit\nb\n");
    env.write("work/y.txt", "hit\n");
    let out = ok(
        &GrepTool::new(),
        &env,
        json!({"pattern": "hit", "context": 1}),
        diet(),
    );
    assert_eq!(out, "x.txt\n  1- a\n  2: hit\n  3- b\ny.txt\n  1: hit");
}

#[test]
fn grep_diet_is_smaller_and_keeps_every_matching_line() {
    let env = TestEnv::new();
    write_tree(&env);
    let args = |glob: Option<String>| {
        let mut a = json!({"pattern": "needle", "context": 2, "limit": 1000, "path": "project"});
        if let Some(g) = glob {
            a["glob"] = json!(g);
        }
        a
    };
    // Pi's cap (16 KB) cuts the whole tree in both forms, so the byte numbers are measured per
    // module: each call stays under the cap and the sums are the full output.
    let (mut before_total, mut after_total) = (0usize, 0usize);
    for i in 0..30 {
        let glob = format!("module_{i:02}/**");
        let before = ok(
            &GrepTool::new(),
            &env,
            args(Some(glob.clone())),
            ToolTune::default(),
        );
        let after = ok(&GrepTool::new(), &env, args(Some(glob)), diet());
        before_total += before.len();
        after_total += after.len();
        assert!(after.len() < before.len(), "module {i}: diet must be smaller");
        assert!(
            !before.contains("[truncated") && !before.contains("limit reached"),
            "module {i} cut"
        );
        // Every matching line text is in the diet result exactly once, with its line number.
        for k in (2..40).step_by(4) {
            let text = format!("needle item {i:02}-{k:02} in the component");
            assert_eq!(after.matches(&text).count(), 1, "{text}");
            assert_eq!(before.matches(&text).count(), 1, "{text}");
        }
        // The long path is written once per file, not once per line.
        assert_eq!(after.matches(&fixture_rel(i)).count(), 1);
        assert_eq!(before.matches(&fixture_rel(i)).count(), 40);
        // Deterministic: a second run gives the same bytes.
        assert_eq!(
            ok(
                &GrepTool::new(),
                &env,
                args(Some(format!("module_{i:02}/**"))),
                diet()
            ),
            after
        );
    }
    let saved = 100.0 * (before_total - after_total) as f64 / before_total as f64;
    eprintln!(
        "grep, 30 modules x 10 matches, context 2 (per-module calls, uncapped): \
         before {before_total} bytes, after {after_total} bytes, saved {saved:.1}%"
    );
    assert!(after_total < before_total);
    // The whole tree in one call: both are cut by pi's cap, so only the sizes are reported.
    let whole_before = ok(&GrepTool::new(), &env, args(None), ToolTune::default());
    let whole_after = ok(&GrepTool::new(), &env, args(None), diet());
    eprintln!(
        "grep whole tree (one call, capped): before {} bytes, after {} bytes",
        whole_before.len(),
        whole_after.len()
    );
}

#[test]
fn read_first_page_notice_and_continuation() {
    let env = TestEnv::new();
    let body: String = (1..=250).map(|i| format!("line {i}\n")).collect();
    env.write("work/long.txt", &body);
    let paged = ToolTune {
        read_page_lines: 100,
        local: true,
        ..ToolTune::default()
    };
    let first = ok(&ReadTool::new(), &env, json!({"path": "long.txt"}), paged);
    let notice =
        "[Showing lines 1-100 of 251. Use offset=101 to continue. Or use grep to find the part you need.]";
    assert!(first.ends_with(&format!("\n\n{notice}")), "got: {first}");
    let page: Vec<&str> = first.split("\n\n").next().unwrap_or_default().lines().collect();
    assert_eq!(page.len(), 100);
    assert_eq!(page.first(), Some(&"line 1"));
    assert_eq!(page.last(), Some(&"line 100"));

    // Without the local hint the notice is pi's text.
    let no_hint = ToolTune {
        read_page_lines: 100,
        ..ToolTune::default()
    };
    let plain = ok(&ReadTool::new(), &env, json!({"path": "long.txt"}), no_hint);
    assert!(
        plain.ends_with("\n\n[Showing lines 1-100 of 251. Use offset=101 to continue.]"),
        "got: {plain}"
    );

    // The next read, with the offset from the notice, starts at line 101: no gap, no overlap.
    let next = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "long.txt", "offset": 101, "limit": 100}),
        paged,
    );
    let next_page = next.split("\n\n").next().unwrap_or_default();
    let next_lines: Vec<&str> = next_page.lines().collect();
    assert_eq!(next_lines.len(), 100);
    assert_eq!(next_lines.first(), Some(&"line 101"));
    assert_eq!(next_lines.last(), Some(&"line 200"));
    // An offset without a limit is pi's read: the rest of the file, no page rule.
    let rest = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "long.txt", "offset": 101}),
        paged,
    );
    assert!(rest.starts_with("line 101\n"));
    assert!(rest.ends_with("line 250\n"));
    assert!(!rest.contains("Showing lines"));
}

#[test]
fn read_first_page_byte_limit_applies_when_first() {
    let env = TestEnv::new();
    // Ten lines of 19 bytes each.
    let body: String = (1..=10).map(|i| format!("{i:02}{}\n", "x".repeat(17))).collect();
    env.write("work/bytes.txt", &body);
    let tune = ToolTune {
        read_page_lines: 100,
        read_page_bytes: 50,
        local: true,
        ..ToolTune::default()
    };
    let out = ok(&ReadTool::new(), &env, json!({"path": "bytes.txt"}), tune);
    // Two 19-byte lines fit in 50 bytes (19 + 1 + 19 = 39); a third would make 59.
    let two_lines = body.lines().take(2).collect::<Vec<_>>().join("\n");
    assert!(out.starts_with(&format!("{two_lines}\n\n[")), "got: {out}");
    assert!(
        out.contains("Use offset=3 to continue. Or use grep to find the part you need.]"),
        "got: {out}"
    );
    // An explicit offset is pi's read: no hint.
    let explicit = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "bytes.txt", "offset": 1}),
        tune,
    );
    assert!(!explicit.contains("Or use grep"), "got: {explicit}");
}

#[test]
fn grep_over_limit_gives_one_notice_and_counts() {
    let env = TestEnv::new();
    write_tree(&env);
    let args = json!({"pattern": "needle", "context": 2, "path": "project"});
    // Pi's answer keeps the cut list and pi's notice.
    let pi = ok(&GrepTool::new(), &env, args.clone(), ToolTune::default());
    assert!(
        pi.contains("[100 matches limit reached. Use limit=200 for more, or refine pattern."),
        "got tail: {pi}"
    );

    let out = ok(&GrepTool::new(), &env, args, local());
    let (head, rows) = out.split_once("\n\n").expect("notice, blank line, counts");
    assert_eq!(
        head,
        "300 matches in total, the limit was 100. Narrow the pattern, path or glob, or raise limit, to see the lines."
    );
    let rows: Vec<&str> = rows.lines().collect();
    assert_eq!(rows.len(), 30);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(*row, format!("{}: 10 matches", fixture_rel(i)));
    }
}

#[test]
fn grep_under_limit_is_unchanged_by_local() {
    let env = TestEnv::new();
    write_tree(&env);
    let args = json!({"pattern": "item 03-", "limit": 1000});
    let a = ok(&GrepTool::new(), &env, args.clone(), ToolTune::default());
    let b = ok(&GrepTool::new(), &env, args, local());
    assert_eq!(a, b);
}

/// parity-local: a first line that fits pi's cap but not the first page is not refused. The
/// read falls back to pi's read (the 16 KiB cap and pi's notice), so `local` shows every line a
/// plain read shows under `pi`, byte for byte.
#[test]
fn read_first_page_never_refuses_a_line_pi_shows() {
    let env = TestEnv::new();
    let wide = format!("{}\n{}\n{}", "a".repeat(9000), "b".repeat(9000), "c");
    env.write("work/wide.txt", &wide);
    let tune = ToolTune {
        read_page_lines: 400,
        read_page_bytes: 8192,
        local: true,
        ..ToolTune::default()
    };
    let pi = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "wide.txt"}),
        ToolTune::default(),
    );
    let local = ok(&ReadTool::new(), &env, json!({"path": "wide.txt"}), tune);
    assert_eq!(local, pi);
    assert!(
        local.starts_with(&"a".repeat(9000)),
        "the whole first line is shown"
    );
    assert!(
        local.contains("(16.0KB limit). Use offset=2 to continue.]"),
        "got tail: {}",
        &local[9000..]
    );
    // A line over pi's cap is refused by both with pi's text.
    env.write("work/huge.txt", &"z".repeat(20_000));
    let a = run(
        &ReadTool::new(),
        &env,
        &env.work(),
        json!({"path": "huge.txt"}),
        ToolTune::default(),
    );
    let b = run(
        &ReadTool::new(),
        &env,
        &env.work(),
        json!({"path": "huge.txt"}),
        tune,
    );
    assert_eq!(a.text, b.text);
    assert_eq!(a.is_error, b.is_error);
}
