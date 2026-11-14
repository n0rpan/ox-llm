//! Item 1 (sec-prod): programs the harness starts with the user's own rights (`!cmd`, stdio MCP
//! servers) while offline. They run through bwrap with the network unshared and the file system
//! as the user sees it: `--bind / /` (no deny-list mask, the cwd writable as usual). No bwrap
//! means no launch: the caller refuses with the offline error.
use crate::Bwrap;
use oxa_proto::OxaError;
use std::path::Path;

/// The bwrap argv (with `bwrap` first) that runs `argv` in `cwd` with `env` added, no network
/// and the user's file system. Pure and deterministic; golden-tested.
pub fn no_network_argv(bwrap: &Path, cwd: &Path, env: &[(String, String)], argv: &[String]) -> Vec<String> {
    let mut a: Vec<String> = vec![bwrap.to_string_lossy().into_owned()];
    a.extend(
        [
            "--unshare-net",
            "--die-with-parent",
            "--bind",
            "/",
            "/",
            "--dev-bind",
            "/dev",
            "/dev",
            "--proc",
            "/proc",
            "--chdir",
        ]
        .iter()
        .map(|x| (*x).to_string()),
    );
    a.push(cwd.to_string_lossy().into_owned());
    for (k, v) in env {
        a.push("--setenv".to_string());
        a.push(k.clone());
        a.push(v.clone());
    }
    a.push("--".to_string());
    a.extend(argv.iter().cloned());
    a
}

/// [`no_network_argv`] with bwrap found on `path_var` and probed (as [`Bwrap::detect`] does).
/// `Err` when bwrap is missing or unusable: the caller must refuse, never run without the wrap.
pub fn offline_argv(
    path_var: &str,
    home: &Path,
    cwd: &Path,
    env: &[(String, String)],
    argv: &[String],
) -> Result<Vec<String>, OxaError> {
    let b = Bwrap::detect(path_var, home)?;
    Ok(no_network_argv(&b.bwrap, cwd, env, argv))
}
