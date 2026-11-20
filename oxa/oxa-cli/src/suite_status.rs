//! `oxa status --json`: the one status object the ox AI panel reads (`oxa/docs/pages/panel.md`).
//!
//! Read-only: no model, no network request and no write. Key files are checked by `stat` only;
//! their contents and their paths never leave this module. The object carries the envelope and
//! the `local` and `cloud` sections, `sessions` and `workers` (see `sessions.rs`), and the `lead`
//! section (see `lead.rs`). `usage_state` arrives with its own slice, so a reader must treat an
//! absent section as "not reported yet".
mod lead;
mod sessions;

use crate::in_process;
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::OxaError;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Panel commands this binary has. Each slice adds its word when its command lands; a word is
/// never listed before its command exists.
const CAPABILITIES: &[&str] = &[];

/// The provider names `oxa.cloud.models` may use: the same set the in-process gateway accepts.
const PROVIDERS: [&str; 4] = ["anthropic", "openai", "openrouter", "chatgpt"];

pub(crate) fn run(dirs: &Dirs, cwd: &Path, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    // Project settings are not trusted here: cloud routes and local binaries are user-only.
    let user = Settings::load(dirs, cwd, false);
    let generated = now_ms();
    let mut value = suite_value(&user, dirs, engine_binary(&user).is_file(), generated);
    // The same session folder as `oxa status <session>`: the environment variable, then `sessionDir`.
    let sessions_dir = match user.session_dir(&dirs.home, cwd) {
        Some(configured) if env.get("OXA_SESSION_DIR").is_none() => configured,
        _ => dirs.sessions_dir.clone(),
    };
    sessions::attach(
        &mut value,
        sessions::scan(&sessions_dir, &dirs.agent_dir, generated),
    );
    value["lead"] = lead::section(&user, dirs, cwd, env);
    writeln!(out, "{value}").map_err(|e| OxaError::io("cannot write suite status", &e))?;
    Ok(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The local engine binary: `oxa.local.binary` when set, else the `oxe-server` beside this
/// executable (the same rule as `oxc_local::configured_engine`). Only its existence is reported.
fn engine_binary(user: &Settings) -> PathBuf {
    user.get("oxa.local.binary")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("oxe-server")))
        })
        .unwrap_or_default()
}

fn suite_value(user: &Settings, dirs: &Dirs, engine_found: bool, generated_ms: u64) -> Value {
    json!({
        "type": "oxa_suite_status",
        "version": 1,
        "generated_ms": generated_ms,
        "oxa": {"version": env!("CARGO_PKG_VERSION")},
        "capabilities": CAPABILITIES,
        "default_model": user.string("defaultModel"),
        "local": local_section(user, engine_found),
        "cloud": cloud_section(user, dirs),
    })
}

fn local_section(user: &Settings, engine_found: bool) -> Value {
    let enabled = user.bool(oxc_local::LOCAL_SETTING, false);
    let wired = user
        .get("oxa.local.models")
        .and_then(Value::as_object)
        .map_or(0, |models| models.len());
    // Files on disk that are not wired are not counted here: that scan belongs to the models
    // command. Without wired models there is nothing this status can call ready or failed.
    let (state, fix) = if wired == 0 {
        ("off", None)
    } else if !enabled {
        ("off", Some("enable_local"))
    } else if !engine_found {
        ("error", Some("install_engine"))
    } else {
        ("ready", None)
    };
    let engine = if engine_found { "found" } else { "missing" };
    json!({
        "state": state,
        "enabled": enabled,
        "engine_binary": engine,
        "wired": wired,
        "fix": fix,
    })
}

/// The cloud routes of `oxa.cloud` as the in-process gateway reads them. `None` when the
/// configuration would not parse there (a non-object, an unknown provider, a route without one).
#[derive(Default)]
struct Cloud {
    routes: usize,
    providers: BTreeSet<&'static str>,
    go: bool,
    discover: bool,
}

