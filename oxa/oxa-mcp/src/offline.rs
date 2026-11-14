//! P4 (wave H lane `local`): the offline check of oxa-mcp's connect sites (the HTTP transport
//! and the OAuth requests). Offline, only a loopback server is reached; the error is the one
//! offline shape of `oxa_proto::offline_error`.
use oxa_proto::OxaError;

/// `Ok` unless offline mode is on and `url`'s host is not loopback (a URL without a host is
/// left to the request, which reports it).
pub(crate) fn check(url: &str) -> Result<(), OxaError> {
    match oxa_proto::url_host(url) {
        Some(host) => oxa_proto::check_connect("MCP over HTTP", &host),
        None => Ok(()),
    }
}

/// Item 1 (sec-prod): the argv a program with the user's rights starts with. Not offline: `argv`
/// unchanged. Offline: bwrap with no network (`oxa_sandbox::offline_argv`, bwrap found on
/// `path_var`); a missing bwrap is refused with the offline error for `what`. Never run with
/// network while offline.
pub(crate) fn launch(
    what: &str,
    offline: bool,
    path_var: &str,
    home: &std::path::Path,
    cwd: &std::path::Path,
    argv: Vec<String>,
) -> Result<Vec<String>, OxaError> {
    if !offline {
        return Ok(argv);
    }
    oxa_sandbox::offline_argv(path_var, home, cwd, &[], &argv).map_err(|_| oxa_proto::offline_error(what))
}
