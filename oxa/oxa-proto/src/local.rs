//! Local-first additions (wave H, additive): the one user-facing problem shape
//! (R6) and the process network policy (P3 / P4).
//!
//! **The problem shape (R6).** Every refusal or stop added for local-first use reads the same:
//! line 1 `<what happened>: <reason>`, line 2 `Fix: <command or action>`. As an [`OxaError`]
//! the first line is `message` and the second is `hint`, so `Display` prints exactly the two
//! lines; as a notice or a shown entry it is [`problem_text`].
use crate::{codes, OxaError};
use std::sync::atomic::{AtomicBool, Ordering};

/// The two lines of the problem shape.
pub fn problem_text(what: &str, why: &str, fix: &str) -> String {
    format!("{what}: {why}\nFix: {fix}")
}

/// The problem shape as an error with `code`.
pub fn problem(code: &'static str, what: &str, why: &str, fix: &str) -> OxaError {
    OxaError::new(code, format!("{what}: {why}")).with_hint(format!("Fix: {fix}"))
}

// ---- P3 / P4: the process network policy ----
//
// Set once at startup by the front end (`oxa-cli`) from `OXA_OFFLINE` / `--offline` /
// `oxa.offline` and `oxa.engine.allowRemote`; read at every connect site of the harness (the
// engine client, MCP http/oauth, extension and codemode fetch) and before every command that
// reaches the network for the user (`git` for packages, `gh` for report-issue) or a
// network grant. Defaults (nothing set): online, remote engine refused.

static OFFLINE: AtomicBool = AtomicBool::new(false);
static ALLOW_REMOTE_ENGINE: AtomicBool = AtomicBool::new(false);

/// Install the process network policy (startup, once; tests of a single process may call it).
pub fn set_net_policy(offline: bool, allow_remote_engine: bool) {
    OFFLINE.store(offline, Ordering::SeqCst);
    ALLOW_REMOTE_ENGINE.store(allow_remote_engine, Ordering::SeqCst);
}

/// `true` when the hard offline switch is on.
pub fn offline() -> bool {
    OFFLINE.load(Ordering::SeqCst)
}

/// `true` when `oxa.engine.allowRemote` is on.
pub fn remote_engine_allowed() -> bool {
    ALLOW_REMOTE_ENGINE.load(Ordering::SeqCst)
}

/// Loopback = `localhost` (any case, with or without a trailing dot), 127.0.0.0/8, `::1`
/// (also bracketed, and the IPv4-mapped `::ffff:127.x.y.z`). Names are never resolved.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    let h = h.strip_suffix('.').unwrap_or(h);
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback();
    }
    if let Ok(ip) = h.parse::<std::net::Ipv6Addr>() {
        return ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback());
    }
    false
}

/// The host of `http://` / `https://` URL text (`None` when there is no authority).
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|(_, a)| a).unwrap_or(authority);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("").to_string()
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    (!host.is_empty()).then_some(host)
}

/// The one offline refusal (R6 shape). `what` names the site and target, e.g.
/// `MCP server "docs" (example.com)`, `git clone`.
pub fn offline_error(what: &str) -> OxaError {
    problem(
        codes::OFFLINE,
        &format!("Refused: {what} needs the network"),
        "offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline)",
        "turn offline mode off: unset OXA_OFFLINE, drop --offline, set \"oxa.offline\": false",
    )
}

/// A connect site: offline refuses every host that is not loopback.
pub fn check_connect(what: &str, host: &str) -> Result<(), OxaError> {
    if offline() && !is_loopback_host(host) {
        return Err(offline_error(&format!("{what} ({host})")));
    }
    Ok(())
}

/// A command or grant that needs the network (packages, report-issue, a network grant).
pub fn check_online(what: &str) -> Result<(), OxaError> {
    if offline() {
        return Err(offline_error(what));
    }
    Ok(())
}

/// The engine host: offline as [`check_connect`]; online, a host that is not loopback needs
/// `oxa.engine.allowRemote: true`.
pub fn check_engine_host(host: &str) -> Result<(), OxaError> {
    check_connect("the engine", host)?;
    if !is_loopback_host(host) && !remote_engine_allowed() {
        return Err(remote_engine_error(host));
    }
    Ok(())
}

/// P3: the refusal of an engine host that is not loopback (R6 shape).
pub fn remote_engine_error(host: &str) -> OxaError {
    problem(
        codes::REMOTE_ENGINE,
        &format!("Refused to use the engine at {host}"),
        "it is not a loopback address, so prompts and files would leave this machine over plain HTTP",
        "set \"oxa.engine.allowRemote\": true in the user settings.json, or use an engine on 127.0.0.1",
    )
}

/// P3: the startup notice when a remote engine is allowed (R6 shape).
pub fn remote_engine_notice(host: &str) -> String {
    problem_text(
        &format!("Remote engine {host}"),
        "prompts and files leave this machine unencrypted (plain HTTP)",
        "use an engine on 127.0.0.1 or an encrypted tunnel (ssh -L) to keep them private",
    )
}

/// T2 repair causes (`AgentEvent::ToolRepair::cause`, the `cause` of the stored repair entry).
pub mod repair_cause {
    pub const UNKNOWN_FUNCTION: &str = "unknown_function";
    pub const UNKNOWN_PARAMETER: &str = "unknown_parameter";
    pub const MISSING_REQUIRED: &str = "missing_required";
    pub const UNCLOSED: &str = "unclosed";
    pub const BAD_JSON: &str = "bad_json";
    pub const WRONG_TYPE: &str = "wrong_type";
    pub const FORMAT: &str = "format";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_and_text_render_the_same_two_lines() {
        let e = problem(crate::codes::DENIED, "A", "b", "c");
        assert_eq!(e.to_string(), problem_text("A", "b", "c"));
        assert_eq!(e.to_string(), "A: b\nFix: c");
    }

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "127.0.0.1",
            "127.9.8.7",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "0.0.0.0",
            "10.0.0.2",
            "192.168.1.1",
            "example.com",
            "localhost.example.com",
            "::",
            "128.0.0.1",
            "",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn url_hosts() {
        assert_eq!(url_host("http://127.0.0.1:8080/v1").as_deref(), Some("127.0.0.1"));
        assert_eq!(
            url_host("https://u:p@example.com/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(url_host("http://[::1]:80").as_deref(), Some("::1"));
        assert_eq!(url_host("nourl"), None);
    }
}
