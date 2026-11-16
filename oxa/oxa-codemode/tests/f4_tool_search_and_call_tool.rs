//! F4 acceptance tests: tool_search and call_tool.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_codemode::{
    rank, Catalog, CodemodeConfig, CodemodeHooks, Namespace, Namespaces, DEFAULT_SEARCH_LIMIT,
};
use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, ExecResult, ExecSpec, Exposure, Hooks, Host, HostAction, Mode, OxaError, Sandbox,
    Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// A sandbox that is never asked to run anything: the search tools run no commands.
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

fn spec(name: &str, description: &str, schema: &str, exposure: Exposure) -> ToolSpec {
    let source = match name.strip_prefix("mcp__") {
        Some(rest) => Source::Mcp(rest.split("__").next().unwrap_or_default().to_string()),
        None => Source::Builtin,
    };
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema_json: schema.to_string(),
        class: ToolClass::Read,
        exposure,
        source,
    }
}

/// A tool whose `filter` argument is a `$ref` to a definition with a property the query names.
const TICKETS_SCHEMA: &str = r##"{"type":"object","properties":{"filter":{"$ref":"#/$defs/Filter"}},"$defs":{"Filter":{"type":"object","properties":{"lifecycle_state":{"type":"string","description":"Current lifecycle phase of the record"}}}}}"##;
const WEATHER_SCHEMA: &str = r#"{"type":"object","properties":{"city":{"type":"string","description":"City name"}},"required":["city"]}"#;
const PAGE_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Page path"}},"required":["path"]}"#;

fn catalog_specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "read",
            "Read a file from the workspace",
            r#"{"type":"object","properties":{"path":{"type":"string"}}}"#,
            Exposure::Direct,
        ),
        spec(
            CODEMODE_NAME,
            "Run a script",
            r#"{"type":"object","properties":{"code":{"type":"string"}}}"#,
            Exposure::Direct,
        ),
        spec(
            "mcp__tickets__list",
            "List work items",
            TICKETS_SCHEMA,
            Exposure::Deferred,
        ),
        spec(
            "mcp__weather__forecast",
            "Forecast for a city",
            WEATHER_SCHEMA,
            Exposure::Deferred,
        ),
        spec(
            "mcp__docs__page_read",
            "Read one documentation page",
            PAGE_SCHEMA,
            Exposure::Deferred,
        ),
        spec(
            "mcp__docs__hidden_probe",
            "Internal probe",
            PAGE_SCHEMA,
            Exposure::Hidden,
        ),
    ]
}

const CODEMODE_NAME: &str = "codemode";

fn namespaces() -> Vec<Namespace> {
    vec![
        Namespace {
            name: "docs".to_string(),
            description: Some("Developer documentation pages".to_string()),
            instructions: None,
        },
        Namespace {
            name: "tickets".to_string(),
            description: Some("Issue tracker".to_string()),
            instructions: None,
        },
    ]
}

/// A host that records `CallTool` requests and answers with a fixed text, or fails when asked.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Value)>>,
    fail: bool,
}

impl Host for Recorder {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        match action {
            HostAction::CallTool { name, args } => {
                self.calls.lock().unwrap().push((name.clone(), args));
                if self.fail {
                    return Err(OxaError::new(codes::NOT_FOUND, "Permission denied: test refusal"));
                }
                Ok(json!({"text": format!("ran {name}"), "isError": false}))
            }
            _ => Err(OxaError::new(codes::NOT_FOUND, "not mocked")),
        }
    }
}

/// Hook set with the given `tool_search` switch, over a catalog that can grow during a test.
fn hooks(tool_search: bool, catalog: Arc<Mutex<Vec<ToolSpec>>>, host: &Arc<Recorder>) -> CodemodeHooks {
    let cat: Catalog = Arc::new(move || catalog.lock().unwrap().clone());
    let ns: Namespaces = Arc::new(namespaces);
    let config = CodemodeConfig {
        codemode: false,
        tool_search,
        max_workers: 1,
    };
    let env = TestEnv::new();
    let hooks = CodemodeHooks::new(config, cat, ns, env.tmp());
    let dyn_host: Arc<dyn Host> = host.clone();
    hooks.attach(dyn_host);
    hooks
}

fn default_hooks(host: &Arc<Recorder>) -> CodemodeHooks {
    hooks(true, Arc::new(Mutex::new(catalog_specs())), host)
}

fn find(hooks: &CodemodeHooks, name: &str) -> Arc<dyn Tool> {
    hooks
        .tools()
        .into_iter()
        .find(|t| t.spec().name == name)
        .unwrap_or_else(|| panic!("tool {name} is not declared"))
}

