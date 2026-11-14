//! P3 / P4 (wave H lane `local`): the engine client. A host that is not loopback is refused
//! when the client is built unless `oxa.engine.allowRemote`; offline, every connect (engine
//! and `http_request`, which extension fetch uses) is loopback only. The tests share the process network policy, so they hold one lock each.

use oxa_engine::HttpEngine;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
use oxa_proto::{codes, set_net_policy, Engine};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static POLICY: Mutex<()> = Mutex::new(());

#[test]
fn remote_host_is_refused_without_connecting() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    set_net_policy(false, false);
    let t = Instant::now();
    let e = HttpEngine::new("http://10.255.0.2:8092", None).unwrap_err();
    assert_eq!(e.code, codes::REMOTE_ENGINE);
    assert!(t.elapsed() < Duration::from_secs(1), "no connect was attempted");
    assert_eq!(
        e.to_string(),
        "Refused to use the engine at 10.255.0.2: it is not a loopback address, so prompts and files would leave this machine over plain HTTP\nFix: set \"oxa.engine.allowRemote\": true in the user settings.json, or use an engine on 127.0.0.1"
    );
}

#[test]
fn offline_refuses_a_remote_engine_even_when_allowed() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    set_net_policy(true, true);
    let e = HttpEngine::new("http://10.255.0.2:8092", None).unwrap_err();
    set_net_policy(false, false);
    assert_eq!(e.code, codes::OFFLINE);
    assert_eq!(
        e.to_string(),
        "Refused: the engine (10.255.0.2) needs the network: offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline)\nFix: turn offline mode off: unset OXA_OFFLINE, drop --offline, set \"oxa.offline\": false"
    );
}

#[test]
fn offline_keeps_the_loopback_engine() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let server = FakeServer::start(vec![ScriptTurn::text("a")], FakeOpts::default()).unwrap();
    set_net_policy(true, false);
    let info = HttpEngine::new(&server.url(), None).unwrap().info(None);
    set_net_policy(false, false);
    assert!(info.is_ok(), "{info:?}");
}

#[test]
fn an_engine_built_before_offline_is_still_refused_at_connect() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    set_net_policy(false, true);
    let engine = HttpEngine::new("http://10.255.0.2:8092", None).unwrap();
    set_net_policy(true, true);
    let t = Instant::now();
    let e = engine.info(None).unwrap_err();
    set_net_policy(false, false);
    assert_eq!(e.code, codes::OFFLINE);
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(
        e.message
            .starts_with("Refused: a connection (10.255.0.2) needs the network"),
        "{e}"
    );
}
