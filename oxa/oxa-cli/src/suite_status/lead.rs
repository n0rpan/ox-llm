//! The `lead` section of `oxa status --json`.
//!
//! Read-only. The selected Lead Session is read through its file store without taking the writer
//! lock, so nothing is appended, synced or checkpointed and the session file keeps its bytes. What
//! leaves this module is a state word, counts, the goal's own status word, the model id, times and
//! the loopback address of `oxa.lead.serve`. Ask text, ask ids, the objective and entry bodies
//! never leave it.
use crate::dispatch::{is_agent, lead_model};
use crate::lead_access::Config;
use crate::LeadOwnerBinding;
use oxa_config::{Dirs, Env, Settings};
use oxa_session::{Goal, GoalStatus};
use serde_json::{json, Value};
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

/// A loopback connect is local, so a short timeout is enough and keeps the status fast.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// The facts a read of the selected Lead Session gives. Counts and words only.
#[derive(Debug, PartialEq)]
struct Facts {
    state: &'static str,
    configured: bool,
    goal_status: Option<&'static str>,
    open_asks: usize,
    last_activity_ms: Option<u64>,
}

impl Facts {
    fn unavailable(state: &'static str, configured: bool) -> Self {
        Self {
            state,
            configured,
            goal_status: None,
            open_asks: 0,
            last_activity_ms: None,
        }
    }
}

/// The `lead` object. `state` is `off` when no Lead Session is selected, `unknown` when the
/// selected one cannot be read, and otherwise the state of its saved goal.
pub(super) fn section(user: &Settings, dirs: &Dirs, cwd: &Path, env: &Env) -> Value {
    // Use the Lead command's owner-private settings reader and validation rather than the
    // merged Settings value, which may include project settings.
    let model = fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .ok()
        .and_then(|uid| lead_model(dirs, uid).ok())
        .flatten();
    let facts = facts(user, dirs, cwd, env);
    // The owner's configured per-host capacity (#2369), user layer only. Display
    // truth for the panel; observed use/Ready/stuck stay on the lead board, which
    // alone reads the situation. Unset or unreadable is null, never a fabricated map.
    let capacity = crate::lead::capacity::Capacity::parse(user.user_value("oxa.lead.capacity"));
    json!({
        "state": facts.state,
        "configured": facts.configured,
        "goal_status": facts.goal_status,
        "open_asks": facts.open_asks,
        "model": model,
        "last_activity_ms": facts.last_activity_ms,
        "serve": serve(dirs, cwd),
        "capacity": if capacity.is_empty() { Value::Null } else { capacity.to_json() },
    })
}

fn facts(user: &Settings, dirs: &Dirs, cwd: &Path, env: &Env) -> Facts {
    let selected = user.get("oxa.lead.sessionId").is_some_and(|id| !id.is_null());
    if !selected {
        return Facts::unavailable("off", false);
    }
    // The Lead commands refuse agent callers; the status reports the lead as unknown to them.
    if is_agent(env) {
        return Facts::unavailable("unknown", false);
    }
    let Ok(project) = cwd.canonicalize() else {
        return Facts::unavailable("unknown", false);
    };
    let Ok(uid) = fs::metadata("/proc/self").map(|meta| meta.uid()) else {
        return Facts::unavailable("unknown", false);
    };
    let binding = match LeadOwnerBinding::load(dirs, &project, uid) {
        Ok(Some(binding)) => binding,
        _ => return Facts::unavailable("unknown", false),
    };
    let Ok(entries) = binding.read_entries() else {
        return Facts::unavailable("unknown", true);
    };
    let last_activity_ms = entries.last().map(|entry| entry.ts_ms);
    let goal = match Goal::latest(&entries) {
        Ok(goal) => goal,
        Err(_) => {
            return Facts {
                last_activity_ms,
                ..Facts::unavailable("unknown", true)
            }
        }
    };
    let (state, goal_status, open_asks) = match goal {
        None => ("idle", None, 0),
        Some(goal) => {
            let asks = goal.lead.as_ref().map_or(0, |lead| lead.open_asks.len());
            let state = match goal.status {
                // Cancellation keeps Lead's open asks, but a terminal goal cannot await its owner.
                GoalStatus::Done | GoalStatus::Canceled => "idle",
                _ if asks > 0 => "awaiting_owner",
                GoalStatus::Pursuing => "pursuing",
                GoalStatus::Paused => "paused",
                GoalStatus::Blocked => "stalled",
            };
            (state, Some(goal.status.as_str()), asks)
        }
    };
    Facts {
        state,
        configured: true,
        goal_status,
        open_asks,
        last_activity_ms,
    }
}