/// Run a tool with `args` in a fresh test environment.
fn run(tool: &dyn Tool, args: Value) -> ToolOutput {
    let env = TestEnv::new();
    let cwd = env.work();
    let spill = env.tmp();
    let sandbox = NoSandbox;
    let cancel = CancelToken::new();
    let envs: Vec<(String, String)> = Vec::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &envs,
        tune: Default::default(),
        hidden: None,
    };
    tool.run(&args, &cx, &mut |_| {})
}

/// The declared specs of a hook set, as the prefix sees them.
fn declared(hooks: &CodemodeHooks) -> Vec<ToolSpec> {
    hooks.tools().iter().map(|t| t.spec().clone()).collect()
}

/// $ref schema properties searched
#[test]
fn ranking_includes_ref_properties() {
    let tools = catalog_specs();
    // The property is only reachable through `$ref` into `$defs`.
    let hits = rank("lifecycle phase", &tools, &namespaces(), 0, None);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].name, "mcp__tickets__list");
    // The property name itself, split on underscores, also matches.
    let hits = rank("lifecycle state", &tools, &namespaces(), 0, None);
    assert_eq!(hits.first().map(|h| h.name.as_str()), Some("mcp__tickets__list"));
    // Without the reference the same words match nothing.
    let plain = vec![spec(
        "mcp__tickets__get",
        "Get a work item",
        r#"{"type":"object","properties":{"filter":{"type":"object"}}}"#,
        Exposure::Deferred,
    )];
    assert!(rank("lifecycle phase", &plain, &namespaces(), 0, None).is_empty());
}

/// call_tool refuses unknown names
#[test]
fn deferred_callable_only_after_match() {
    let host = Arc::new(Recorder::default());
    let hooks = default_hooks(&host);
    let call = find(&hooks, "call_tool");

    // Not matched yet: refused, and nothing reaches the host.
    let out = run(
        call.as_ref(),
        json!({"name": "mcp__docs__page_read", "args": {"path": "a"}}),
    );
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "Tool mcp__docs__page_read has not been loaded by tool_search"
    );
    assert!(host.calls.lock().unwrap().is_empty());

    // A name no search matched stays refused, even after other searches.
    let search = find(&hooks, "tool_search");
    let found = run(search.as_ref(), json!({"query": "documentation page"}));
    assert!(!found.is_error);
    let out = run(
        call.as_ref(),
        json!({"name": "mcp__docs__hidden_probe", "args": {}}),
    );
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "Tool mcp__docs__hidden_probe has not been loaded by tool_search"
    );

    // The matched tool is callable now.
    let out = run(
        call.as_ref(),
        json!({"name": "mcp__docs__page_read", "args": {"path": "a"}}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(host.calls.lock().unwrap().len(), 1);
}

/// W3
#[test]
fn prefix_sha_unchanged_across_search() {
    let host = Arc::new(Recorder::default());
    let catalog = Arc::new(Mutex::new(catalog_specs()));
    let hooks = hooks(true, catalog.clone(), &host);
    let before = declared(&hooks);

    let search = find(&hooks, "tool_search");
    let out = run(search.as_ref(), json!({"query": "forecast city"}));
    assert!(out.text.contains("mcp__weather__forecast"));
    assert_eq!(
        declared(&hooks),
        before,
        "a search must not change the declared specs"
    );

    // An MCP server connects later: the catalog grows, the declared specs stay the same.
    catalog.lock().unwrap().push(spec(
        "mcp__maps__route",
        "Plan a route between two places",
        WEATHER_SCHEMA,
        Exposure::Deferred,
    ));
    let out = run(search.as_ref(), json!({"query": "route places"}));
    assert!(out.text.contains("mcp__maps__route"), "{}", out.text);
    assert_eq!(
        declared(&hooks),
        before,
        "a later MCP connect must not change the prefix"
    );
}

/// not declared
#[test]
fn results_returned_as_text() {
    let host = Arc::new(Recorder::default());
    let hooks = default_hooks(&host);
    let search = find(&hooks, "tool_search");
    let before = declared(&hooks);

    let out = run(
        search.as_ref(),
        json!({"query": "page documentation", "limit": 1}),
    );
    assert!(!out.is_error);
    assert!(out.text.contains("name: mcp__docs__page_read"), "{}", out.text);
    assert!(
        out.text.contains(PAGE_SCHEMA),
        "the JSON Schema is part of the text"
    );
    assert!(out.text.contains("call_tool"), "{}", out.text);
    // Matches are text: nothing is declared for the next call.
    assert_eq!(declared(&hooks), before);

    let none = run(search.as_ref(), json!({"query": "zzzz unmatched words"}));
    assert_eq!(none.text, "No matching tools.");
    // Deterministic (W4).
    let again = run(
        search.as_ref(),
        json!({"query": "page documentation", "limit": 1}),
    );
    assert_eq!(again.text, out.text);
}

/// same path as direct tools
#[test]
fn call_tool_passes_policy_and_shape() {
    let host = Arc::new(Recorder::default());
    let hooks = default_hooks(&host);
    let search = find(&hooks, "tool_search");
    let call = find(&hooks, "call_tool");
    run(search.as_ref(), json!({"query": "documentation page"}));

    // The call reaches the host as a CallTool action with the target name and arguments, so the
    // host applies hooks, policy and shape exactly as for a direct tool.
    let out = run(
        call.as_ref(),
        json!({"name": "mcp__docs__page_read", "args": {"path": "intro"}}),
    );
    assert!(!out.is_error);
    assert_eq!(out.text, "ran mcp__docs__page_read");
    let calls = host.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![("mcp__docs__page_read".to_string(), json!({"path": "intro"}))]
    );

    // A refusal from the host (policy) comes back as the tool error text.
    let refusing = Arc::new(Recorder {
        calls: Mutex::new(Vec::new()),
        fail: true,
    });
    let hooks2 = default_hooks(&refusing);
    let search2 = find(&hooks2, "tool_search");
    let call2 = find(&hooks2, "call_tool");
    run(search2.as_ref(), json!({"query": "documentation page"}));
    let out = run(
        call2.as_ref(),
        json!({"name": "mcp__docs__page_read", "args": {"path": "x"}}),
    );
    assert!(out.is_error);
    assert_eq!(out.text, "Permission denied: test refusal");

    // Malformed arguments are refused before the host is reached.
    let out = run(call.as_ref(), json!({"args": {}}));
    assert!(out.is_error);
    let out = run(
        call.as_ref(),
        json!({"name": "mcp__docs__page_read", "args": "text"}),
    );
    assert!(out.is_error);
}

