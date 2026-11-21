//! #955: `write` and `edit` still write on the host, and say so when sandboxed bash cannot see
//! the file (warn, never refuse). Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_fake::TestEnv;
use oxa_proto::{
    BashView, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput,
};
use oxa_tools::{EditTool, WriteTool};
use serde_json::json;
use std::path::Path;

/// A sandbox that reports one fixed view for every path.
struct Views(Option<BashView>);

impl Sandbox for Views {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bash_view(&self, _cwd: &Path, _path: &Path, _writable: bool) -> BashView {
        self.0.unwrap_or(BashView::Same)
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: None,
            timed_out: false,
            unsandboxed: false,
        }
    }
}

fn run(tool: &dyn Tool, sb: &Views, env: &TestEnv, args: serde_json::Value) -> ToolOutput {
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
    tool.run(&args, &cx, &mut |_: &str| {})
}

#[test]
fn a_file_hidden_from_bash_is_written_and_reported() {
    let env = TestEnv::new();
    for view in [BashView::Hidden] {
        let out = run(
            &WriteTool::new(),
            &Views(Some(view)),
            &env,
            json!({"path": "f.txt", "content": "hi"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("oxa.sandbox.writePaths"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(env.work().join("f.txt")).unwrap(), "hi");
        let out = run(
            &EditTool::new(),
            &Views(Some(view)),
            &env,
            json!({"path": "f.txt", "edits": [{"oldText": "hi", "newText": "ho"}]}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("oxa.sandbox.writePaths"), "{}", out.text);
    }
}

#[test]
fn a_shared_file_gets_no_note() {
    let env = TestEnv::new();
    for view in [None, Some(BashView::Same), Some(BashView::ReadOnly)] {
        let out = run(
            &WriteTool::new(),
            &Views(view),
            &env,
            json!({"path": "g.txt", "content": "hi"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(!out.text.contains("Note:"), "{}", out.text);
    }
}
