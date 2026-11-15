//! L4 (budget): no single result of a read-class tool is longer than the
//! byte cap the tuning gives (the budget of the working size), and every cut names the way on:
//! the next `offset` for `read`, the narrower search for `grep`, `find` and `ls`. A loop of
//! `read` calls that follows the notices reads a 67,000-byte file in whole, page by page.
//! Offline and deterministic: every file is made in a `TestEnv`.

use oxa_fake::TestEnv;
use oxa_proto::{
    CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, ToolTune, Value,
};
use oxa_tools::{FindTool, GrepTool, LsTool, ReadTool};
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

/// The tuning with the given tool output cap, as `tune_for` makes it: the first page of
/// `read` at most 8 KiB within the cap, every other cut at the cap.
fn tuned(cap: u32) -> ToolTune {
    ToolTune {
        diet: true,
        read_page_lines: 400,
        read_page_bytes: cap.min(8_192),
        bash_timeout_s: 120,
        output_cap_bytes: cap,
    }
}

/// A source-like text of exactly 67,000 bytes: about 900 lines of short Rust functions and a
/// last comment line that makes up the length.
fn source_text() -> String {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < 66_900 {
        s.push_str(&format!(
            "pub fn item_{i:04}(x: u32) -> u32 {{ x.wrapping_mul({i}) + 1 }}\n"
        ));
        i += 1;
    }
    let rest = 67_000 - s.len();
    s.push_str("//");
    s.push_str(&" ".repeat(rest - 3));
    s.push('\n');
    assert_eq!(s.len(), 67_000);
    s
}

/// Split a `read` result into its content and the `offset` its notice names (`None` at the end
/// of the file: no notice).
fn split_page(out: &str) -> (&str, Option<usize>) {
    let Some(i) = out.rfind("\n\n[") else {
        return (out, None);
    };
    let notice = &out[i + 2..];
    let digits: String = notice
        .split("Use offset=")
        .nth(1)
        .unwrap_or_default()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    (
        &out[..i],
        Some(digits.parse().expect("the notice names an offset")),
    )
}

/// Follow the notices from the first page to the end of `path`: every page within `cap`, the
/// lines of all pages in order. Returns the pages' lengths and their lines.
fn read_all(env: &TestEnv, path: &str, cap: u32) -> (Vec<usize>, Vec<String>) {
    let tune = tuned(cap);
    let mut lens = Vec::new();
    let mut lines = Vec::new();
    let mut offset: Option<usize> = None;
    loop {
        let args = match offset {
            None => json!({"path": path}),
            Some(o) => json!({"path": path, "offset": o}),
        };
        let out = ok(&ReadTool::new(), env, args, tune);
        assert!(
            out.len() <= cap as usize,
            "page of {} bytes > cap {cap}",
            out.len()
        );
        lens.push(out.len());
        let (content, next) = split_page(&out);
        lines.extend(content.lines().map(str::to_string));
        match next {
            Some(n) => {
                assert!(n > lines.len(), "the next offset must move forward");
                offset = Some(n);
            }
            None => break,
        }
    }
    (lens, lines)
}

#[test]
fn read_first_page_is_cut_at_the_cap_and_names_the_next_offset() {
    let env = TestEnv::new();
    env.write("work/src.rs", &source_text());
    let out = ok(&ReadTool::new(), &env, json!({"path": "src.rs"}), tuned(6_000));
    eprintln!(
        "read, no offset/limit, 67000 B file, cap 6000: returned {} bytes",
        out.len()
    );
    assert!(out.len() <= 6_000, "got {} bytes", out.len());
    let (content, next) = split_page(&out);
    let shown = content.lines().count();
    assert!(
        content.len() > 5_000,
        "a byte cut near the cap: {}",
        content.len()
    );
    assert!(shown < 400, "a byte cut, not the 400-line page: {shown}");
    assert_eq!(
        next,
        Some(shown + 1),
        "the next offset is the line after the page"
    );
    assert!(
        out.contains("Or use grep to find the part you need."),
        "got: {out}"
    );
}

#[test]
fn read_explicit_huge_limit_is_cut_at_the_cap_with_the_next_offset() {
    let env = TestEnv::new();
    env.write("work/src.rs", &source_text());
    let args = json!({"path": "src.rs", "offset": 1, "limit": 100_000});
    let out = ok(&ReadTool::new(), &env, args, tuned(6_000));
    eprintln!(
        "read, offset 1 limit 100000, cap 6000: returned {} bytes",
        out.len()
    );
    assert!(out.len() <= 6_000, "got {} bytes", out.len());
    let (content, next) = split_page(&out);
    assert!(out.contains("Use offset="), "got: {out}");
    assert_eq!(next, Some(content.lines().count() + 1));
    // The output is the file's own text up to the cut (no shaper cut in the middle).
    assert!(source_text().starts_with(&format!("{content}\n")));
}

