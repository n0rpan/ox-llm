//! F7 acceptance tests: /mcp selector.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::Keymap;
use oxa_fake::TestEnv;
use oxa_interactive::selectors::{
    apply_mcp_request, mcp_rows, McpRequest, McpRowState, McpSelector, McpServerRow,
};
use oxa_interactive::Outcome;
use oxa_mcp::{load_config, project_config_path, AccessGate, ManagerOpts, McpManager, ServerState};
use oxa_proto::{codes, Exposure};
use oxa_tui::{Key, Theme};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn row(name: &str, state: McpRowState) -> McpServerRow {
    McpServerRow {
        name: name.to_string(),
        state,
        exposure: Exposure::Codemode,
        tool_count: 0,
        transport: "stdio".to_string(),
        defined_in: PathBuf::from(format!("/cfg/{name}/mcp.json")),
        project: false,
    }
}

fn with_exposure(mut r: McpServerRow, exposure: Exposure) -> McpServerRow {
    r.exposure = exposure;
    r
}

fn render_text(sel: &mut McpSelector) -> String {
    sel.render(100, &Theme::plain()).join("\n")
}

/// The server names of the list rows, in the order they are drawn.
fn listed_names(sel: &mut McpSelector) -> Vec<String> {
    let lines = sel.render(100, &Theme::plain());
    lines[1..]
        .iter()
        .take_while(|l| l.starts_with("→ ") || l.starts_with("  "))
        .filter_map(|l| {
            l.trim_start_matches(['→', ' '])
                .split_whitespace()
                .next()
                .map(str::to_string)
        })
        .collect()
}

fn press(sel: &mut McpSelector, id: &str) -> Outcome<McpRequest> {
    sel.key(&Key::press(id), &Keymap::defaults())
}

fn typed(sel: &mut McpSelector, c: char) -> Outcome<McpRequest> {
    sel.key(&Key::char(c), &Keymap::defaults())
}

fn manager(env: &TestEnv, trusted: bool, allow: bool) -> McpManager {
    let user = env.agent_dir().join("mcp.json");
    let project = project_config_path(&env.work());
    let config = load_config(&user, trusted.then_some(project.as_path()));
    let gate: AccessGate = Arc::new(move |_| allow);
    McpManager::new(ManagerOpts {
        config,
        cwd: env.work(),
        agent_dir: env.agent_dir(),
        home: env.home(),
        trusted,
        gate,
        client_version: "test".to_string(),
    })
}

/// A stdio entry for a command that does not exist (nothing is ever spawned by these tests
/// unless the failure itself is under test).
fn stdio_entry(extra: &str) -> String {
    format!(r#"{{"command": "oxa-f7-no-such-binary", "args": []{extra}}}"#)
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read mcp.json")
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&read_text(path)).expect("valid json")
}

fn wait_for(manager: &McpManager, name: &str, done: impl Fn(&ServerState) -> bool) -> ServerState {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = manager.server(name).expect("server exists").state;
        if done(&state) || Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// as mcp.md
#[test]
fn list_order() {
    // mcp.md: "Servers that need attention appear first"; then the rest in pi's order: starting,
    // connected, disabled; each group by name.
    let rows = vec![
        row("zeta", McpRowState::Connected),
        row("alpha", McpRowState::Disabled),
        row("beta", McpRowState::Failed("boom".to_string())),
        row("gamma", McpRowState::NeedsAuth),
        row("delta", McpRowState::Connecting),
    ];
    let mut sel = McpSelector::new(rows);
    assert_eq!(
        listed_names(&mut sel),
        ["gamma", "beta", "delta", "zeta", "alpha"]
    );

    // The same order from the manager: a server that is not started is "connecting", a disabled
    // one goes last.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/mcp.json",
        &format!(
            r#"{{"mcpServers": {{"c-off": {}, "a-on": {}, "b-off": {}}}}}"#,
            stdio_entry(", \"enabled\": false"),
            stdio_entry(""),
            stdio_entry(", \"enabled\": false"),
        ),
    );
    let names: Vec<String> = mcp_rows(&manager(&env, false, true))
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["a-on", "b-off", "c-off"]);
}

