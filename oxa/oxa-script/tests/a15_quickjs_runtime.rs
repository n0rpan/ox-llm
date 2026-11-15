//! A15 acceptance tests: QuickJS runtime.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_proto::{codes, OxaError, Value};
use oxa_script::{strip_types, FnRef, HostFn, Limits, MapLoader, Runtime, RuntimeThread};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn host(f: impl Fn(&mut oxa_script::CallCx, &[Value]) -> Result<Value, OxaError> + Send + 'static) -> HostFn {
    Box::new(f)
}

/// JS calls a registered host fn and gets its JSON result
#[test]
fn host_function_binding() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    rt.register(
        "oxa.add",
        host(|_cx, args| {
            let a = args[0].as_i64().unwrap();
            let b = args[1].as_i64().unwrap();
            Ok(json!(a + b))
        }),
    );
    rt.register("oxa.obj", host(|_cx, _args| Ok(json!({"a": [1, 2], "b": "x"}))));
    assert_eq!(rt.eval_script("t", "oxa.add(2, 3) * 10").unwrap(), json!(50));
    assert_eq!(
        rt.eval_script("t", "oxa.obj()").unwrap(),
        json!({"a": [1, 2], "b": "x"})
    );
    // Dotted paths create the namespace object; a later registration replaces the function.
    rt.register("oxa.add", host(|_cx, _args| Ok(json!("replaced"))));
    assert_eq!(rt.eval_script("t", "oxa.add(1, 1)").unwrap(), json!("replaced"));
}

/// and back to a script error
#[test]
fn host_error_becomes_js_exception() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    rt.register(
        "oxa.fail",
        host(|_cx, _args| Err(OxaError::new(codes::DENIED, "nope"))),
    );
    // Caught in the script: the error carries the message and the code.
    let caught = rt
        .eval_script("t", "try { oxa.fail() } catch (e) { e.code + ':' + e.message }")
        .unwrap();
    assert_eq!(caught, json!("denied:nope"));
    // Uncaught: it comes back to Rust as an error that keeps its stable code (wave H: an
    // omitted API is reported as `unsupported_in_oxa`, not as a generic `script` error).
    let err = rt.eval_script("t", "oxa.fail()").unwrap_err();
    assert_eq!(err.code, codes::DENIED);
    assert!(err.message.contains("nope"), "message was {:?}", err.message);
}

/// import through the embedder's loader; unknown module fails
#[test]
fn es_module_loader() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    rt.set_loader(Box::new(
        MapLoader::new()
            .with(
                "lib/math.js",
                "export const x = 41; export default function inc(a) { return a + 1; }",
            )
            .with(
                "lib/main.js",
                "import inc, { x } from './math.js'; export const r = inc(x);",
            ),
    ));
    let m = rt
        .eval_module(
            "app.js",
            "import { r } from './lib/main.js'; export const answer = r;",
        )
        .unwrap();
    let names = rt.exports(&m).unwrap();
    assert_eq!(names, vec!["answer".to_string()]);
    assert_eq!(rt.eval_script("t", "1").unwrap(), json!(1));

    let f = rt
        .eval_module(
            "f.js",
            "import inc from './lib/math.js'; export default function go(a) { return inc(a) * 2; }",
        )
        .unwrap();
    assert_eq!(rt.call_export(&f, "default", &[json!(4)]).unwrap(), json!(10));

    let missing = rt.eval_module("bad.js", "import './nope.js'; export const z = 1;");
    assert!(missing.is_err(), "unknown module must fail");
    let node = rt.eval_module("node.js", "import fs from 'node:fs'; export const z = fs;");
    let e = node.unwrap_err();
    assert!(e.hint.unwrap_or_default().contains("not available in oxa"));
}

