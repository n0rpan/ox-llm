//! V020-OPTIMIZE token measurement (lane `tokens`): one scripted session per arm, the chat
//! transcript of each written as JSONL for the engine-tokenizer harness
//! (`oxc/benchmarks/tokens/token_harness.py`). Ignored by default; it writes nothing unless
//! `OXA_TOKEN_DUMP_DIR` is set:
//!
//! `OXA_TOKEN_DUMP_DIR=<dir> cargo test -p oxa-core --test v020_token_dump -- --ignored --nocapture`
//!
//! Each line's `body` carries `chat_template_kwargs` as the engine request sends it (`preserve_thinking`
//! when the request sets it, `enable_thinking` always), so the harness renders the same template.
//!
//! Arms: `pi`, `local`, `local_nodiet` (profile local, output diet and tool diet off), and the
//! three again with a `subagent` tool declared (`pi_sub`, `local_sub`, `local_nodiet_sub`). Every
//! arm runs the same script on the same fixture tree: a `grep`, a `read` of the large file, a
//! second `read` of a 67,000-byte source-like file with no offset or limit, a short final answer.
//! Byte counts are printed per request for cross-checking.
//!
//! L4: `local_soft8k` and `pi_soft8k` are the same script with `oxa.compaction.softTokens` 8000
//! (the working size that caps the local tool results at 6000 bytes; pi keeps its 16 KiB).

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    write_json_str, Access, CommandSpec, Engine, Exposure, HeadlessUi, HookPoint, HookReply, Hooks, Host,
    OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::{discover, DiscoverOpts};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The user prompt of every arm.
const PROMPT: &str = "Find where `needle` is used and show me the big file.";
/// The grep call of turn 1: the pattern matches in every one of the 30 fixture files.
const GREP_ARGS: &str = r#"{"pattern":"needle","context":2,"path":"project"}"#;
/// The grep call of turn 2: under both limits (4 modules, 40 matches, about 8 KB), so the normal result form
/// is returned (full lines, or the path-once form when `oxa.diet.tools` is on).
const GREP_SMALL_ARGS: &str =
    r#"{"pattern":"needle","context":2,"path":"project","glob":"module_0[0-3]/**"}"#;
/// The read call of turn 3: the large file, no offset or limit.
const READ_ARGS: &str = r#"{"path":"project/big/large.txt"}"#;
/// The read call of turn 4: the 67,000-byte source-like file, no offset or limit (L4).
const SOURCE_READ_ARGS: &str = r#"{"path":"project/src/large_source.rs"}"#;
/// The test root (random per run) is replaced by this fixed text in the dumped bytes.
const FIXTURE_ROOT: &str = "/oxa-fixture";

struct Arm {
    name: &'static str,
    settings: &'static str,
    subagent: bool,
}

const ARMS: [Arm; 8] = [
    Arm {
        name: "pi",
        settings: "{}",
        subagent: false,
    },
    Arm {
        name: "local",
        settings: r#"{"oxa": {"profile": "local"}}"#,
        subagent: false,
    },
    Arm {
        name: "local_nodiet",
        settings: r#"{"oxa": {"profile": "local", "diet": {"output": false, "tools": false}}}"#,
        subagent: false,
    },
    Arm {
        name: "pi_sub",
        settings: "{}",
        subagent: true,
    },
    Arm {
        name: "local_sub",
        settings: r#"{"oxa": {"profile": "local"}}"#,
        subagent: true,
    },
    Arm {
        name: "local_nodiet_sub",
        settings: r#"{"oxa": {"profile": "local", "diet": {"output": false, "tools": false}}}"#,
        subagent: true,
    },
    Arm {
        name: "local_soft8k",
        settings: r#"{"oxa": {"profile": "local", "compaction": {"softTokens": 8000}}}"#,
        subagent: false,
    },
    Arm {
        name: "pi_soft8k",
        settings: r#"{"oxa": {"compaction": {"softTokens": 8000}}}"#,
        subagent: false,
    },
];

/// A stand-in for the `subagent` tool: declared (Direct) so the prompt lists it.
struct SubagentTool(ToolSpec);

impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput::ok("delegated")
    }
}

struct SubagentDecl;

impl Hooks for SubagentDecl {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SubagentTool(ToolSpec {
            name: "subagent".to_string(),
            description: "Delegate tasks to specialized subagents with isolated context.".to_string(),
            schema_json: r#"{"type":"object","properties":{"task":{"type":"string","description":"Task to delegate"}}}"#
                .to_string(),
            class: ToolClass::Exec,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        }))]
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, _host: Arc<dyn Host>) {}
}

/// A default session in `env` working in `cwd` under `settings`, driven by `engine`.
fn open(env: &TestEnv, cwd: &Path, engine: Arc<dyn Engine>, settings: &str, subagent: bool) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, settings).unwrap();
    let settings = Settings::load(&dirs, cwd, true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, cwd, &settings, &opts);
    let hooks: Vec<Arc<dyn Hooks>> = if subagent {
        vec![Arc::new(SubagentDecl)]
    } else {
        Vec::new()
    };
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: oxa_tools::builtin_tools(),
            hooks,
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: cwd.to_path_buf(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
        },
        Open::New,
    )
    .expect("open")
}

/// The fixture of `oxa-tools/tests/v020_tool_diet.rs` (30 files, 10 matches each, long nested
/// paths) plus one large text file of 1500 lines with no match.
fn write_fixture(env: &TestEnv) {
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
    let big: String = (0..1500)
        .map(|k| format!("line {k:04} of the big file: plain text for the read tool\n"))
        .collect();
    env.write("work/project/big/large.txt", &big);
    // L4: 67,000 bytes of Rust-like source, the last line a comment that makes up the length.
    let mut src = String::new();
    let mut i = 0;
    while src.len() < 66_900 {
        src.push_str(&format!(
            "pub fn item_{i:04}(x: u32) -> u32 {{ x.wrapping_mul({i}) + 1 }}\n"
        ));
        i += 1;
    }
    let rest = 67_000 - src.len();
    src.push_str("//");
    src.push_str(&" ".repeat(rest - 3));
    src.push('\n');
    assert_eq!(src.len(), 67_000);
    env.write("work/project/src/large_source.rs", &src);
}

/// Turn 1 greps the whole tree (over the limit), turn 2 greps 8 modules (under the limit), turn 3
/// reads the large file, turn 4 reads the 67,000-byte source file, turn 5 answers in one line.
fn script() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("c1", "grep", GREP_ARGS),
        ScriptTurn::tool("c2", "grep", GREP_SMALL_ARGS),
        ScriptTurn::tool("c3", "read", READ_ARGS),
        ScriptTurn::tool("c4", "read", SOURCE_READ_ARGS),
        ScriptTurn::text("The needle is used in the 30 module files; the big file is large.txt."),
    ]
}

/// The assistant message as the transcript format wants it: `content` and `tool_calls`.
fn reply_json(m: &WireMsg) -> Value {
    let calls: Vec<Value> = m
        .tool_calls
        .iter()
        .map(|t| {
            json!({"id": t.id, "type": "function", "function": {"name": t.name, "arguments": t.args_raw}})
        })
        .collect();
    json!({"content": m.content, "tool_calls": calls})
}