/// action
#[test]
fn reconnect() {
    let rows = vec![
        row("alpha", McpRowState::Connected),
        row("beta", McpRowState::Failed("boom".to_string())),
        row("off", McpRowState::Disabled),
    ];
    let mut sel = McpSelector::new(rows);
    // The failed server sorts first: Enter and `r` both reconnect it.
    assert_eq!(
        press(&mut sel, "enter"),
        Outcome::Done(McpRequest::Reconnect("beta".into()))
    );
    assert_eq!(
        typed(&mut sel, 'r'),
        Outcome::Done(McpRequest::Reconnect("beta".into()))
    );
    press(&mut sel, "down");
    assert_eq!(
        press(&mut sel, "enter"),
        Outcome::Done(McpRequest::Reconnect("alpha".into()))
    );
    // A disabled server cannot be reconnected; Enter turns it on instead.
    press(&mut sel, "down");
    assert_eq!(typed(&mut sel, 'r'), Outcome::Pending);
    assert!(render_text(&mut sel).contains("is disabled"));
    assert_eq!(
        press(&mut sel, "enter"),
        Outcome::Done(McpRequest::SetEnabled {
            server: "off".into(),
            enabled: true
        })
    );

    // The app carries the request out: the manager reconnects in the background. A gate that
    // refuses the server's start shows as "denied" without spawning anything.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/mcp.json",
        &format!(r#"{{"mcpServers": {{"alpha": {}}}}}"#, stdio_entry("")),
    );
    let denied = manager(&env, false, false);
    apply_mcp_request(&denied, &McpRequest::Reconnect("alpha".into())).expect("reconnect");
    assert_eq!(
        wait_for(&denied, "alpha", |s| *s == ServerState::Denied),
        ServerState::Denied
    );

    let unknown = apply_mcp_request(&denied, &McpRequest::Reconnect("nope".into())).unwrap_err();
    assert_eq!(unknown.code, codes::NOT_FOUND);
}

/// saved
#[test]
fn enable_disable() {
    let mut sel = McpSelector::new(vec![row("alpha", McpRowState::Connected)]);
    assert_eq!(
        typed(&mut sel, 'e'),
        Outcome::Done(McpRequest::SetEnabled {
            server: "alpha".into(),
            enabled: false
        })
    );
    assert!(render_text(&mut sel).contains("saved to /cfg/alpha/mcp.json"));

    // Saved to the file: unrelated fields of the entry stay as they were.
    let env = TestEnv::new();
    let file = env.write(
        "home/.oxa/agent/mcp.json",
        &format!(
            r#"{{"mcpServers": {{"alpha": {}}}}}"#,
            stdio_entry(", \"description\": \"keep me\"")
        ),
    );
    let mgr = manager(&env, false, true);
    apply_mcp_request(
        &mgr,
        &McpRequest::SetEnabled {
            server: "alpha".into(),
            enabled: false,
        },
    )
    .expect("disable");
    let doc = read_json(&file);
    assert_eq!(doc["mcpServers"]["alpha"]["enabled"], false);
    assert_eq!(doc["mcpServers"]["alpha"]["description"], "keep me");
    assert_eq!(mcp_rows(&mgr)[0].state, McpRowState::Disabled);

    apply_mcp_request(
        &mgr,
        &McpRequest::SetEnabled {
            server: "alpha".into(),
            enabled: true,
        },
    )
    .expect("enable");
    let doc = read_json(&file);
    assert_eq!(doc["mcpServers"]["alpha"]["enabled"], true);
    assert_eq!(doc["mcpServers"]["alpha"]["description"], "keep me");
    // Not started, so the server waits as "connecting" rather than connecting.
    assert_eq!(mcp_rows(&mgr)[0].state, McpRowState::Connecting);
}