/// allocation loop -> script error, runtime usable afterwards or cleanly dead
#[test]
fn memory_cap_interrupts() {
    let limits = Limits {
        memory_bytes: 4 << 20,
        deadline_ms: Some(20_000),
        stack_bytes: 1 << 20,
    };
    let mut rt = Runtime::new(limits).unwrap();
    let err = rt
        .eval_script(
            "t",
            "var a = []; while (true) { a.push(new Array(10000).fill(7)); }",
        )
        .unwrap_err();
    assert_eq!(err.code, codes::SCRIPT);
    // Whatever the outcome, the process is intact and a fresh call still works.
    assert_eq!(rt.eval_script("t", "1 + 1").unwrap(), json!(2));
    assert!(rt.memory_used() <= 4 << 20);
}

/// `while(true){}` stops at the deadline
#[test]
fn deadline_interrupts_runaway() {
    let limits = Limits {
        memory_bytes: 16 << 20,
        deadline_ms: Some(200),
        stack_bytes: 1 << 20,
    };
    let mut rt = Runtime::new(limits).unwrap();
    let started = Instant::now();
    let err = rt.eval_script("t", "while (true) {}").unwrap_err();
    assert_eq!(err.code, codes::SCRIPT);
    assert!(err.message.contains("deadline"), "message was {:?}", err.message);
    assert!(started.elapsed() < Duration::from_secs(10));
    // The next call gets a fresh deadline.
    assert_eq!(rt.eval_script("t", "2 * 21").unwrap(), json!(42));
}

/// no require/fetch/process/std globals
#[test]
fn no_ambient_fs_or_net() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    let kinds = rt
        .eval_script(
            "t",
            "['require', 'fetch', 'process', 'std', 'os', 'setTimeout', 'Deno', 'Bun', 'XMLHttpRequest', 'WebSocket']
                .map(n => typeof globalThis[n]).join(',')",
        )
        .unwrap();
    assert_eq!(
        kinds,
        json!("undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined")
    );
    assert_eq!(
        rt.eval_script("t", "typeof console.log").unwrap(),
        json!("function")
    );
}

/// a JS closure passed to a host fn can be called later
#[test]
fn callbacks_as_fnref() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    let saved: Arc<Mutex<Option<FnRef>>> = Arc::new(Mutex::new(None));
    let s = saved.clone();
    rt.register(
        "oxa.on",
        host(move |_cx, args| {
            let f = FnRef::from_value(&args[0])
                .ok_or_else(|| OxaError::new(codes::INVALID, "want a function"))?;
            *s.lock().unwrap() = Some(f);
            Ok(json!(true))
        }),
    );
    assert_eq!(
        rt.eval_script("t", "oxa.on(function (x) { return x * 2; })")
            .unwrap(),
        json!(true)
    );
    let f = saved.lock().unwrap().unwrap();
    assert_eq!(rt.call(&f, &[json!(21)]).unwrap(), json!(42));
    // A host function can call the closure too, while the script waits.
    let g: Arc<Mutex<Option<FnRef>>> = Arc::new(Mutex::new(None));
    let g2 = g.clone();
    rt.register(
        "oxa.keep",
        host(move |_cx, args| {
            *g2.lock().unwrap() = FnRef::from_value(&args[0]);
            Ok(Value::Null)
        }),
    );
    rt.register(
        "oxa.twice",
        host(move |cx, args| {
            let f = FnRef::from_value(&args[0]).unwrap();
            cx.call(&f, &[json!(5)])
        }),
    );
    rt.eval_script("t", "oxa.keep(function (x) { return 'got ' + x; })")
        .unwrap();
    let kept = g.lock().unwrap().unwrap();
    assert_eq!(rt.call(&kept, &[json!("me")]).unwrap(), json!("got me"));
    assert_eq!(
        rt.eval_script("t", "oxa.twice(function (x) { return x + 1; })")
            .unwrap(),
        json!(6)
    );
    // After release the reference is stale.
    rt.release(&f);
    assert!(rt.call(&f, &[json!(1)]).is_err());
}