#[test]
fn read_pages_follow_the_notices_to_the_end_in_order() {
    let env = TestEnv::new();
    let text = source_text();
    env.write("work/src.rs", &text);
    let file_lines: Vec<&str> = text.lines().collect();
    for cap in [6_000u32, 8_192, 2_048] {
        let (lens, lines) = read_all(&env, "src.rs", cap);
        eprintln!("read loop, cap {cap}: {} pages, sizes {:?}", lens.len(), lens);
        assert!(lens.len() > 5, "the file takes several pages at cap {cap}");
        let got: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(got, file_lines, "cap {cap}: pages concatenate to the file");
    }
}

#[test]
fn read_under_pi_is_pis_cut_not_the_budget() {
    let env = TestEnv::new();
    env.write("work/src.rs", &source_text());
    let out = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "src.rs"}),
        ToolTune::default(),
    );
    // Pi: the 16 KiB cap with pi's notice, no hint, no budget.
    assert!(
        out.len() > 16_000 && out.len() <= 16_384 + 200,
        "got {}",
        out.len()
    );
    assert!(!out.contains("Or use grep"));
}

/// The tree of `v020_tool_diet.rs`: 30 files, 10 matches of `needle` in each (300 in all).
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
        env.write(
            &format!("work/project/module_{i:02}/src/components/nested/deeper/file_{i:02}.txt"),
            &body,
        );
    }
}

#[test]
fn grep_under_the_limit_larger_than_the_cap_is_cut_and_says_how_to_narrow() {
    let env = TestEnv::new();
    write_tree(&env);
    // limit 1000: 300 matches, the normal form, about 17 KB: over a 2 KB cap.
    let args = json!({"pattern": "needle", "context": 2, "limit": 1000, "path": "project"});
    let out = ok(&GrepTool::new(), &env, args, tuned(2_048));
    eprintln!("grep under limit, cap 2048: returned {} bytes", out.len());
    assert!(out.len() <= 2_048, "got {} bytes", out.len());
    assert!(
        out.contains("limit reached. Narrow the search to see the rest]"),
        "got tail: {out}"
    );
}

#[test]
fn grep_over_the_limit_counts_fit_the_cap() {
    let env = TestEnv::new();
    write_tree(&env);
    let args = json!({"pattern": "needle", "context": 2, "path": "project"});
    let out = ok(&GrepTool::new(), &env, args, tuned(2_048));
    eprintln!("grep over limit (counts), cap 2048: returned {} bytes", out.len());
    assert!(out.len() <= 2_048, "got {} bytes", out.len());
    assert!(
        out.starts_with("300 matches in total, the limit was 100."),
        "got: {out}"
    );
}

#[test]
fn find_over_the_cap_is_cut_and_says_how_to_narrow() {
    let env = TestEnv::new();
    write_tree(&env);
    // "*" lists the files and the directories: about 4 KB, over the 2 KB cap.
    let args = json!({"pattern": "*", "limit": 1000, "path": "project"});
    let out = ok(&FindTool::new(), &env, args, tuned(2_048));
    eprintln!("find, cap 2048: returned {} bytes", out.len());
    assert!(out.len() <= 2_048, "got {} bytes", out.len());
    assert!(
        out.contains("Narrow the search to see the rest]"),
        "got tail: {out}"
    );
}

#[test]
fn ls_over_the_cap_is_cut_and_says_how_to_narrow() {
    let env = TestEnv::new();
    for i in 0..300 {
        env.write(&format!("work/many/entry_with_a_long_name_{i:04}.txt"), "x");
    }
    let args = json!({"path": "many", "limit": 1000});
    let out = ok(&LsTool::new(), &env, args, tuned(2_048));
    eprintln!("ls, cap 2048: returned {} bytes", out.len());
    assert!(out.len() <= 2_048, "got {} bytes", out.len());
    assert!(
        out.contains("Narrow the search to see the rest]"),
        "got tail: {out}"
    );
}

/// 600 entries with long names in one directory: about 21 KB as `ls` lists them, more for
/// `find`.
fn write_many(env: &TestEnv) {
    for i in 0..600 {
        env.write(&format!("work/many/entry_with_a_long_name_{i:04}.txt"), "x");
    }
}