/// user vs project file
#[test]
fn exposure_change_saved_to_defining_file() {
    let env = TestEnv::new();
    let user = env.write(
        "home/.oxa/agent/mcp.json",
        &format!(r#"{{"mcpServers": {{"user-srv": {}}}}}"#, stdio_entry("")),
    );
    let project = env.write(
        "work/.oxa/mcp.json",
        &format!(r#"{{"mcpServers": {{"proj-srv": {}}}}}"#, stdio_entry("")),
    );
    let mgr = manager(&env, true, true);
    let project_before = read_text(&project);

    // A user-level server: the change goes to the user file only.
    apply_mcp_request(
        &mgr,
        &McpRequest::SetExposure {
            server: "user-srv".into(),
            exposure: Exposure::Direct,
        },
    )
    .expect("exposure");
    assert_eq!(read_json(&user)["mcpServers"]["user-srv"]["exposure"], "direct");
    assert_eq!(read_text(&project), project_before);
    let user_after_first = read_text(&user);

    // A project-level server: the change goes to the project file only.
    apply_mcp_request(
        &mgr,
        &McpRequest::SetExposure {
            server: "proj-srv".into(),
            exposure: Exposure::Hidden,
        },
    )
    .expect("exposure");
    assert_eq!(
        read_json(&project)["mcpServers"]["proj-srv"]["exposure"],
        "hidden"
    );
    assert_eq!(read_text(&user), user_after_first);

    // The rows say which file each server is defined in.
    let rows = mcp_rows(&mgr);
    let user_row = rows.iter().find(|r| r.name == "user-srv").expect("row");
    let proj_row = rows.iter().find(|r| r.name == "proj-srv").expect("row");
    assert_eq!(user_row.defined_in, user);
    assert!(!user_row.project);
    assert_eq!(proj_row.defined_in, project);
    assert!(proj_row.project);
}

/// failed server
#[test]
fn error_state_shown() {
    let rows = vec![row(
        "broken",
        McpRowState::Failed("spawn failed: no such file".to_string()),
    )];
    let mut sel = McpSelector::new(rows);
    let text = render_text(&mut sel);
    assert!(text.contains("failed: spawn failed: no such file"), "{text}");
    assert!(text.contains("\nspawn failed: no such file"), "{text}");

    // A real failed connection: the manager reports it and the selector shows the error.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/mcp.json",
        &format!(r#"{{"mcpServers": {{"broken": {}}}}}"#, stdio_entry("")),
    );
    let mgr = manager(&env, false, true);
    mgr.start();
    let state = wait_for(&mgr, "broken", |s| matches!(s, ServerState::Failed(_)));
    assert!(matches!(state, ServerState::Failed(_)), "{state:?}");
    let rows = mcp_rows(&mgr);
    assert!(matches!(rows[0].state, McpRowState::Failed(ref m) if !m.is_empty()));
    let mut sel = McpSelector::new(rows);
    assert!(render_text(&mut sel).contains("failed: "));
    mgr.stop();
}

#[test]
fn exposure_cycles_with_x_and_says_where_it_is_saved() {
    let cycle = [
        (Exposure::Codemode, Exposure::Deferred),
        (Exposure::Deferred, Exposure::Direct),
        (Exposure::Direct, Exposure::Hidden),
        (Exposure::Hidden, Exposure::Codemode),
    ];
    for (from, to) in cycle {
        let mut sel = McpSelector::new(vec![with_exposure(row("srv", McpRowState::Connected), from)]);
        assert_eq!(
            typed(&mut sel, 'x'),
            Outcome::Done(McpRequest::SetExposure {
                server: "srv".into(),
                exposure: to
            })
        );
        let text = render_text(&mut sel);
        assert!(text.contains("saved to /cfg/srv/mcp.json"), "{text}");
        // Direct tools are frozen at the epoch start: the selector says so.
        assert_eq!(
            text.contains("/reload"),
            from == Exposure::Direct || to == Exposure::Direct
        );
    }
}

#[test]
fn escape_closes_and_empty_list_says_so() {
    let mut sel = McpSelector::new(Vec::new());
    assert!(render_text(&mut sel).contains("No MCP servers configured."));
    assert_eq!(press(&mut sel, "x"), Outcome::Ignored);
    assert_eq!(press(&mut sel, "escape"), Outcome::Cancelled);
}

#[test]
fn set_rows_keeps_the_cursor_on_the_same_server() {
    let mut sel = McpSelector::new(vec![
        row("a", McpRowState::Connected),
        row("b", McpRowState::Connected),
        row("c", McpRowState::Connected),
    ]);
    press(&mut sel, "down");
    sel.set_rows(vec![
        row("a", McpRowState::Connected),
        row("b", McpRowState::Connected),
        row("c", McpRowState::Connected),
        row("d", McpRowState::Connected),
    ]);
    assert_eq!(
        press(&mut sel, "enter"),
        Outcome::Done(McpRequest::Reconnect("b".into()))
    );
}