/// tool_search off = absent
#[test]
fn tools_declared_only_when_enabled() {
    let host = Arc::new(Recorder::default());
    let off = hooks(false, Arc::new(Mutex::new(catalog_specs())), &host);
    let names: Vec<String> = off.tools().iter().map(|t| t.spec().name.clone()).collect();
    assert!(
        !names.iter().any(|n| n == "tool_search" || n == "call_tool"),
        "{names:?}"
    );

    let on = hooks(true, Arc::new(Mutex::new(catalog_specs())), &host);
    let names: Vec<String> = on.tools().iter().map(|t| t.spec().name.clone()).collect();
    assert!(names.iter().any(|n| n == "tool_search"));
    assert!(names.iter().any(|n| n == "call_tool"));
}

// --- extra checks -----------------------------------------------------------------------------

#[test]
fn default_limit_and_namespace_filter() {
    let mut many = Vec::new();
    for i in 0..12 {
        many.push(spec(
            &format!("mcp__bulk__item_{i:02}"),
            "Bulk item lookup",
            WEATHER_SCHEMA,
            Exposure::Deferred,
        ));
    }
    let hits = rank("bulk item lookup", &many, &[], 0, None);
    assert_eq!(hits.len(), DEFAULT_SEARCH_LIMIT);
    // Ties break by name.
    assert_eq!(hits[0].name, "mcp__bulk__item_00");

    let mixed = catalog_specs();
    let only_docs = rank("page", &mixed, &namespaces(), 0, Some("docs"));
    assert!(
        only_docs.iter().all(|h| h.name.starts_with("mcp__docs__")),
        "{only_docs:?}"
    );
    assert!(!only_docs.is_empty());
}

#[test]
fn exact_name_query_ranks_first_and_camel_case_splits() {
    let tools = catalog_specs();
    let hits = rank("mcp__weather__forecast", &tools, &namespaces(), 0, None);
    assert_eq!(
        hits.first().map(|h| h.name.as_str()),
        Some("mcp__weather__forecast")
    );
    // camelCase in the query splits like the names do.
    let hits = rank("WeatherForecast", &tools, &namespaces(), 0, None);
    assert_eq!(
        hits.first().map(|h| h.name.as_str()),
        Some("mcp__weather__forecast")
    );
}

#[test]
fn invalid_search_arguments_are_errors() {
    let host = Arc::new(Recorder::default());
    let hooks = default_hooks(&host);
    let search = find(&hooks, "tool_search");
    assert!(run(search.as_ref(), json!({})).is_error);
    assert!(run(search.as_ref(), json!({"query": "   "})).is_error);
    assert!(run(search.as_ref(), json!({"query": "page", "limit": 0})).is_error);
}
