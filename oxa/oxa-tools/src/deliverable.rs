//! #2293 A: the `deliverable` tool. The model names the file a request asks it to produce and
//! checks it from the same guarded file view as `read` and `write`: existence and non-empty
//! content, never the content itself. A call's typed `ui` is the declaration the run's end
//! record reads. Nothing here gates a run, changes an exit code or scores a task: a missing
//! file is a fact the model is told, not a refusal.
use crate::checked;
use crate::read::arg_path;
use oxa_proto::{Access, Exposure, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value};
use std::io;
use std::path::Path;

/// The longest `path` the tool accepts, in bytes; a longer one is invalid arguments.
const MAX_PATH_BYTES: usize = 4096;

/// What one check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A regular file with content.
    Present,
    /// A regular file with no bytes.
    Empty,
    /// Nothing at the path.
    Missing,
    /// Something that is not a regular file (a directory, a device, a socket).
    NotFile,
    /// There but not judgeable (a permission error, a swapped symlink, an unreadable path).
    Unverified,
}

impl State {
    /// The `state` field of a call's typed `ui`.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Present => "present",
            State::Empty => "empty",
            State::Missing => "missing",
            State::NotFile => "not_file",
            State::Unverified => "unverified",
        }
    }

    /// The reverse of [`State::as_str`]; an unknown word is `None` (never guess a state).
    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "present" => State::Present,
            "empty" => State::Empty,
            "missing" => State::Missing,
            "not_file" => State::NotFile,
            "unverified" => State::Unverified,
            _ => return None,
        })
    }
}

/// The state of `path` from one metadata look, for the end-of-run check (there is no
/// `ToolCtx` then): no content is read, and a symlink is followed as `read` follows it.
pub fn observe(path: &Path) -> (State, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => (State::NotFile, None),
        Ok(m) if m.len() == 0 => (State::Empty, Some(0)),
        Ok(m) => (State::Present, Some(m.len())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (State::Missing, None),
        Err(_) => (State::Unverified, None),
    }
}

/// `deliverable {path}`: record the file a request names as its output and report whether it is
/// on disk now. `ToolOutput::ui` = `{"deliverable": true, "path": <as written>, "abs": <guarded
/// location>, "state": <[`State`]>, "bytes": n | null}`.
#[derive(Debug, Clone)]
pub struct DeliverableTool {
    spec: ToolSpec,
}

impl DeliverableTool {
    pub fn new() -> DeliverableTool {
        DeliverableTool {
            spec: ToolSpec {
                name: "deliverable".into(),
                description: "Record the file this task asks you to produce and check it now. Call it as soon as the request names an output file, again after writing it, and once before you finish. It reports existence and size only, never the content.".into(),
                schema_json: serde_json::json!({
                    "type": "object", "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "description": "The output file, as the request names it"}
                    },
                    "required": ["path"]
                })
                .to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
        }
    }
}