/// async function result is awaited
#[test]
fn promises_are_pumped() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    assert_eq!(
        rt.eval_script("t", "(async () => { await null; return 7; })()")
            .unwrap(),
        json!(7)
    );
    assert_eq!(
        rt.eval_async_body("t", "const v = await Promise.resolve(5); return v * 2;")
            .unwrap(),
        json!(10)
    );
    // A promise that can never settle is an error, not a hang.
    let err = rt.eval_script("t", "new Promise(() => {})").unwrap_err();
    assert_eq!(err.code, codes::SCRIPT);
    // A rejected promise is a script error.
    let rejected = rt
        .eval_script("t", "Promise.reject(new Error('bad'))")
        .unwrap_err();
    assert!(rejected.message.contains("bad"));
    // register_async: the host result arrives through a promise, also in allSettled.
    rt.register_async(
        "oxa.later",
        host(|_cx, args| Ok(json!(args[0].as_i64().unwrap() + 1))),
    );
    assert_eq!(
        rt.eval_async_body("t", "return await oxa.later(41);").unwrap(),
        json!(42)
    );
    assert_eq!(
        rt.eval_async_body(
            "t",
            "const r = await Promise.allSettled([oxa.later(1)]); return r[0].status;"
        )
        .unwrap(),
        json!("fulfilled")
    );
}

/// calls from another thread
#[test]
fn runtime_thread_is_send() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RuntimeThread>();

    let th = Arc::new(RuntimeThread::spawn("a15-test").unwrap());
    let id = th
        .run(|slots| slots.create(Limits::EXTENSION), &mut |_, _| Value::Null)
        .unwrap()
        .unwrap();
    let th2 = th.clone();
    let handle = std::thread::spawn(move || {
        th2.run(
            move |slots| {
                let rt = slots
                    .get(id)
                    .ok_or_else(|| OxaError::new(codes::NOT_FOUND, "no slot"))?;
                rt.eval_script("t", "1 + 2")
            },
            &mut |_, _| Value::Null,
        )
    });
    let out = handle.join().unwrap().unwrap();
    assert_eq!(out.unwrap(), json!(3));
    th.shutdown();
    th.shutdown();
}

/// Upcalls: a script blocks in a host fn while the submitter answers; callbacks go back to it.
#[test]
fn runtime_thread_upcalls() {
    let th = RuntimeThread::spawn("a15-upcall").unwrap();
    let id = th
        .run(
            |slots| {
                let slot = slots.create(Limits::EXTENSION)?;
                let rt = slots.get(slot).unwrap();
                rt.register("oxa.ask", host(|cx, _args| cx.upcall(json!({"q": "name?"}))));
                Ok::<_, OxaError>(slot)
            },
            &mut |_, _| Value::Null,
        )
        .unwrap()
        .unwrap();
    let answer = th
        .run(
            move |slots| {
                let rt = slots.get(id).unwrap();
                rt.eval_script("t", "'hi ' + oxa.ask()")
            },
            &mut |req, port| {
                assert_eq!(req, json!({"q": "name?"}));
                // Call back into the script while it is blocked in the upcall.
                let f = port.call(&FnRef(0), &[]);
                assert!(f.is_err());
                json!("ada")
            },
        )
        .unwrap();
    assert_eq!(answer.unwrap(), json!("hi ada"));
}

/// strip_types -> unsupported_in_oxa
#[test]
fn ts_rejected_with_hint() {
    let err = strip_types("let x: number = 1;").unwrap_err();
    assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA);
    assert!(err.hint.unwrap_or_default().contains("tsc"));
}

/// The bridge: bytes, errors and JSON edge cases survive a round trip.
#[test]
fn value_bridge_round_trip() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    rt.register("oxa.echo", host(|_cx, args| Ok(args[0].clone())));
    let bytes = rt.eval_script("t", "new Uint8Array([1, 2, 255])").unwrap();
    assert_eq!(bytes, json!({"$bytes": "AQL/"}));
    let back = rt.eval_script("t", "oxa.echo({'$bytes': 'AQL/'})").unwrap();
    assert_eq!(back, json!({"$bytes": "AQL/"}));
    let err = rt.eval_script("t", "new TypeError('x')").unwrap();
    assert_eq!(err["$error"]["name"], json!("TypeError"));
    assert_eq!(err["$error"]["message"], json!("x"));
    assert_eq!(
        rt.eval_script("t", "[1.5, null, undefined, true]").unwrap(),
        json!([1.5, null, null, true])
    );
    // Cycles are refused rather than overflowing the stack.
    let cyc = rt.eval_script("t", "const o = {}; o.self = o; o");
    assert!(cyc.is_err());
}

