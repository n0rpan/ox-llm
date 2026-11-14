//! X1: engine discovery against the real local-ai controller.
//!
//! The fixtures under `tests/fixtures/` follow the lab controller's own code, not oxa's
//! preferred shape: `controller_status_*.json` is `StatusMixin.status` (`runtime/ctl_status.py`,
//! key order of `tests/test_controller_contract.py` `STATUS_KEYS` / `BUSY_KEYS`, the `context`
//! block of `profile_policy.context_status` with the D5 `compact_at` per mode of
//! `harness_registry.COMPACT_AT`); `engine_props_oxe.json` is the oxe server's `/props`
//! (`service.rs` `props`, with `oxe_hints`). The context is `context_size` / `context.cap`
//! (never `n_ctx`), the queue `queued_requests`, and no output limit is published.
//! Nothing outside 127.0.0.1 is contacted.

use oxa_engine::{fetch_info, parse_info, parse_info_for, HttpEngine, Url, DEFAULT_MAX_OUTPUT};
use oxa_proto::{codes, Engine, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

const READY: &str = include_str!("fixtures/controller_status_ready.json");
const BUSY: &str = include_str!("fixtures/controller_status_busy.json");
const NO_MODES: &str = include_str!("fixtures/controller_status_no_modes.json");
const PROPS: &str = include_str!("fixtures/engine_props_oxe.json");
const MODELS: &str =
    r#"{"object":"list","data":[{"id":"ornith","object":"model"},{"id":"moe","object":"model"}]}"#;

fn json(s: &str) -> Value {
    serde_json::from_str(s).expect("fixture is JSON")
}

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

/// The real controller on its port, model loaded: context from `context.cap`, queue from
/// `queued_requests`, compaction point of the selected mode, H6 from the proxied `/props`.
#[test]
fn http_engine_info_reads_the_real_controller_status() {
    let (base, seen) = routes(vec![
        ("/v1/models", 200, MODELS.to_string()),
        ("/_local/status", 200, READY.to_string()),
        ("/props", 200, PROPS.to_string()),
    ]);
    let engine = HttpEngine::new(&base, None).unwrap();
    let info = engine
        .info(None)
        .expect("the real controller status is understood");
    assert_eq!(info.n_ctx, 131072);
    assert_eq!(info.model, "ornith");
    assert_eq!(info.models, vec!["ornith".to_string(), "moe".to_string()]);
    assert_eq!(info.queue, 2);
    assert_eq!(info.compact_at, Some(102000), "deep is selected");
    assert_eq!(
        info.max_output, DEFAULT_MAX_OUTPUT,
        "no output limit published: the default"
    );
    assert!(info.prefill_only, "oxe_hints.prefill_only from /props");
    let asked = seen.lock().unwrap().clone();
    assert!(
        asked.contains(&"/_local/status".to_string()) && asked.contains(&"/props".to_string()),
        "{asked:?}"
    );

    // An ordinary request can be built from what was learnt: a generating max_tokens > 0.
    assert!(info.max_output > 0 && info.max_output < info.n_ctx);
}

/// No model loaded: the controller answers `/props` with 503 (it never starts a model for it).
/// Discovery still succeeds from the status; no capabilities are advertised, so idle prefill
/// is off.
#[test]
fn props_unavailable_means_no_capabilities() {
    let (base, _) = routes(vec![
        ("/v1/models", 200, MODELS.to_string()),
        ("/_local/status", 200, NO_MODES.to_string()),
        (
            "/props",
            503,
            r#"{"error":{"message":"Local backend could not be started"}}"#.to_string(),
        ),
    ]);
    let info = fetch_info(&Url::parse(&base).unwrap(), None).unwrap();
    assert_eq!(
        info.n_ctx, 16384,
        "context_size / context.cap of a profile without modes"
    );
    assert_eq!(info.model, "qwen3.6-35b-a3b-local");
    assert_eq!(info.compact_at, None, "no mode, no compaction point");
    assert_eq!(info.max_output, 4096, "a quarter of a small context");
    assert_eq!(info.queue, 0);
    assert!(!info.prefill_only);
}

/// The busy reply (the controller's lock is held while a model loads) has no context; the
/// cached `/props` n_ctx fills it. Queue from `active.queued_requests`.
#[test]
fn busy_status_falls_back_to_props_context() {
    let models = json(MODELS);
    let mut status = json(BUSY);
    let props = json(PROPS);
    status["default_generation_settings"] = props["default_generation_settings"].clone();
    status["oxe_hints"] = props["oxe_hints"].clone();
    let info = parse_info("http://h:1", &models, &status).unwrap();
    assert_eq!(info.n_ctx, 131072);
    assert_eq!(info.queue, 1);
    assert_eq!(
        info.model, "ornith",
        "first /v1/models id when the busy reply names none"
    );

    // Busy (loading) and no /props: a clear, retryable engine_busy error that says to try
    // again (F14).
    let err = parse_info("http://h:1", &models, &json(BUSY)).unwrap_err();
    assert_eq!(err.code, codes::ENGINE_BUSY);
    assert!(err.hint.as_deref().unwrap_or("").contains("loading"), "{err:?}");
}

/// Field-by-field mapping on the ready fixture, and each fallback in its order.
#[test]
fn parse_info_mapping_and_precedence() {
    let models = json(MODELS);
    let ready = json(READY);
    assert!(
        ready.get("n_ctx").is_none() && ready.get("queue").is_none() && ready.get("max_output").is_none(),
        "the fixture is the controller's shape, not oxa's"
    );
    let info = parse_info("http://h:1", &models, &ready).unwrap();
    assert_eq!(
        (info.n_ctx, info.queue, info.compact_at, info.prefill_only),
        (131072, 2, Some(102000), false)
    );

    // context.cap (mode-applied) wins over context_size.
    let mut v = ready.clone();
    v["context_size"] = Value::from(8192);
    assert_eq!(parse_info("h", &models, &v).unwrap().n_ctx, 131072);
    // Without the context block: context_size.
    v.as_object_mut().unwrap().remove("context");
    let i = parse_info("h", &models, &v).unwrap();
    assert_eq!((i.n_ctx, i.compact_at), (8192, None));

    // Plain engine /props as the status (no controller).
    let p = parse_info("h", &models, &json(PROPS)).unwrap();
    assert_eq!(
        (p.n_ctx, p.model.as_str(), p.prefill_only),
        (131072, "ornith", true)
    );

    // A published max_output is used as is.
    let mut m = ready.clone();
    m["max_output"] = Value::from(2048);
    assert_eq!(parse_info("h", &models, &m).unwrap().max_output, 2048);
}

/// The controller's per-model `model_limits` (lab `ModelsMixin.model_limits`, documented in lab
/// `docs/UI-DATA.md`), keyed by `/v1/models` id: Ornith (loaded) in deep mode, an unloaded 16K
/// Flash with the 2048 qualified output cap.
fn with_model_limits(mut status: Value) -> Value {
    status["model_limits"] = serde_json::json!({
        "ornith": {"cap": 131072, "mode": "deep", "modes": [
            {"id": "fast", "compact_at": 49536, "cap": 131072, "available": true, "reason": null},
            {"id": "deep", "compact_at": 102000, "cap": 131072, "available": true, "reason": null}],
            "max_output": 16384},
        "flash": {"cap": 16384, "mode": null, "modes": [], "max_output": 2048},
        "small": {"cap": 8192, "mode": null, "modes": [], "max_output": null},
    });
    status
}

/// X3: `info(Some(target))` reads the target's published limits even when it is not loaded;
/// `limits_for` says whose limits they are. Without a row: the loaded model's.
#[test]
fn target_limits_come_from_model_limits() {
    let models = json(MODELS);
    let status = with_model_limits(json(READY));
    let f = parse_info_for("h", &models, &status, Some("flash")).unwrap();
    assert_eq!((f.model.as_str(), f.limits_for.as_str()), ("ornith", "flash"));
    assert_eq!((f.n_ctx, f.max_output, f.compact_at), (16384, 2048, None));
    let small = parse_info_for("h", &models, &status, Some("small")).unwrap();
    assert_eq!(
        (small.n_ctx, small.max_output),
        (8192, 2048),
        "null max_output: a quarter of the cap"
    );
    let o = parse_info_for("h", &models, &status, None).unwrap();
    assert_eq!(
        (o.limits_for.as_str(), o.n_ctx, o.max_output, o.compact_at),
        ("ornith", 131072, 16384, Some(102000))
    );
    // An unknown target, or an older controller without model_limits: the loaded model's.
    let u = parse_info_for("h", &models, &status, Some("unknown")).unwrap();
    assert_eq!((u.limits_for.as_str(), u.n_ctx), ("ornith", 131072));
    let old = parse_info_for("h", &models, &json(READY), Some("flash")).unwrap();
    assert_eq!(
        (old.limits_for.as_str(), old.n_ctx, old.max_output),
        ("ornith", 131072, DEFAULT_MAX_OUTPUT)
    );

    // Over HTTP, through the Engine trait.
    let (base, _) = routes(vec![
        ("/v1/models", 200, MODELS.to_string()),
        ("/_local/status", 200, status.to_string()),
        ("/props", 200, PROPS.to_string()),
    ]);
    let engine = HttpEngine::new(&base, None).unwrap();
    let info = engine.info(Some("flash")).unwrap();
    assert_eq!(
        (info.limits_for.as_str(), info.n_ctx, info.max_output),
        ("flash", 16384, 2048)
    );
}

/// The fake engine now serves the controller's shape and the engine's /props, so a test
/// against it exercises the same mapping.
#[test]
fn the_fake_serves_the_real_shapes() {
    let server = oxa_fake::FakeServer::start(
        vec![],
        oxa_fake::FakeOpts {
            n_ctx: 32768,
            ..Default::default()
        },
    )
    .unwrap();
    let info = fetch_info(&Url::parse(&server.url()).unwrap(), None).unwrap();
    assert_eq!(info.n_ctx, 32768);
    assert_eq!(info.max_output, 8192);
    assert!(info.prefill_only);
    let no_hints = oxa_fake::FakeServer::start(
        vec![],
        oxa_fake::FakeOpts {
            hints: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !fetch_info(&Url::parse(&no_hints.url()).unwrap(), None)
            .unwrap()
            .prefill_only
    );
}
