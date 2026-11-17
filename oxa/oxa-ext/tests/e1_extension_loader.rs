//! E1 acceptance tests: Extension loader.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_ext::{
    load_extensions, load_source, merge_exported_manifest, read_manifest, unload_all, ExtManifest, ExtState,
    LoadOpts, OXA_D_TS,
};
use oxa_fake::TestEnv;
use oxa_proto::{codes, HookPoint, OxaError, Value};
use oxa_script::{CallbackPort, FnRef, Limits, RuntimeThread, SlotId, Slots};
use std::path::PathBuf;
use std::sync::Arc;

fn new_state(env: &TestEnv) -> Arc<ExtState> {
    Arc::new(ExtState::new(env.work()))
}

fn thread(name: &str) -> RuntimeThread {
    RuntimeThread::spawn(name).expect("start the script thread")
}

/// Run `src` on the runtime of `slot` and return the JSON result.
fn eval_in(th: &RuntimeThread, slot: SlotId, src: &str) -> Result<Value, OxaError> {
    let src = src.to_string();
    let run = th.run(
        move |slots: &mut Slots| match slots.get(slot) {
            Some(rt) => rt.eval_script("test", &src),
            None => Err(OxaError::new(codes::NOT_FOUND, "no such extension runtime")),
        },
        &mut |_: Value, _: &CallbackPort| Value::Null,
    );
    match run {
        Ok(inner) => inner,
        Err(e) => Err(e),
    }
}

/// Names of the extensions in `exts`, in order.
fn names(exts: &[oxa_ext::LoadedExtension]) -> Vec<String> {
    exts.iter().map(|e| e.name.clone()).collect()
}

/// user, project (trusted), -e paths, packages
#[test]
fn discovery_dirs_and_order() {
    let env = TestEnv::new();
    let user = env.write(
        "home/.oxa/agent/extensions/user.js",
        "export default function (oxa) {}",
    );
    let project = env.write(
        "work/.oxa/extensions/project.js",
        "export default function (oxa) {}",
    );
    let cli = env.write("work/e/cli.js", "export default function (oxa) {}");
    env.write(
        "work/pkg/package.json",
        r#"{"name":"pkg","oxa":{"extensions":["b.js","a.js"]}}"#,
    );
    env.write("work/pkg/a.js", "export default function (oxa) {}");
    env.write("work/pkg/b.js", "export default function (oxa) {}");
    let pkg = env.work().join("pkg");

    // The order a caller passes is the order of `Resources::extension_paths`: user, project,
    // `-e` paths, then packages (a package's entries in the order its manifest lists them).
    let paths: Vec<PathBuf> = vec![user, project, cli, pkg];
    let st = new_state(&env);
    let th = thread("discovery");
    let exts = load_extensions(&st, &th, &paths, &LoadOpts::default());

    assert_eq!(names(&exts), vec!["user", "project", "cli", "pkg", "pkg"]);
    assert!(
        exts[3].path.ends_with("b.js"),
        "package entries keep the manifest order: {}",
        exts[3].path
    );
    assert!(
        exts[4].path.ends_with("a.js"),
        "package entries keep the manifest order: {}",
        exts[4].path
    );
    for (i, e) in exts.iter().enumerate() {
        assert_eq!(e.id.0 as usize, i, "ids are indices in load order");
        assert!(e.error.is_none(), "{e:?}");
        assert!(e.slot.is_some(), "{e:?}");
    }
    assert_eq!(
        st.registry.lock().unwrap().extensions,
        exts,
        "the registry holds the same list"
    );
    th.shutdown();
}