/// console output reaches the sink; the bridge is not reachable from scripts as a global.
#[test]
fn console_goes_to_the_sink() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let seen: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    rt.set_console(Box::new(move |level, line| {
        s.borrow_mut().push((level.to_string(), line.to_string()));
    }));
    rt.eval_script("t", "console.warn('a', 1, {b: 2}); console.log('done');")
        .unwrap();
    assert_eq!(
        *seen.borrow(),
        vec![
            ("warn".to_string(), "a 1 {\"b\":2}".to_string()),
            ("log".to_string(), "done".to_string()),
        ]
    );
    assert_eq!(
        rt.eval_script("t", "typeof __oxa_helpers").unwrap(),
        json!("undefined")
    );
}

/// A port used from another thread after its `serve` returned is refused at once (`script`);
/// it must not block until the job ends.
#[test]
fn callback_port_is_dead_after_serve_returns() {
    let th = RuntimeThread::spawn("a15-stale-port").unwrap();
    let id = th
        .run(
            |slots| {
                let slot = slots.create(Limits::EXTENSION)?;
                let rt = slots.get(slot).unwrap();
                rt.register("oxa.ask", host(|cx, _args| cx.upcall(json!(1))));
                Ok::<_, OxaError>(slot)
            },
            &mut |_, _| Value::Null,
        )
        .unwrap()
        .unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let mut helper = None;
    let mut done_tx = Some(done_tx);
    let out = th
        .run(
            move |slots| {
                let v = slots.get(id).unwrap().eval_script("t", "oxa.ask()");
                // The job stays busy until the helper has seen the refusal.
                done_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("stale call was refused");
                v
            },
            &mut |_, port| {
                let port = port.clone();
                let tx = done_tx.take().expect("one upcall");
                helper = Some(std::thread::spawn(move || loop {
                    let err = port.call(&FnRef(0), &[]).expect_err("FnRef(0) never resolves");
                    assert_eq!(err.code, codes::SCRIPT);
                    if err.message.contains("after its upcall was answered") {
                        let _ = tx.send(());
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }));
                json!(2)
            },
        )
        .unwrap();
    assert_eq!(out.unwrap(), json!(2));
    helper.unwrap().join().unwrap();
    th.shutdown();
}

/// H (extapi): a host function releases a function it received for its own call only; a thrown
/// error keeps its stable `code`.
#[test]
fn host_fn_releases_a_callback_and_errors_keep_their_code() {
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    let seen: Arc<Mutex<Option<FnRef>>> = Arc::new(Mutex::new(None));
    let s = seen.clone();
    rt.register(
        "oxa.once",
        host(move |cx, args| {
            let f = FnRef::from_value(&args[0]).unwrap();
            let out = cx.call(&f, &[json!(1)]);
            cx.release(&f);
            *s.lock().unwrap() = Some(f);
            out
        }),
    );
    assert_eq!(
        rt.eval_script("t", "oxa.once(function (x) { return x + 1; })")
            .unwrap(),
        json!(2)
    );
    let f = seen.lock().unwrap().unwrap();
    assert!(rt.call(&f, &[json!(1)]).is_err(), "released");

    rt.register(
        "oxa.omitted",
        host(|_cx, _args| Err(OxaError::unsupported("oxa.omitted"))),
    );
    let e = rt.eval_script("t", "oxa.omitted()").unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED_IN_OXA);
    let e = rt
        .eval_script("t", "const x = new Error('m'); x.code = 'not_a_code'; throw x;")
        .unwrap_err();
    assert_eq!(e.code, codes::SCRIPT, "an unknown code stays script");
}
