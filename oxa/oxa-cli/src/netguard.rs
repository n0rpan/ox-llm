//! Wave H: the network decisions of the front end (O-167(b), P3, P4). Every
//! decision is here; `dispatch.rs` only calls in.
use oxa_config::{dirs_from_env, Env, Settings};
use oxa_proto::{Level, OxaError, Sandbox};
use oxa_sandbox::{sandbox_net_setting, NetSandbox, SETTING_SANDBOX_NET};
use std::path::Path;
use std::sync::Arc;

/// O-167(b): the process sandbox with the bash network decision (`oxa.sandbox.net`, default
/// on; always off while offline). Read once at startup.
pub(crate) fn with_net(sandbox: Arc<dyn Sandbox>, settings: &Settings) -> Arc<dyn Sandbox> {
    let on = sandbox_net_setting(settings.string(SETTING_SANDBOX_NET).as_deref());
    Arc::new(NetSandbox::new(sandbox, on, oxa_proto::offline()))
}

/// The setting that allows an engine host that is not loopback (P3). Read from the user's
/// settings only: a project file cannot send the prompts elsewhere.
pub(crate) const SETTING_ALLOW_REMOTE: &str = "oxa.engine.allowRemote";
/// The setting of the hard offline switch (P4).
pub(crate) const SETTING_OFFLINE: &str = "oxa.offline";
/// The environment variable of the hard offline switch (pi: `PI_OFFLINE`).
pub(crate) const ENV_OFFLINE: &str = "OXA_OFFLINE";

/// pi's `isTruthyEnvFlag`: `1`, `true`, `yes` (any case).
pub(crate) fn truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

/// `(offline, allow_remote_engine)` from the flag, the environment and the user's settings.
/// Offline is on when any of `--offline`, `OXA_OFFLINE`, `oxa.offline` says so.
pub(crate) fn policy_from(flag_offline: bool, env: &Env, user: &Settings) -> (bool, bool) {
    let offline = flag_offline || truthy(env.get(ENV_OFFLINE)) || user.bool(SETTING_OFFLINE, false);
    (offline, user.bool(SETTING_ALLOW_REMOTE, false))
}

/// Install the process network policy (`oxa_proto::set_net_policy`) before anything connects.
pub(crate) fn install(flag_offline: bool, env: &Env, session_dir: Option<&Path>, cwd: &Path) {
    let user = match dirs_from_env(env, session_dir) {
        Ok(dirs) => Settings::load(&dirs, cwd, false),
        Err(_) => Settings::default(),
    };
    let (offline, allow_remote) = policy_from(flag_offline, env, &user);
    oxa_proto::set_net_policy(offline, allow_remote);
}

/// P3 at startup: the engine endpoint must be loopback unless `oxa.engine.allowRemote` (and
/// never a remote one while offline). The same check runs at every engine connect.
pub(crate) fn check_endpoint(base_url: &str) -> Result<(), OxaError> {
    match oxa_proto::url_host(base_url) {
        Some(host) => oxa_proto::check_engine_host(&host),
        None => Ok(()), // not a URL: `HttpEngine::new` reports it
    }
}

/// P3: the one startup notice for an allowed remote engine.
pub(crate) fn endpoint_notice(base_url: &str) -> Option<(Level, String)> {
    let host = oxa_proto::url_host(base_url)?;
    (!oxa_proto::is_loopback_host(&host)).then(|| (Level::Warning, oxa_proto::remote_engine_notice(&host)))
}
