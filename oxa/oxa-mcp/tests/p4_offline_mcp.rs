//! P4 (wave H): offline, the MCP HTTP transport and the OAuth requests reach no
//! host that is not loopback; the error is the one offline shape. One process, one policy.

use oxa_mcp::{discover, HttpConfig, HttpTransport, OAuthSettings, Transport};
use oxa_proto::{codes, set_net_policy};
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn offline_refuses_remote_mcp_http_and_oauth() {
    set_net_policy(true, false);
    let cfg = HttpConfig {
        url: "http://10.255.0.2:9/mcp".to_string(),
        headers: Vec::new(),
        oauth: None,
        proxy: None,
    };
    let mut t = HttpTransport::new(&cfg, Vec::new(), None, Duration::from_secs(10));
    let started = Instant::now();
    let e = t
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .unwrap_err();
    assert_eq!(e.code, codes::OFFLINE);
    assert_eq!(
        e.to_string(),
        "Refused: MCP over HTTP (10.255.0.2) needs the network: offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline)\nFix: turn offline mode off: unset OXA_OFFLINE, drop --offline, set \"oxa.offline\": false"
    );
    t.close();
    let settings = OAuthSettings {
        auth_server_metadata_url: Some(
            "https://as.example.com/.well-known/oauth-authorization-server".to_string(),
        ),
        ..OAuthSettings::default()
    };
    let e = discover("https://mcp.example.com/mcp", &settings).unwrap_err();
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "no connect was attempted"
    );
    set_net_policy(false, false);
}