/// default export forms
#[test]
fn sync_and_async_factory() {
    let env = TestEnv::new();
    let sync = env.write(
        "work/sync.js",
        "export default function (oxa) { globalThis.marker = 'sync'; }",
    );
    let asynchronous = env.write(
        "work/async.js",
        "export default async function (oxa) { await Promise.resolve(); globalThis.marker = 'async'; }",
    );
    let st = new_state(&env);
    let th = thread("factories");
    let exts = load_extensions(&st, &th, &[sync, asynchronous], &LoadOpts::default());
    assert_eq!(exts.len(), 2);
    for e in &exts {
        assert!(e.error.is_none(), "{e:?}");
    }
    // The async factory was awaited: its code after the `await` ran before the load returned.
    let slot = exts[1].slot.expect("async extension has a runtime");
    assert_eq!(
        eval_in(&th, slot, "globalThis.marker").unwrap(),
        Value::from("async")
    );
    th.shutdown();
}

/// A factory that never settles fails the load (there are no timers to wait for it).
#[test]
fn unsettled_factory_fails_the_load() {
    let env = TestEnv::new();
    let p = env.write(
        "work/stuck.js",
        "export default function (oxa) { return new Promise(() => {}); }",
    );
    let st = new_state(&env);
    let th = thread("stuck");
    let exts = load_extensions(&st, &th, &[p], &LoadOpts::default());
    assert_eq!(exts.len(), 1);
    assert!(exts[0].slot.is_none());
    assert!(exts[0].error.is_some());
    th.shutdown();
}

/// others still load, ExtensionError reported
#[test]
fn load_error_isolates_one_extension() {
    let env = TestEnv::new();
    let ok1 = env.write("work/ok1.js", "export default function (oxa) {}");
    let broken = env.write("work/broken.js", "export default function (oxa) {");
    let throws = env.write(
        "work/throws.js",
        "export default function (oxa) { throw new Error('boom from the factory'); }",
    );
    let ok2 = env.write(
        "work/ok2.js",
        "export default function (oxa) { globalThis.two = 2; }",
    );
    let st = new_state(&env);
    let th = thread("isolation");
    let exts = load_extensions(
        &st,
        &th,
        &[ok1, broken.clone(), throws.clone(), ok2],
        &LoadOpts::default(),
    );

    assert_eq!(exts.len(), 4);
    assert!(exts[0].slot.is_some() && exts[0].error.is_none());
    assert!(exts[1].slot.is_none() && exts[1].error.is_some(), "{:?}", exts[1]);
    assert!(exts[2].slot.is_none());
    assert!(
        exts[2]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("boom from the factory"),
        "{:?}",
        exts[2].error
    );
    assert!(exts[3].slot.is_some() && exts[3].error.is_none());
    // The surviving extension still runs.
    let slot = exts[3].slot.unwrap();
    assert_eq!(eval_in(&th, slot, "globalThis.two").unwrap(), Value::from(2));

    // Each failure is reported once as an extension error, with the failing path.
    let errors = st.take_errors();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors.iter().all(|e| e.event == "load"));
    assert!(errors
        .iter()
        .any(|e| e.extension_path == broken.display().to_string()));
    assert!(errors
        .iter()
        .any(|e| e.extension_path == throws.display().to_string()));
    // Nothing from the failed loads stays registered.
    assert!(st
        .registry
        .lock()
        .unwrap()
        .handlers
        .values()
        .all(|hs| hs.iter().all(|h| h.ext.0 != 1 && h.ext.0 != 2)));
    th.shutdown();
}