fn cloud_routes(user: &Settings) -> Option<Cloud> {
    let mut cloud = Cloud::default();
    match user.get("oxa.cloud") {
        None | Some(Value::Null) => return Some(cloud),
        Some(value) if !value.is_object() => return None,
        Some(_) => {}
    }
    cloud.go = user.get("oxa.cloud.opencode.enabled") == Some(&Value::Bool(true));
    cloud.discover = user.get("oxa.cloud.discover_models") == Some(&Value::Bool(true));
    if cloud.go {
        cloud.providers.insert("opencode");
    }
    if let Some(models) = user.get("oxa.cloud.models").filter(|v| !v.is_null()) {
        for route in models.as_object()?.values() {
            let provider = route.get("provider")?.as_str()?;
            // Keep readiness aligned with the fields `open_gateway` requires before it can
            // construct a route. Credentials remain stat-only below.
            route
                .get("upstream_model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())?;
            for field in ["context_window", "max_output"] {
                route
                    .get(field)
                    .and_then(Value::as_u64)
                    .and_then(|limit| u32::try_from(limit).ok())?;
            }
            cloud
                .providers
                .insert(PROVIDERS.into_iter().find(|p| *p == provider)?);
            cloud.routes += 1;
        }
    }
    Some(cloud)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Missing,
    UnsafeMode,
}

impl Presence {
    fn key_word(self) -> &'static str {
        match self {
            Presence::Present => "present",
            Presence::Missing => "missing",
            Presence::UnsafeMode => "unsafe_mode",
        }
    }
}

/// `stat` only: existence, file type and permission bits, the same bits the gateway refuses.
/// The contents are never read, so an unreadable key is reported by its bits, not by a failed open.
fn key_file(path: &Path) -> Presence {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 => Presence::UnsafeMode,
        Ok(_) => Presence::Present,
        Err(_) => Presence::Missing,
    }
}

