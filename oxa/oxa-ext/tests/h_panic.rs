//! Wave H lane `harden` (pre-flight A1/A2/B9): an extension handler whose host call panics (the
//! session side of `ctx.sessionManager.getEntries()` panics here) is reported as an extension
//! error for that handler; the extension host and the next fire go on. Passes in debug and
//! release (the release profile unwinds).
use oxa_ext::{ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{HeadlessUi, HookPoint, HookReply, Hooks, Host, HostAction, OxaError, Value};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Panics on the first `GetEntries`, answers an empty list afterwards.
struct PanicOnce(AtomicBool);

impl Host for PanicOnce {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        if matches!(action, HostAction::GetEntries) && self.0.swap(false, Ordering::SeqCst) {
            panic!("injected session-side panic");
        }
        Ok(json!([]))
    }
}

#[test]
fn a_panicking_host_call_is_an_extension_error_and_the_host_goes_on() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let path = env.write(
        "work/e.js",
        "export default function (oxa) {\n\
           oxa.on('turn_start', (e, ctx) => { globalThis.n = ctx.sessionManager.getEntries().length; });\n\
         }",
    );
    let host = ExtensionHost::new(env.work()).expect("host");
    let loaded = host.load(&[path], &LoadOpts::default());
    assert!(loaded.iter().all(|e| e.error.is_none()));
    host.attach(Arc::new(PanicOnce(AtomicBool::new(true))));
    let mut ui = HeadlessUi::default();
    let payload = json!({"turnIndex": 0, "timestamp": 0});
    assert_eq!(
        host.fire(HookPoint::TurnStart, &payload, &mut ui),
        HookReply::Continue
    );
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].event, "turn_start");
    assert!(errors[0].extension_path.ends_with("e.js"), "{errors:?}");
    assert!(
        errors[0].error.contains("injected session-side panic"),
        "the error names the panic: {errors:?}"
    );
    // The same handler runs again, now without the panic.
    assert_eq!(
        host.fire(HookPoint::TurnStart, &payload, &mut ui),
        HookReply::Continue
    );
    assert!(host.state().take_errors().is_empty());
}
