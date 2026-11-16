//! #597: the sandbox mounts the session's scratch directory at `/tmp`, so a model that wrote
//! `/tmp/x` with bash names that path to the read tools. They must reach the session's file,
//! not the host's `/tmp`. Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_fake::TestEnv;
use oxa_proto::{
    Access, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, Value,
};
use oxa_tools::{FindTool, GrepTool, LsTool, ReadTool};
use serde_json::json;
use std::path::{Path, PathBuf};

/// A sandbox whose session scratch directory (`/tmp` inside) is `scratch`.
struct Scratchy {
    scratch: Option<PathBuf>,
}

impl Sandbox for Scratchy {
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
    fn scratch_dir(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }
}

fn run(tool: &dyn Tool, sb: &Scratchy, env: &TestEnv, args: &Value) -> (Vec<Access>, ToolOutput) {
    let cwd = env.work();
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: sb,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    let access = tool.access(args, &cx).expect("access");
    (access, tool.run(args, &cx, &mut |_: &str| {}))
}

#[test]
fn read_tools_map_tmp_to_the_session_scratch() {
    let env = TestEnv::new();
    let scratch = env
        .write("scratch/notes.json", "needle\n")
        .parent()
        .unwrap()
        .to_path_buf();
    let sb = Scratchy {
        scratch: Some(scratch.clone()),
    };

    let (access, out) = run(&ReadTool::new(), &sb, &env, &json!({"path": "/tmp/notes.json"}));
    assert_eq!(access, vec![Access::Read(scratch.join("notes.json"))]);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "needle\n");

    let (access, out) = run(&LsTool::new(), &sb, &env, &json!({"path": "/tmp"}));
    assert_eq!(access, vec![Access::Read(scratch.clone())]);
    assert!(out.text.contains("notes.json"), "{}", out.text);

    let (access, out) = run(
        &GrepTool::new(),
        &sb,
        &env,
        &json!({"pattern": "needle", "path": "/tmp"}),
    );
    assert_eq!(access, vec![Access::Read(scratch.clone())]);
    assert!(out.text.contains("notes.json"), "{}", out.text);

    let (access, out) = run(
        &FindTool::new(),
        &sb,
        &env,
        &json!({"pattern": "*.json", "path": "/tmp"}),
    );
    assert_eq!(access, vec![Access::Read(scratch.clone())]);
    assert!(out.text.contains("notes.json"), "{}", out.text);
}

#[test]
fn paths_that_only_start_like_tmp_or_without_a_scratch_are_unchanged() {
    let env = TestEnv::new();
    let scratch = env.write("scratch/a", "x").parent().unwrap().to_path_buf();
    let sb = Scratchy {
        scratch: Some(scratch),
    };
    for p in ["/tmpfoo/a", "/var/tmp/a", "/tmp/../etc/passwd"] {
        let (access, _) = run(&ReadTool::new(), &sb, &env, &json!({"path": p}));
        let want = oxa_tools::resolve_path(&env.work(), p);
        assert_eq!(access, vec![Access::Read(want)], "{p}");
    }
    let none = Scratchy { scratch: None };
    let (access, _) = run(&ReadTool::new(), &none, &env, &json!({"path": "/tmp/a"}));
    assert_eq!(access, vec![Access::Read(PathBuf::from("/tmp/a"))]);
}

/// A workspace that itself lives under `/tmp` is bound at its real path in the sandbox, so the
/// files in it are the workspace's, not the scratch directory's.
#[test]
fn a_workspace_under_tmp_keeps_its_own_paths() {
    let env = TestEnv::new();
    let scratch = env.write("scratch/a", "x").parent().unwrap().to_path_buf();
    let sb = Scratchy {
        scratch: Some(scratch),
    };
    let ws = PathBuf::from(format!("/tmp/oxa-i597-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace under /tmp");
    std::fs::write(ws.join("note.txt"), "NOTE\n").expect("write");
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: &ws,
        spill_dir: &spill,
        sandbox: &sb,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    let tool = ReadTool::new();
    for p in ["note.txt", ws.join("note.txt").to_str().unwrap()] {
        let args = json!({ "path": p });
        let access = tool.access(&args, &cx).expect("access");
        let out = tool.run(&args, &cx, &mut |_: &str| {});
        assert_eq!(access, vec![Access::Read(ws.join("note.txt"))], "{p}");
        assert_eq!(out.text, "NOTE\n", "{p}");
    }
    std::fs::remove_dir_all(&ws).ok();
}
