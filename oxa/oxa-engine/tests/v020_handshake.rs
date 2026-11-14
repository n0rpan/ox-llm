//! Version / capability handshake (self-feedback): a mock server advertising unknown or missing
//! capabilities gives one warning line from the `info` read already made at start; a matching
//! server gives none. The mock answers the controller's three documents; nothing but loopback.

use oxa_engine::{HttpEngine, NEEDED_CAPABILITIES, PROTOCOL_UNDERSTOOD};
use oxa_proto::{Engine, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

const READY: &str = include_str!("fixtures/controller_status_ready.json");
const PROPS: &str = include_str!("fixtures/engine_props_oxe.json");
const MODELS: &str = r#"{"object":"list","data":[{"id":"ornith","object":"model"}]}"#;

/// A loopback server that answers GETs from a path -> (status, body) table; anything else 404.
/// Returns the base URL and the list of paths it was asked for.
fn routes(table: Vec<(&'static str, u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let table: HashMap<&str, (u16, String)> = table.into_iter().map(|(p, c, b)| (p, (c, b))).collect();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let path = read_path(&mut s);
            log.lock().unwrap().push(path.clone());
            let (code, body) = table
                .get(path.as_str())
                .cloned()
                .unwrap_or((404, r#"{"error":{"code":404,"message":"not found"}}"#.to_string()));
            let head = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
            let _ = s.shutdown(Shutdown::Both);
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn read_path(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let line = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    line.split_whitespace().nth(1).unwrap_or("").to_string()
}


fn status_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(READY).unwrap();
    edit(&mut v);
    v.to_string()
}

fn warning_for(status: String) -> (Option<String>, Vec<String>) {
    let (base, seen) = routes(vec![
        ("/v1/models", 200, MODELS.to_string()),
        ("/_local/status", 200, status),
        ("/props", 200, PROPS.to_string()),
    ]);
    let engine = HttpEngine::new(&base, None).unwrap();
    assert_eq!(engine.handshake_warning(), None, "nothing before the first info");
    engine.info(None).expect("info");
    let asked = seen.lock().unwrap().clone();
    (engine.handshake_warning(), asked)
}

#[test]
fn a_matching_controller_gives_no_warning() {
    let (w, _) = warning_for(READY.to_string());
    assert_eq!(w, None);
}

#[test]
fn a_newer_protocol_warns_once_without_extra_requests() {
    let newer = PROTOCOL_UNDERSTOOD + 1;
    let (w, asked) = warning_for(status_with(|v| v["protocol_version"] = newer.into()));
    let w = w.expect("a warning");
    assert!(w.contains("oxc offers features this oxa doesn't know"), "{w}");
    assert!(w.contains("update oxa"), "{w}");
    let plain = warning_for(READY.to_string()).1;
    assert_eq!(asked.len(), plain.len(), "the handshake adds no request: {asked:?}");
}

#[test]
fn a_controller_without_a_needed_capability_warns() {
    let (w, _) = warning_for(status_with(|v| {
        v["capabilities"] = serde_json::json!(["models", "storage"]);
    }));
    let w = w.expect("a warning");
    assert!(w.contains("context_modes") && w.contains("update oxc"), "{w}");
    assert!(NEEDED_CAPABILITIES.contains(&"context_modes"));
}

#[test]
fn unknown_extra_capabilities_are_not_a_warning() {
    let (w, _) = warning_for(status_with(|v| {
        v["capabilities"].as_array_mut().unwrap().push("telepathy".into());
        v["protocol_version"] = PROTOCOL_UNDERSTOOD.into();
    }));
    assert_eq!(w, None);
}

#[test]
fn a_plain_engine_with_a_newer_protocol_warns_about_the_engine() {
    let props = serde_json::json!({"n_ctx": 8192, "oxe_hints": {"protocol_version": 99, "hints": []}}).to_string();
    let (base, _) = routes(vec![
        ("/v1/models", 200, MODELS.to_string()),
        ("/props", 200, props),
    ]);
    let engine = HttpEngine::new(&base, None).unwrap();
    engine.info(None).expect("info");
    let w = engine.handshake_warning().expect("a warning");
    assert!(w.starts_with("the engine offers features this oxa doesn't know"), "{w}");
}
