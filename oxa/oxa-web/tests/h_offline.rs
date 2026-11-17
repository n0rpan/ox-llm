//! Lane `search`: offline mode (`OXA_OFFLINE=1` / `--offline` / `oxa.offline` install the
//! process policy): the tool is not declared, and a direct call is refused with the one
//! offline error before anything connects. One test: the policy is process-wide.
mod common;

use common::*;
use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, Hooks, Mode, Tool};
use oxa_web::{Brave, KeyInputs, WebConfig, WebSearchHooks, WebSearchTool};
use serde_json::json;
use std::sync::Arc;

#[test]
fn offline_declares_nothing_and_refuses_a_direct_call() {
    let env = TestEnv::new();
    let mock = Mock::start(|_, _| ok(&llm_body(1, "x")));
    let hooks = WebSearchHooks::new(WebConfig {
        key: KeyInputs {
            env_key: Some(key()),
            agent_dir: env.agent_dir(),
            isolation: true,
            real_home: oxa_web::real_home(),
        },
        opts: opts(&mock.url),
    });
    assert_eq!(hooks.tools().len(), 1, "online with a key: declared");

    oxa_proto::set_net_policy(true, false);
    assert!(hooks.tools().is_empty(), "offline: not declared");
    assert!(hooks.status_lines()[0].contains("off (offline mode)"));
    let tool = WebSearchTool::new(Arc::new(Brave::new(key(), opts(&mock.url))));
    let cwd = std::env::temp_dir();
    let (sb, cancel) = (NoSandbox, CancelToken::new());
    let cx = ctx(&cwd, &sb, Mode::Workspace, &cancel);
    let expected = "Refused: web_search (127.0.0.1) needs the network: offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline)\nFix: turn offline mode off: unset OXA_OFFLINE, drop --offline, set \"oxa.offline\": false";
    let out = tool.run(&json!({"query": "q"}), &cx, &mut |_| {});
    assert!(out.is_error);
    assert_eq!(out.text, expected);
    assert_eq!(tool.access(&json!({"query": "q"}), &cx).unwrap_err(), expected);
    // The real host is refused at the connect site too (no isolation needed for that).
    let real = Brave::new(key(), oxa_web::BraveOpts::default());
    let err = real
        .search(
            &oxa_web::Query {
                q: "q".into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap_err();
    assert!(
        err.starts_with("Refused: web_search (api.search.brave.com) needs the network"),
        "{err}"
    );
    assert_eq!(mock.requests().len(), 0);
    assert_eq!(real.shared().requests(), 0);
    oxa_proto::set_net_policy(false, false);
}
