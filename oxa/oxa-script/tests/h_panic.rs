//! Wave H lane `harden` (pre-flight A1/A2/B9): the release binary unwinds, so a panicking host
//! function is an exception in the script (the runtime goes on) and a panicking job on the
//! script thread is an error for that job (the thread goes on). These pass in debug and release.

use oxa_proto::{codes, OxaError, Value};
use oxa_script::{HostFn, Limits, Runtime, RuntimeThread};
use serde_json::json;

fn host(f: impl Fn(&mut oxa_script::CallCx, &[Value]) -> Result<Value, OxaError> + Send + 'static) -> HostFn {
    Box::new(f)
}

#[test]
fn a_panicking_host_function_is_an_exception_and_the_runtime_goes_on() {
    oxa_proto::panic::install_hook();
    let mut rt = Runtime::new(Limits::EXTENSION).unwrap();
    rt.register("oxa.boom", host(|_cx, _args| panic!("injected host panic")));
    rt.register("oxa.ok", host(|_cx, _args| Ok(json!("ok"))));
    let caught = rt
        .eval_script(
            "t",
            "let m = ''; try { oxa.boom(); } catch (e) { m = String(e.message); } m",
        )
        .expect("the script catches the exception");
    let text = caught.as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("host function panicked") && text.contains("injected host panic"),
        "{text}"
    );
    let e = rt
        .eval_script("t", "oxa.boom()")
        .expect_err("uncaught = script error");
    assert!(e.message.contains("injected host panic"), "{e}");
    assert_eq!(rt.eval_script("t", "oxa.ok()").unwrap(), json!("ok"));
}

#[test]
fn a_panicking_job_fails_that_job_and_the_thread_goes_on() {
    oxa_proto::panic::install_hook();
    let thread = RuntimeThread::spawn("h-panic").expect("spawn");
    let r: Result<(), OxaError> = thread.run(|_slots| panic!("injected job panic"), &mut |_, _| Value::Null);
    let e = r.expect_err("the job fails");
    assert_eq!(e.code, codes::SCRIPT);
    assert!(e.message.contains("injected job panic"), "{e}");
    let ok = thread
        .run(
            |slots| {
                let id = slots.create(Limits::EXTENSION)?;
                let rt = slots.get(id).expect("runtime");
                rt.eval_script("t", "40 + 2")
            },
            &mut |_, _| Value::Null,
        )
        .expect("the thread is alive")
        .expect("the next job runs");
    assert_eq!(ok, json!(42));
}