/// One transcript line for request `r`: `{"body": {"messages", "tools"}, "reply": {...}}`.
/// The messages are written by `WireMsg::write_json`, the writer the engine request body uses;
/// the tools are the `tools_json` bytes as sent.
fn dump_line(r: &Recorded, root: &str) -> String {
    let system = r.system.replace(root, FIXTURE_ROOT);
    let tools = r.tools_json.replace(root, FIXTURE_ROOT);
    serde_json::from_str::<Value>(&tools).expect("tools_json is a JSON array");
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"body\":{\"messages\":[{\"role\":\"system\",\"content\":");
    write_json_str(&system, &mut out);
    out.push(b'}');
    for m in &r.messages {
        out.push(b',');
        m.write_json(&mut out);
    }
    out.extend_from_slice(b"],\"tools\":");
    out.extend_from_slice(tools.trim().as_bytes());
    // The engine body's `chat_template_kwargs` (oxa-engine chat.rs build_body): `preserve_thinking`
    // only when the request sets it, `enable_thinking` always.
    out.extend_from_slice(b",\"chat_template_kwargs\":{");
    if r.preserve_thinking == Some(true) {
        out.extend_from_slice(b"\"preserve_thinking\":true,");
    }
    out.extend_from_slice(b"\"enable_thinking\":");
    out.extend_from_slice(if r.thinking { b"true" } else { b"false" });
    out.extend_from_slice(b"}");
    out.extend_from_slice(b"},\"reply\":");
    let reply = r
        .reply
        .as_ref()
        .map(reply_json)
        .unwrap_or_else(|| json!({"content": null, "tool_calls": []}));
    out.extend_from_slice(reply.to_string().as_bytes());
    out.extend_from_slice(b"}\n");
    String::from_utf8(out).expect("UTF-8 JSON")
}

/// The tool results that request `i` adds to the history of request `i - 1`: the messages past
/// the previous request's history that have the `tool` role.
fn new_tool_results(reqs: &[Recorded], i: usize) -> Vec<usize> {
    if i == 0 {
        return Vec::new();
    }
    reqs[i]
        .messages
        .iter()
        .skip(reqs[i - 1].messages.len())
        .filter(|m| m.role == oxa_proto::Role::Tool)
        .map(|m| m.content.as_deref().unwrap_or("").len())
        .collect()
}

#[test]
#[ignore = "writes transcripts to OXA_TOKEN_DUMP_DIR; run with --ignored"]
fn dump_token_transcripts() {
    let Some(dir) = std::env::var_os("OXA_TOKEN_DUMP_DIR") else {
        eprintln!("OXA_TOKEN_DUMP_DIR is unset: nothing to dump");
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create the dump directory");
    for arm in &ARMS {
        let env = TestEnv::new();
        write_fixture(&env);
        let cwd = env.work();
        let eng = Arc::new(ScriptedEngine::new(script()));
        let engine: Arc<dyn Engine> = eng.clone();
        let mut s = open(&env, &cwd, engine, arm.settings, arm.subagent);
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.prompt(PROMPT.to_string(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        let reqs = eng.requests();
        assert_eq!(eng.remaining(), 0, "{}: the script is used up", arm.name);
        if arm.settings.contains("softTokens") {
            // The soft point is reached after the answer: compaction requests follow the script.
            assert!(reqs.len() >= 5, "{}: one request per scripted turn", arm.name);
        } else {
            assert_eq!(reqs.len(), 5, "{}: one request per scripted turn", arm.name);
        }

        let root = env.root().to_string_lossy().into_owned();
        let mut text = String::new();
        eprintln!("== arm {}", arm.name);
        for (i, r) in reqs.iter().enumerate() {
            let system = r.system.replace(&root, FIXTURE_ROOT);
            let tools = r.tools_json.replace(&root, FIXTURE_ROOT);
            let results = new_tool_results(&reqs, i);
            eprintln!(
                "  request {i}: system {} B, tools {} B, history {} msgs, new tool results {:?} B",
                system.len(),
                tools.len(),
                r.messages.len(),
                results
            );
            eprintln!(
                "  request {i}: chat_template_kwargs enable_thinking {}, preserve_thinking {:?}",
                r.thinking, r.preserve_thinking
            );
            text.push_str(&dump_line(r, &root));
        }
        // The under-limit grep (turn 2) result, printed for the form check: full lines under pi
        // and the diet-off local arm; the path-once form only when `oxa.diet.tools` is on.
        let small = reqs[3]
            .messages
            .iter()
            .filter(|m| m.role == oxa_proto::Role::Tool)
            .nth(1)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        eprintln!(
            "  grep (under limit) result: {} B, first 120 chars: {:?}",
            small.len(),
            small.chars().take(120).collect::<String>()
        );
        let path = dir.join(format!("{}.jsonl", arm.name));
        std::fs::write(&path, text).expect("write the transcript");
        eprintln!("  wrote {}", path.display());
    }
}