/// old closures unreachable
#[test]
fn reload_drops_contexts() {
    let env = TestEnv::new();
    let st = new_state(&env);
    let th = thread("reload");
    let opts = LoadOpts::default();
    let first = load_source(
        &st,
        &th,
        "ctx",
        "export default function (oxa) { globalThis.keep = () => 42; }",
        &opts,
    );
    let slot = first.slot.expect("loaded");
    // A script function, as a handle the host could hold.
    let handle = eval_in(&th, slot, "globalThis.keep").unwrap();
    let f = FnRef::from_value(&handle).expect("a function is bridged as a handle");
    let still_callable = th.run(
        move |slots: &mut Slots| slots.get(slot).expect("live slot").call(&f, &[]),
        &mut |_: Value, _: &CallbackPort| Value::Null,
    );
    assert_eq!(still_callable.unwrap().unwrap(), Value::from(42));

    unload_all(&st, &th);
    unload_all(&st, &th); // idempotent

    assert!(st.registry.lock().unwrap().extensions.is_empty());
    let gone = th
        .run(
            move |slots: &mut Slots| slots.get(slot).is_none(),
            &mut |_: Value, _: &CallbackPort| Value::Null,
        )
        .unwrap();
    assert!(gone, "the old runtime is dropped");
    // The old handle is stale: calling it on a fresh runtime is an error, never another context.
    let stale = th
        .run(
            move |slots: &mut Slots| {
                let id = slots.create(Limits::EXTENSION)?;
                slots.get(id).expect("fresh slot").call(&f, &[])
            },
            &mut |_: Value, _: &CallbackPort| Value::Null,
        )
        .unwrap();
    assert!(stale.is_err(), "a stale handle must not resolve: {stale:?}");

    // Reloading brings the extension back with a fresh context.
    let again = load_source(
        &st,
        &th,
        "ctx",
        "export default function (oxa) { globalThis.keep = () => 7; }",
        &opts,
    );
    assert!(again.error.is_none());
    assert_eq!(
        eval_in(&th, again.slot.unwrap(), "globalThis.keep()").unwrap(),
        Value::from(7)
    );
    th.shutdown();
}

/// oxa.d.ts covers every registered API name
#[test]
fn d_ts_lists_api() {
    // The names the registration tasks provide (api_register, api_ctx, api_ui, api_render).
    let oxa_api = [
        "on",
        "registerTool",
        "registerCommand",
        "registerShortcut",
        "registerFlag",
        "getFlag",
        "registerMessageRenderer",
        "registerEntryRenderer",
        "registerToolRenderer",
        "registerMarkdownTransformer",
        "sendMessage",
        "sendUserMessage",
        "appendEntry",
        "setSessionName",
        "setLabel",
        "setModel",
        "setThinkingLevel",
        "getActiveTools",
        "getAllTools",
        "setActiveTools",
        "addPromptSection",
        "exec",
        "events",
    ];
    let ctx_api = [
        "select",
        "confirm",
        "input",
        "editor",
        "notify",
        "setStatus",
        "setWidget",
        "setFooter",
        "setHeader",
        "setTitle",
        "setWorkingMessage",
        "setEditorText",
        "getEditorText",
        "addAutocompleteProvider",
        "custom",
        "setEditorComponent",
        "hasUI",
        "cwd",
        "sessionManager",
        "getEntries",
        "getBranch",
        "getLeafId",
        "getContextUsage",
        "isIdle",
        "abort",
        "compact",
        "shutdown",
        "waitForIdle",
        "newSession",
        "fork",
        "navigateTree",
        "switchSession",
        "reload",
        "fs",
        "readFile",
        "writeFile",
        "fetch",
    ];
    for name in oxa_api.iter().chain(ctx_api.iter()) {
        assert!(OXA_D_TS.contains(name), "oxa.d.ts does not declare `{name}`");
    }
    // Every event the core fires has a typed (or at least named) entry in `OxaEvents`.
    for p in HookPoint::ALL {
        let key = format!("  {}: ", p.as_str());
        assert!(
            OXA_D_TS.contains(&key),
            "oxa.d.ts does not declare the event `{}`",
            p.as_str()
        );
    }
    // Omitted calls are not declared as members.
    for omitted in [
        "registerProvider(",
        "unregisterProvider(",
        "registerVirtualModel(",
        "unregisterVirtualModel(",
    ] {
        assert!(!OXA_D_TS.contains(omitted), "omitted API declared: {omitted}");
    }
}