fn cloud_section(user: &Settings, dirs: &Dirs) -> Value {
    let key_path = in_process::credential_path(dirs);
    let key = key_file(&key_path);
    let login = key_file(&key_path.with_file_name("chatgpt-auth.json"));
    let login_word = if login == Presence::Present {
        "present"
    } else {
        "missing"
    };
    let enabled = user.bool(in_process::SETTING, false);
    let (state, fix, routes, providers) = match cloud_routes(user) {
        None => ("error", Some("fix_config"), 0, Vec::new()),
        Some(cloud) => {
            let routes = cloud.routes;
            let providers: Vec<&str> = cloud.providers.iter().copied().collect();
            if !enabled {
                ("off", (routes > 0).then_some("enable_cloud"), routes, providers)
            } else if routes == 0 && !cloud.go && !cloud.discover {
                ("off", Some("fix_config"), routes, providers)
            } else {
                let needs_login = cloud.providers.contains("chatgpt");
                let needs_key = cloud.go || cloud.discover || cloud.providers.iter().any(|p| *p != "chatgpt");
                let missing_key = needs_key && key != Presence::Present;
                let missing_login = needs_login && login != Presence::Present;
                if missing_key || missing_login {
                    let fix = if missing_key && key == Presence::UnsafeMode {
                        "fix_key_mode"
                    } else {
                        "add_key"
                    };
                    ("no_credentials", Some(fix), routes, providers)
                } else {
                    ("ready", None, routes, providers)
                }
            }
        }
    };
    json!({
        "state": state,
        "enabled": enabled,
        "routes": routes,
        "providers": providers,
        "key_file": key.key_word(),
        "chatgpt_login": login_word,
        "fix": fix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use oxa_proto::codes;

    fn fixture(name: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("omarchy-panel")
            .join("tests")
            .join("fixtures")
            .join(name);
        serde_json::from_str(&fs::read_to_string(&path).expect("read fixture")).expect("fixture is JSON")
    }

    fn cloud_config() -> Value {
        json!({
            "defaultModel": "openai/gpt-6.2-sol",
            "oxa": {
                "engine": {"inProcessCloud": true},
                "cloud": {"models": {"gpt-sol": {
                    "provider": "openai",
                    "upstream_model": "gpt-6.2-sol",
                    "context_window": 128000,
                    "max_output": 16384
                }}}
            }
        })
    }

    /// Writes the key file of the test's own home with the given permission bits.
    fn write_key(dirs: &Dirs, mode: u32) {
        let path = in_process::credential_path(dirs);
        fs::create_dir_all(path.parent().expect("key folder")).expect("create key folder");
        // Removing needs only folder access, so a previous unreadable key can be replaced.
        let _ = fs::remove_file(&path);
        fs::write(&path, "{}").expect("write key");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set mode");
    }

    #[test]
    fn suite_status_matches_fixture_shape() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        write_key(&dirs, 0o600);
        let user = Settings::from_values(cloud_config(), json!({}));
        let value = suite_value(&user, &dirs, true, 1);
        let fixture = fixture("status.json");
        assert_eq!(value["type"], fixture["type"]);
        assert_eq!(value["version"], fixture["version"]);
        assert_eq!(value["default_model"], fixture["default_model"]);
        for key in value.as_object().expect("object").keys() {
            assert!(
                fixture.get(key.as_str()).is_some(),
                "status key {key} is in the fixture"
            );
        }
        for section in ["local", "cloud"] {
            for key in value[section].as_object().expect("object").keys() {
                assert!(
                    fixture[section].get(key.as_str()).is_some(),
                    "{section}.{key} is in the fixture"
                );
            }
        }
        assert_eq!(value["cloud"], fixture["cloud"]);
    }

    #[test]
    fn suite_status_never_opens_key_files() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        // Mode 0 cannot be opened by this process. Its bits are safe, so a stat reports it as
        // present and the state is ready: an open would have failed and reported it missing.
        write_key(&dirs, 0o000);
        let user = Settings::from_values(cloud_config(), json!({}));
        let value = suite_value(&user, &dirs, true, 1);
        assert_eq!(value["cloud"]["key_file"], "present");
        assert_eq!(value["cloud"]["state"], "ready");
        // Group or world bits are unsafe: still only a stat, and the fix is the mode.
        write_key(&dirs, 0o644);
        let value = suite_value(&user, &dirs, true, 1);
        assert_eq!(value["cloud"]["key_file"], "unsafe_mode");
        assert_eq!(value["cloud"]["state"], "no_credentials");
        assert_eq!(value["cloud"]["fix"], "fix_key_mode");
        // The command still answers with the object, whatever the key's mode.
        let mut out = Vec::new();
        let code = crate::status::run(
            &["--json".to_string()],
            &dirs,
            env.root(),
            &Env::from_pairs(&[]),
            &mut out,
        )
        .expect("status --json answers with an unreadable key");
        assert_eq!(code, 0);
        let printed: Value = serde_json::from_slice(&out).expect("one JSON object");
        assert_eq!(printed["type"], "oxa_suite_status");
    }

    #[test]
    fn incomplete_cloud_route_reports_config_error() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        write_key(&dirs, 0o600);
        let user = Settings::from_values(
            json!({
                "oxa": {
                    "engine": {"inProcessCloud": true},
                    "cloud": {"models": {"gpt-sol": {"provider": "openai"}}}
                }
            }),
            json!({}),
        );

        let value = suite_value(&user, &dirs, true, 1);
        assert_eq!(value["cloud"]["state"], "error");
        assert_eq!(value["cloud"]["fix"], "fix_config");
        assert_eq!(value["cloud"]["routes"], 0);
    }

    #[test]
    fn session_status_form_is_unchanged() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let no_env = Env::from_pairs(&[]);
        let mut out = Vec::new();
        // No argument, an unknown flag, and `--json` with more words are still usage errors,
        // exactly as before: none of them is a session id and none prints the suite object.
        for words in [
            vec![],
            vec!["-x".to_string()],
            vec!["--json".to_string(), "extra".to_string()],
        ] {
            let error = crate::status::run(&words, &dirs, env.root(), &no_env, &mut out).unwrap_err();
            assert_eq!(error.code, codes::USAGE, "{words:?}");
        }
        assert!(out.is_empty(), "no suite object for the other forms");
    }
}
