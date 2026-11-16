//! #518: with `ToolCtx::checked` set, `read`, `write` and `edit` open the location the permission
//! check pinned and nothing else. Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_fake::TestEnv;
use oxa_proto::{
    CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, Value,
};
use oxa_tools::{EditTool, ReadTool, WriteTool};
use serde_json::json;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

/// A sandbox that is never asked to run anything: these tests only use file tools.
struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: None,
            timed_out: false,
        }
    }
}

/// Run `tool` on `work/<rel>` with the check's pin for it set to `pin`.
fn run_pinned(tool: &dyn Tool, env: &TestEnv, rel: &str, pin: PathBuf, mut args: Value) -> ToolOutput {
    let cwd = env.work();
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let checked = vec![(cwd.join(rel), pin)];
    let cx = ToolCtx {
        checked: &checked,
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    args["path"] = json!(rel);
    tool.run(&args, &cx, &mut |_: &str| {})
}

/// The real path of `p` (the test root may sit below a symlink, e.g. `/tmp`).
fn real(p: PathBuf) -> PathBuf {
    fs::canonicalize(p).unwrap()
}

#[test]
fn the_pinned_file_is_read_edited_and_written_as_usual() {
    let env = TestEnv::new();
    let f = real(env.write("work/real.txt", "a\n"));
    symlink(&f, env.work().join("link")).unwrap();
    let out = run_pinned(&ReadTool::new(), &env, "link", f.clone(), json!({}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "a\n");
    let out = run_pinned(
        &EditTool::new(),
        &env,
        "link",
        f.clone(),
        json!({"oldText": "a", "newText": "b"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fs::read_to_string(&f).unwrap(), "b\n");
    let out = run_pinned(
        &WriteTool::new(),
        &env,
        "link",
        f.clone(),
        json!({"content": "c\n"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fs::read_to_string(&f).unwrap(), "c\n");
    assert!(env.work().join("link").is_symlink(), "the link stays");
    // A new file in a new directory: the pin's tail does not exist yet.
    let new = real(env.work()).join("sub/dir/new.txt");
    let out = run_pinned(
        &WriteTool::new(),
        &env,
        "sub/dir/new.txt",
        new.clone(),
        json!({"content": "n\n"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fs::read_to_string(&new).unwrap(), "n\n");
}

#[test]
fn a_read_that_opens_another_file_than_the_pinned_one_is_refused() {
    let env = TestEnv::new();
    let approved = real(env.write("work/real.txt", "fine\n"));
    let secret = env.write("outside/secret", "SECRET-518\n");
    symlink(&secret, env.work().join("link")).unwrap();
    let out = run_pinned(&ReadTool::new(), &env, "link", approved, json!({}));
    assert!(out.is_error);
    assert!(!out.text.contains("SECRET-518"), "{}", out.text);
    assert!(
        out.text.contains("changed after the permission check"),
        "{}",
        out.text
    );
}

#[test]
fn a_write_whose_pinned_directory_became_a_link_is_refused() {
    let env = TestEnv::new();
    let work = real(env.work());
    let pin = work.join("d/f.txt");
    let outside = env.write("outside/f.txt", "original\n");
    // `d` was a directory at the check; now it is a link to a directory outside.
    symlink(outside.parent().unwrap(), env.work().join("d")).unwrap();
    let out = run_pinned(
        &WriteTool::new(),
        &env,
        "d/f.txt",
        pin,
        json!({"content": "pwned\n"}),
    );
    assert!(out.is_error, "{}", out.text);
    assert!(
        out.text.contains("changed after the permission check"),
        "{}",
        out.text
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "original\n");
    let leftovers: Vec<_> = fs::read_dir(outside.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "no temp file left outside");
}