/// `oxa.lead.serve` as the listener would bind it. Only a validated loopback endpoint gets a URL;
/// `Config::load` refuses any other bind, and the guard here keeps that rule in this module too.
/// `settings` is false: the lead page has no settings route in this binary.
fn serve(dirs: &Dirs, cwd: &Path) -> Value {
    serve_with_policy(dirs, cwd, oxa_proto::offline())
}

fn serve_with_policy(dirs: &Dirs, cwd: &Path, offline: bool) -> Value {
    let off = json!({"enabled": false, "listening": false, "url": null, "settings": false});
    let Ok(config) = Config::load(dirs, cwd) else {
        return off;
    };
    if !config.enabled() {
        return off;
    }
    let (bind, port) = config.endpoint();
    if !bind.is_loopback() {
        return off;
    }
    let address = SocketAddr::new(bind, port);
    // Offline is a process-wide no-network invariant, including loopback probes.
    let listening = !offline && TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok();
    json!({
        "enabled": true,
        "listening": listening,
        "url": format!("http://{address}/"),
        "settings": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use oxa_session::{FsStore, OpenGoalAsk, SessionStore};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;

    /// Selects a Lead Session of the test project through the owner's settings. With `ask`, the
    /// saved goal carries one open ask whose text must never reach the status.
    fn owner_lead(test: &TestEnv, dirs: &Dirs, ask: bool) -> String {
        fs::create_dir_all(&dirs.agent_dir).unwrap();
        fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let uid = fs::metadata(&dirs.agent_dir).unwrap().uid();
        fs::create_dir_all(test.work()).unwrap();
        let project = test.work().canonicalize().unwrap();
        let id = LeadOwnerBinding::create(dirs, &project, uid).unwrap();
        write_settings(dirs, &json!({"oxa": {"lead": {"sessionId": id}}}));
        if ask {
            let store = FsStore::new(dirs.sessions_dir.clone());
            let session = store.resolve(&id).unwrap();
            store.acquire(&session).unwrap();
            store.commit_lock();
            let entries = store.load(&session).unwrap();
            let mut goal = Goal::new("private objective text".into(), Default::default(), 1).unwrap();
            goal.lead
                .get_or_insert_with(Default::default)
                .open_asks
                .push(OpenGoalAsk {
                    id: "lead:board:66".into(),
                    revision: entries.len() as u64 + 1,
                    question: "Board item #66 private question".into(),
                });
            oxa_core::goal::checkpoint_idle_lead_goal(&store, &session, &entries, &goal).unwrap();
        }
        id
    }

    /// Writes the owner's user settings with owner-only bits, as the Lead commands require.
    fn write_settings(dirs: &Dirs, value: &Value) {
        let path = dirs.settings_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, value.to_string()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn section_for(test: &TestEnv, dirs: &Dirs) -> Value {
        let user = Settings::load(dirs, &test.work(), false);
        section(&user, dirs, &test.work(), &Env::from_pairs(&[]))
    }

    #[test]
    fn lead_section_does_not_write_goal_store() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let id = owner_lead(&test, &dirs, true);
        let path = FsStore::new(dirs.sessions_dir.clone()).resolve(&id).unwrap().path;
        let before = fs::read(&path).unwrap();
        let value = section_for(&test, &dirs);
        assert_eq!(value["state"], "awaiting_owner");
        assert_eq!(value["configured"], true);
        assert_eq!(value["open_asks"], 1);
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "the Lead Session file keeps its bytes"
        );
    }

    #[test]
    fn lead_section_has_no_ask_text() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        owner_lead(&test, &dirs, true);
        let value = section_for(&test, &dirs);
        assert_eq!(value["open_asks"], 1);
        let text = value.to_string();
        for private in [
            "Board item",
            "private question",
            "lead:board:66",
            "private objective",
        ] {
            assert!(!text.contains(private), "{private} stays out of the status");
        }
    }

    #[test]
    fn lead_section_uses_the_protected_model_reader() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        owner_lead(&test, &dirs, false);
        write_settings(&dirs, &json!({"oxa": {"lead": {"model": "bad\u{0007}model"}}}));
        assert!(section_for(&test, &dirs)["model"].is_null());

        write_settings(&dirs, &json!({"oxa": {"lead": {"model": "owner-model"}}}));
        let project_settings = dirs.project_settings_file(&test.work());
        fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
        fs::write(
            project_settings,
            json!({"oxa": {"lead": {"model": "project-model"}}}).to_string(),
        )
        .unwrap();
        // `oxa.lead.model` is user-only: the project file cannot name the Lead's model, so the
        // merged settings keep the owner's value and the section reports that.
        let merged = Settings::load(&dirs, &test.work(), true);
        assert_eq!(merged.get("oxa.lead.model"), Some(&json!("owner-model")));
        assert_eq!(
            section(&merged, &dirs, &test.work(), &Env::from_pairs(&[]))["model"],
            "owner-model"
        );
    }

    #[test]
    fn lead_section_capacity_is_user_only_and_drops_invalid_entries() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        // Unset capacity is null, never a fabricated map.
        assert!(section_for(&test, &dirs)["capacity"].is_null());
        write_settings(
            &dirs,
            &json!({"oxa": {"lead": {"capacity": {"air": 6, "n0rpan": 8, "bad name": 3}}}}),
        );
        // The invalid host entry is ignored; the valid capacities are reported.
        assert_eq!(
            section_for(&test, &dirs)["capacity"],
            json!({"air": 6, "n0rpan": 8})
        );
        // A project file cannot change the owner's capacity.
        let project_settings = dirs.project_settings_file(&test.work());
        fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
        fs::write(
            &project_settings,
            json!({"oxa": {"lead": {"capacity": {"air": 99}}}}).to_string(),
        )
        .unwrap();
        let merged = Settings::load(&dirs, &test.work(), true);
        assert_eq!(
            section(&merged, &dirs, &test.work(), &Env::from_pairs(&[]))["capacity"],
            json!({"air": 6, "n0rpan": 8})
        );
    }

    #[test]
    fn terminal_goal_with_retained_ask_is_idle() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let id = owner_lead(&test, &dirs, true);
        let awaiting = section_for(&test, &dirs);
        assert_eq!(awaiting["state"], "awaiting_owner");
        assert_eq!(awaiting["open_asks"], 1);

        let store = FsStore::new(dirs.sessions_dir.clone());
        let session = store.resolve(&id).unwrap();
        store.acquire(&session).unwrap();
        store.commit_lock();
        let entries = store.load(&session).unwrap();
        let mut goal = Goal::latest(&entries).unwrap().unwrap();
        goal.cancel().unwrap();
        oxa_core::goal::checkpoint_idle_lead_goal(&store, &session, &entries, &goal).unwrap();

        let value = section_for(&test, &dirs);
        assert_eq!(value["state"], "idle");
        assert_eq!(value["goal_status"], "canceled");
        assert_eq!(value["open_asks"], 1);
    }

    #[test]
    fn serve_url_is_loopback_only() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve_with = |bind: &str| {
            write_settings(
                &dirs,
                &json!({"oxa": {"lead": {"serve": {"enabled": true, "bind": bind, "port": port}}}}),
            );
            serve(&dirs, &test.work())
        };
        let open = serve_with("127.0.0.1");
        assert_eq!(open["enabled"], true);
        assert_eq!(open["listening"], true);
        assert_eq!(open["url"], format!("http://127.0.0.1:{port}/"));
        // A wider bind is refused by the listener's configuration, so no URL is reported.
        let wide = serve_with("0.0.0.0");
        assert_eq!(wide["enabled"], false);
        assert_eq!(wide["listening"], false);
        assert!(wide["url"].is_null());
        drop(listener);
    }

    #[test]
    fn offline_serve_status_does_not_probe_listener() {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        write_settings(
            &dirs,
            &json!({"oxa": {"lead": {"serve": {
                "enabled": true, "bind": "127.0.0.1", "port": port
            }}}}),
        );

        let value = serve_with_policy(&dirs, &test.work(), true);
        assert_eq!(value["enabled"], true);
        assert_eq!(value["listening"], false);
        assert_eq!(value["url"], format!("http://127.0.0.1:{port}/"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