/// The names a runtime really exposes on `oxa` are the ones the declarations list.
#[test]
fn d_ts_matches_registered_names() {
    let env = TestEnv::new();
    let st = new_state(&env);
    let th = thread("names");
    let loaded = load_source(
        &st,
        &th,
        "names",
        "export default function (oxa) { globalThis.names = Object.keys(oxa).concat(Object.keys(oxa.events || {})); }",
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{loaded:?}");
    let value = eval_in(&th, loaded.slot.unwrap(), "globalThis.names").unwrap();
    let registered: Vec<String> = value
        .as_array()
        .expect("names are an array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(!registered.is_empty(), "the API registers names");
    for name in &registered {
        assert!(
            OXA_D_TS.contains(name.as_str()),
            "registered `{name}` is not in oxa.d.ts"
        );
    }
    th.shutdown();
}

/// -ne
#[test]
fn no_extensions_flag() {
    // `-ne` leaves discovery with no paths; the loader then loads nothing and the registry stays
    // empty. (The flag itself is read by `oxa_resources::discover` (`DiscoverOpts::no_extensions`).)
    let env = TestEnv::new();
    let st = new_state(&env);
    let th = thread("none");
    let exts = load_extensions(&st, &th, &[], &LoadOpts::default());
    assert!(exts.is_empty());
    assert!(st.registry.lock().unwrap().extensions.is_empty());
    th.shutdown();
}

/// unsupported
#[test]
fn ts_file_rejected_with_hint() {
    let env = TestEnv::new();
    let p = env.write("work/typed.ts", "export default function (oxa: unknown) {}");
    let err = read_manifest(&p).expect_err("a .ts extension is refused");
    assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA, "{err:?}");
    assert!(err.hint.as_deref().unwrap_or("").contains("compile"), "{err:?}");

    // Through the loader the refusal is one extension error, not a crash.
    let st = new_state(&env);
    let th = thread("ts");
    let exts = load_extensions(&st, &th, std::slice::from_ref(&p), &LoadOpts::default());
    assert_eq!(exts.len(), 1);
    assert!(
        exts[0].error.as_deref().unwrap_or("").contains("not supported"),
        "{:?}",
        exts[0].error
    );
    th.shutdown();
}

/// `import fs from 'node:fs'`
#[test]
fn node_import_fails_loudly() {
    let env = TestEnv::new();
    let st = new_state(&env);
    let th = thread("node");
    let e = load_source(
        &st,
        &th,
        "uses-node",
        "import fs from 'node:fs'; export default function (oxa) { globalThis.fs = fs; }",
        &LoadOpts::default(),
    );
    assert!(e.slot.is_none());
    let text = e.error.expect("the import fails");
    assert!(text.contains("node:fs"), "{text}");
    assert!(
        text.contains("Node and npm imports are not available in oxa"),
        "{text}"
    );
    th.shutdown();
}

// ---------------------------------------------------------------------------------------------
// More tests (manifest, module loader, flags, rebase)
// ---------------------------------------------------------------------------------------------

#[test]
fn package_manifest_reads_oxa_key_and_rebase() {
    let env = TestEnv::new();
    env.write(
        "work/pk/package.json",
        r#"{"oxa":{"extensions":["main.js"],"rebase":true,"name":"pretty"}}"#,
    );
    env.write("work/pk/main.js", "export default function (oxa) {}");
    let m = read_manifest(&env.work().join("pk")).expect("manifest");
    assert_eq!(m.name, "pretty");
    assert!(m.rebase);
    assert_eq!(m.entries.len(), 1);
    assert!(m.entries[0].ends_with("main.js"));
}

#[test]
fn package_without_entries_is_not_found() {
    let env = TestEnv::new();
    env.write("work/empty/readme.txt", "nothing here");
    let err = read_manifest(&env.work().join("empty")).expect_err("nothing loadable");
    assert_eq!(err.code, codes::NOT_FOUND);
}

#[test]
fn bad_package_json_is_invalid() {
    let env = TestEnv::new();
    env.write("work/bad/package.json", "{ not json");
    env.write("work/bad/index.js", "export default function (oxa) {}");
    let err = read_manifest(&env.work().join("bad")).expect_err("bad JSON");
    assert_eq!(err.code, codes::INVALID);
}

#[test]
fn index_js_is_the_default_entry() {
    let env = TestEnv::new();
    env.write("work/dir/index.mjs", "export default function (oxa) {}");
    let m = read_manifest(&env.work().join("dir")).expect("manifest");
    assert_eq!(m.entries.len(), 1);
    assert!(m.entries[0].ends_with("index.mjs"));
}

#[test]
fn exported_manifest_sets_name_and_rebase() {
    let env = TestEnv::new();
    let p = env.write(
        "work/named.js",
        "export const manifest = { name: 'fancy', rebase: true }; export default function (oxa) {}",
    );
    let st = new_state(&env);
    let th = thread("manifest");
    let exts = load_extensions(&st, &th, &[p], &LoadOpts::default());
    assert_eq!(exts[0].name, "fancy");
    assert!(exts[0].declared_rebase);
    assert!(exts[0].error.is_none(), "{:?}", exts[0].error);
    th.shutdown();
}

#[test]
fn merge_exported_manifest_only_takes_known_fields() {
    let base = ExtManifest {
        name: "stem".into(),
        description: None,
        entries: vec![],
        rebase: false,
    };
    let merged = merge_exported_manifest(
        base.clone(),
        &serde_json::json!({"name": "x", "rebase": true, "description": "d", "other": 1}),
    );
    assert_eq!(merged.name, "x");
    assert!(merged.rebase);
    assert_eq!(merged.description.as_deref(), Some("d"));
    assert_eq!(merge_exported_manifest(base.clone(), &Value::Null), base);
}

#[test]
fn relative_imports_resolve_inside_the_package() {
    let env = TestEnv::new();
    env.write("work/pkg2/package.json", r#"{"oxa":{"extensions":["main.js"]}}"#);
    env.write(
        "work/pkg2/main.js",
        "import { value } from './lib/util.js'; export default function (oxa) { globalThis.v = value; }",
    );
    env.write("work/pkg2/lib/util.js", "export const value = 41 + 1;");
    let st = new_state(&env);
    let th = thread("relative");
    let exts = load_extensions(&st, &th, &[env.work().join("pkg2")], &LoadOpts::default());
    assert!(exts[0].error.is_none(), "{:?}", exts[0].error);
    assert_eq!(
        eval_in(&th, exts[0].slot.unwrap(), "globalThis.v").unwrap(),
        Value::from(42)
    );
    th.shutdown();
}

#[test]
fn imports_that_leave_the_extension_directory_are_refused() {
    let env = TestEnv::new();
    env.write("work/secret.js", "export const s = 'nope';");
    let p = env.write(
        "work/ext/x.js",
        "import { s } from '../secret.js'; export default function (oxa) { globalThis.s = s; }",
    );
    let st = new_state(&env);
    let th = thread("escape");
    let exts = load_extensions(&st, &th, &[p], &LoadOpts::default());
    assert!(exts[0].slot.is_none());
    let text = exts[0].error.clone().unwrap_or_default();
    assert!(text.contains("secret.js"), "{text}");
    th.shutdown();
}

#[test]
fn oxa_tui_module_is_virtual() {
    let env = TestEnv::new();
    let p = env.write(
        "work/tui.js",
        "import * as tui from 'oxa:tui'; export default function (oxa) { globalThis.ok = typeof tui === 'object'; }",
    );
    let st = new_state(&env);
    let th = thread("tui");
    let exts = load_extensions(&st, &th, &[p], &LoadOpts::default());
    assert!(exts[0].error.is_none(), "{:?}", exts[0].error);
    assert_eq!(
        eval_in(&th, exts[0].slot.unwrap(), "globalThis.ok").unwrap(),
        Value::Bool(true)
    );
    th.shutdown();
}

#[test]
fn missing_extension_file_is_reported() {
    let env = TestEnv::new();
    let st = new_state(&env);
    let th = thread("missing");
    let exts = load_extensions(&st, &th, &[env.work().join("nope.js")], &LoadOpts::default());
    assert_eq!(exts.len(), 1);
    assert!(exts[0].slot.is_none());
    assert!(
        exts[0].error.as_deref().unwrap_or("").contains("not found"),
        "{:?}",
        exts[0].error
    );
    th.shutdown();
}