#[test]
fn large_window_cuts_only_the_first_read_page_at_8_kib_and_the_rest_at_16_kib() {
    let env = TestEnv::new();
    env.write("work/src.rs", &source_text());
    write_tree(&env);
    write_many(&env);
    let tune = tuned(16_384);
    let first = ok(&ReadTool::new(), &env, json!({"path": "src.rs"}), tune);
    assert!(
        first.len() > 7_000 && first.len() <= 8_192,
        "first page: {}",
        first.len()
    );
    let explicit = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "src.rs", "offset": 1}),
        tune,
    );
    assert!(
        explicit.len() > 15_000 && explicit.len() <= 16_384,
        "explicit read: {}",
        explicit.len()
    );
    assert!(
        explicit.contains("(16.0KB limit). Use offset="),
        "got: {explicit}"
    );
    let limited = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "src.rs", "offset": 1, "limit": 100_000}),
        tune,
    );
    assert_eq!(limited, explicit, "a huge limit changes nothing below the cap");
    let grep = ok(
        &GrepTool::new(),
        &env,
        json!({"pattern": "needle", "context": 2, "limit": 1000, "path": "project"}),
        tune,
    );
    let find = ok(
        &FindTool::new(),
        &env,
        json!({"pattern": "*", "limit": 1000, "path": "many"}),
        tune,
    );
    let ls = ok(&LsTool::new(), &env, json!({"path": "many", "limit": 1000}), tune);
    for (name, out) in [("grep", &grep), ("find", &find), ("ls", &ls)] {
        eprintln!("{name}, cap 16384: returned {} bytes", out.len());
        assert!(out.len() > 15_000 && out.len() <= 16_384, "{name}: {}", out.len());
        assert!(
            out.contains("16.0KB limit reached. Narrow the search to see the rest]"),
            "{name}: {}",
            &out[out.len().saturating_sub(300)..]
        );
    }
}

#[test]
fn without_an_output_cap_the_tools_cut_as_pi_does() {
    // output_cap_bytes 0: grep, find, ls and an explicit read are pi's, byte for byte.
    let env = TestEnv::new();
    env.write("work/src.rs", &source_text());
    write_many(&env);
    let tune = ToolTune {
        output_cap_bytes: 0,
        ..tuned(16_384)
    };
    let pi = ToolTune::default();
    let cases = [
        (
            &ReadTool::new() as &dyn Tool,
            json!({"path": "src.rs", "offset": 1}),
        ),
        (
            &FindTool::new() as &dyn Tool,
            json!({"pattern": "*", "limit": 1000, "path": "many"}),
        ),
        (
            &LsTool::new() as &dyn Tool,
            json!({"path": "many", "limit": 1000}),
        ),
    ];
    for (tool, args) in cases {
        let a = ok(tool, &env, args.clone(), pi);
        let b = ok(tool, &env, args, tune);
        assert_eq!(a, b);
        assert!(!b.contains("Narrow the search"));
    }
}

#[test]
fn a_first_line_only_the_page_refuses_is_read_as_pi_reads_it_under_the_budget() {
    // Parity with main's rule: the page never refuses a line pi shows (9000 bytes > the 8 KiB
    // page, < pi's 16 KiB).
    let env = TestEnv::new();
    env.write(
        "work/wide.txt",
        &format!("{}\n{}\nc", "a".repeat(9000), "b".repeat(9000)),
    );
    let pi = ok(
        &ReadTool::new(),
        &env,
        json!({"path": "wide.txt"}),
        ToolTune::default(),
    );
    let got = ok(&ReadTool::new(), &env, json!({"path": "wide.txt"}), tuned(16_384));
    assert_eq!(got, pi);
    // A first line within the page's cap but not its content room (8000 bytes) is shown too.
    env.write("work/edge.txt", &format!("{}\nb", "a".repeat(8_000)));
    let out = ok(&ReadTool::new(), &env, json!({"path": "edge.txt"}), tuned(16_384));
    assert!(out.starts_with(&"a".repeat(8_000)), "the line is shown");
}

#[test]
fn an_explicit_read_of_a_line_over_the_room_names_the_room_it_had() {
    let env = TestEnv::new();
    env.write("work/long.txt", &"x".repeat(5_900));
    let out = run(
        &ReadTool::new(),
        &env,
        &env.work(),
        json!({"path": "long.txt", "offset": 1}),
        tuned(6_000),
    );
    // 6000 less the 256-byte room for the notice: 5744 bytes, the limit and the head -c.
    assert_eq!(
        out.text,
        "[Line 1 is 5.8KB, exceeds 5.6KB limit. Use bash: sed -n '1p' long.txt | head -c 5744]"
    );
}