impl Default for DeliverableTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for DeliverableTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// A member of the default selection: a `+name` / `-name` patch keeps it (`-deliverable`
    /// removes it), as it keeps the built-ins. A plain `defaultTools` list still replaces it.
    fn default_selected(&self) -> bool {
        true
    }
    /// `[Access::Read(path)]`: the deny-list, the private-path floor and the pinned location of
    /// `read`, so a check never shows what the model may not read.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let path = arg_path(args)?;
        if path.len() > MAX_PATH_BYTES {
            return Err(format!(
                "Invalid arguments: path is longer than {MAX_PATH_BYTES} bytes"
            ));
        }
        Ok(vec![Access::Read(crate::resolve_tool_path(cx, path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = match arg_path(args) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        if path.len() > MAX_PATH_BYTES {
            return ToolOutput::error(format!(
                "Invalid arguments: path is longer than {MAX_PATH_BYTES} bytes"
            ));
        }
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(oxa_proto::tool_errors::ABORTED);
        }
        let abs = crate::resolve_tool_path(cx, path);
        // `read` refuses what is not a regular file before it opens anything: `open(O_RDONLY)`
        // on a named pipe blocks until a writer opens it, and no per-tool timeout would end it.
        // The guarded open below is then the #518 pin check, not a way to discover the kind.
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return report(path, &abs, State::Missing, None),
            Err(_) => return report(path, &abs, State::Unverified, None),
        };
        if !meta.is_file() {
            return report(path, &abs, State::NotFile, None);
        }
        // #518: the location the permission check judged (a symlink resolved): a link swapped
        // after the check is refused, not followed.
        let (state, bytes) = match checked::open(cx, &abs) {
            Ok(file) => match file.metadata() {
                Ok(m) if m.len() == 0 => (State::Empty, Some(0)),
                Ok(m) => (State::Present, Some(m.len())),
                Err(_) => (State::Unverified, None),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => (State::Missing, None),
            Err(_) => (State::Unverified, None),
        };
        report(path, &abs, state, bytes)
    }
}

/// The result for the model and the typed `ui` the run's record reads.
fn report(path: &str, abs: &Path, state: State, bytes: Option<u64>) -> ToolOutput {
    let mut ui = serde_json::json!({
        "deliverable": true,
        "path": path,
        "state": state.as_str(),
        "bytes": bytes,
    });
    // The guarded location, for the run's end check. A path that is not UTF-8 has no record:
    // the call still answers the model, and nothing claims it later.
    if let Some(abs) = abs.to_str() {
        ui["abs"] = Value::String(abs.to_string());
    }
    ToolOutput {
        images: Vec::new(),
        text: match state {
            State::Present => format!(
                "Deliverable {path}: present, {} bytes. The content is not checked: read it once before you finish, and check again if you write it later.",
                bytes.unwrap_or(0)
            ),
            State::Empty => format!(
                "Deliverable {path}: empty (0 bytes). Write the answer with the write tool, then check it again."
            ),
            State::Missing => format!(
                "Deliverable {path}: missing. Write it with the write tool now, then check it again."
            ),
            State::NotFile => format!("Deliverable {path}: not a regular file."),
            State::Unverified => format!("Deliverable {path}: could not be checked."),
        },
        is_error: false,
        ui,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use oxa_proto::{CancelToken, ExecResult, ExecSpec, Mode, OxaError};
    use std::path::PathBuf;

    /// The tool only reads paths, so the test's sandbox never runs a command.
    struct NoSandbox;

    impl oxa_proto::Sandbox for NoSandbox {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
            ExecResult {
                output: Vec::new(),
                exit: Some(0),
                timed_out: false,
                unsandboxed: true,
            }
        }
    }

    fn with_ctx<T>(work: &Path, run: impl FnOnce(&ToolCtx) -> T) -> T {
        let (sandbox, cancel) = (NoSandbox, CancelToken::new());
        let spill: PathBuf = work.to_path_buf();
        let cx = ToolCtx {
            cwd: work,
            spill_dir: &spill,
            sandbox: &sandbox,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &[],
            tune: Default::default(),
            checked: &[],
            hidden: None,
        };
        run(&cx)
    }

    fn check(env: &TestEnv, path: &str) -> ToolOutput {
        with_ctx(&env.work(), |cx| {
            DeliverableTool::new().run(&serde_json::json!({ "path": path }), cx, &mut |_| {})
        })
    }

    #[test]
    fn a_present_file_is_reported_with_its_size_and_typed_ui() {
        let env = TestEnv::new();
        env.write("work/move.txt", "e2e4\n");
        let out = check(&env, "move.txt");
        assert!(!out.is_error);
        assert!(out.text.starts_with("Deliverable move.txt: present, 5 bytes."));
        assert_eq!(out.ui["deliverable"], true);
        assert_eq!(out.ui["state"], "present");
        assert_eq!(out.ui["bytes"], 5);
        assert_eq!(
            out.ui["abs"].as_str(),
            env.work().join("move.txt").to_str(),
            "the record keeps the guarded absolute location"
        );
    }

    #[test]
    fn missing_and_empty_files_are_facts_not_errors() {
        let env = TestEnv::new();
        env.write("work/empty.txt", "");
        let missing = check(&env, "later.txt");
        assert!(!missing.is_error, "a check that ran is not a tool error");
        assert!(missing.text.contains("missing"), "{}", missing.text);
        assert_eq!(missing.ui["state"], "missing");
        let empty = check(&env, "empty.txt");
        assert!(empty.text.contains("empty"), "{}", empty.text);
        assert_eq!(empty.ui["state"], "empty");
        assert_eq!(empty.ui["bytes"], 0);
    }

    /// A named pipe must be refused, not opened: `open(O_RDONLY)` on a FIFO blocks the whole
    /// run until a writer appears. The call runs on its own thread so a regression fails this
    /// test with the reason instead of hanging the suite.
    #[test]
    fn a_fifo_is_refused_before_it_can_block() {
        let env = TestEnv::new();
        let fifo = env.work().join("answer-pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a fresh path inside this test's private directory, with no other references.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let work = env.work();
        std::thread::spawn(move || {
            let out = with_ctx(&work, |cx| {
                DeliverableTool::new().run(&serde_json::json!({ "path": "answer-pipe" }), cx, &mut |_| {})
            });
            let _ = tx.send((out.text, out.ui["state"].clone()));
        });
        let (text, state) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| {
                panic!("the deliverable check blocked on a named pipe instead of refusing it")
            });
        assert!(text.contains("not a regular file"), "{text}");
        assert_eq!(state, "not_file");
    }

    #[test]
    fn a_directory_is_not_a_deliverable() {
        let env = TestEnv::new();
        std::fs::create_dir_all(env.work().join("out")).unwrap();
        let out = check(&env, "out");
        assert!(out.text.contains("not a regular file"), "{}", out.text);
        assert_eq!(out.ui["state"], "not_file");
    }

    /// #2293 A: the declaration is one more entry in the frozen tool list, so its cost is part
    /// of the session's prompt budget on every task, relevant or not. Keep it small and print
    /// the measured bytes (a budget, not a descriptor count).
    #[test]
    fn the_declaration_stays_inside_its_prompt_budget() {
        let spec = DeliverableTool::new().spec().clone();
        let bytes = spec.name.len() + spec.description.len() + spec.schema_json.len();
        eprintln!(
            "deliverable declaration: {bytes} bytes (name {}, description {}, schema {})",
            spec.name.len(),
            spec.description.len(),
            spec.schema_json.len()
        );
        assert!(
            bytes <= 640,
            "the declaration grew past its prompt budget: {bytes} bytes"
        );
    }

    #[test]
    fn invalid_arguments_are_an_error_and_no_record() {
        let env = TestEnv::new();
        let tool = DeliverableTool::new();
        let out = with_ctx(&env.work(), |cx| {
            tool.run(&serde_json::json!({}), cx, &mut |_| {})
        });
        assert!(out.is_error);
        assert!(out.ui.get("deliverable").is_none());
        let long = "x".repeat(MAX_PATH_BYTES + 1);
        assert!(with_ctx(&env.work(), |cx| tool
            .access(&serde_json::json!({ "path": long }), cx))
        .is_err());
    }

    #[test]
    fn observe_reports_the_same_states_without_a_tool_context() {
        let env = TestEnv::new();
        let file = env.write("work/answer.txt", "hello\n");
        assert_eq!(observe(&file), (State::Present, Some(6)));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(observe(&file), (State::Missing, None));
    }
}
