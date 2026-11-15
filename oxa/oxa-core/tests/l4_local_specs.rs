//! OXA-LOCAL-FIRST T3: the profile `local` declarations of the built-ins. Every schema is valid
//! JSON, has no `array` or `object` parameter, declares `integer` (never `number`) for numeric
//! parameters, lists the required parameters first, and a real session runs one call per tool
//! with integer arguments (no tool answers "Invalid arguments").

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{Engine, HeadlessUi, Role, ToolCall, VecSink};
use oxa_resources::{discover, DiscoverOpts};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use oxa_tools::descriptions::local_specs;
use serde_json::Value;
use std::sync::Arc;

const BUILTINS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

#[test]
fn every_local_schema_is_flat_integer_json_with_required_first() {
    for name in BUILTINS {
        let t = local_specs::by_name(name).unwrap_or_else(|| panic!("{name} has a local text"));
        let v: Value = serde_json::from_str(t.schema_json).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(v["type"], "object", "{name}");
        let props = v["properties"].as_object().expect("properties");
        for (p, def) in props {
            let ty = def["type"].as_str().unwrap_or("");
            assert!(
                matches!(ty, "string" | "integer" | "boolean"),
                "{name}.{p}: type {ty:?} (T3: no array, no object, no number)"
            );
            assert!(def.get("items").is_none(), "{name}.{p}: no items");
            assert!(
                def["description"].as_str().is_some_and(|d| !d.is_empty()),
                "{name}.{p}: a description"
            );
        }
        assert!(
            !t.schema_json.contains("\"number\""),
            "{name}: integer, never number"
        );
        let required: Vec<&str> = v["required"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_str().unwrap()).collect())
            .unwrap_or_default();
        for r in &required {
            assert!(props.contains_key(*r), "{name}: required {r} is declared");
        }
        // Required first: the leading properties are exactly the required ones, in order.
        let keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(
            &keys[..required.len()],
            required.as_slice(),
            "{name}: required first"
        );
        // Key order inside the schema: type, properties, required.
        let at = |k: &str| t.schema_json.find(&format!("\"{k}\"")).unwrap_or(usize::MAX);
        assert!(at("type") < at("properties"));
        assert!(at("properties") < at("required") || at("required") == usize::MAX);
    }
    let edit: Value = serde_json::from_str(local_specs::by_name("edit").unwrap().schema_json).unwrap();
    assert_eq!(
        edit["required"],
        serde_json::json!(["path", "oldText", "newText"])
    );
}

fn open(env: &TestEnv) -> (Session, Arc<ScriptedEngine>) {
    std::fs::create_dir_all(Dirs::under(env.root()).settings_file().parent().unwrap()).unwrap();
    std::fs::write(
        Dirs::under(env.root()).settings_file(),
        r#"{"oxa": {"profile": "local"}}"#,
    )
    .unwrap();
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let resources = discover(
        &dirs,
        &env.work(),
        &settings,
        &DiscoverOpts {
            trusted: true,
            ..Default::default()
        },
    );
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let dyn_engine: Arc<dyn Engine> = engine.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let s = Session::open(
        Deps {
            engine: dyn_engine,
            store,
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    (s, engine)
}

#[test]
fn each_local_tool_accepts_integer_arguments() {
    let env = TestEnv::new();
    env.write("work/f.txt", "line 1\nline 2\nline 3\n");
    let (mut s, engine) = open(&env);
    let calls = [
        ("read", r#"{"path":"f.txt","offset":1,"limit":2}"#),
        ("bash", r#"{"command":"echo ok","timeout":5}"#),
        ("write", r#"{"path":"out/new.txt","content":"x"}"#),
        ("grep", r#"{"pattern":"line","path":".","context":1,"limit":2}"#),
        ("find", r#"{"pattern":"*.txt","limit":5}"#),
        ("ls", r#"{"limit":5}"#),
        (
            "edit",
            r#"{"path":"f.txt","oldText":"line 1","newText":"LINE 1"}"#,
        ),
    ];
    let mut turn = ScriptTurn::tool("x", "read", "{}");
    turn.tool_calls = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| ToolCall {
            id: format!("c{i}"),
            name: name.to_string(),
            args_raw: args.to_string(),
        })
        .collect();
    engine.push(turn);
    engine.push(ScriptTurn::text("done"));
    s.prompt(
        "try every tool".to_string(),
        Vec::new(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    let reqs = engine.requests();
    let last = reqs.last().expect("a request after the tool calls");
    let results: Vec<(&str, &str)> = last
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| (m.tool_call_id.as_deref().unwrap(), m.content.as_deref().unwrap()))
        .collect();
    assert_eq!(results.len(), calls.len(), "one result per call: {results:?}");
    for (id, text) in &results {
        assert!(
            !text.starts_with("Invalid arguments") && !text.contains("must be a number"),
            "{id}: {text}"
        );
    }
    assert!(
        results[0].1.contains("line 1"),
        "read paged with integer offset/limit: {}",
        results[0].1
    );
}
