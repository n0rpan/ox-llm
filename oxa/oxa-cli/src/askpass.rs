//! #784: the askpass entry sudo runs. The session's broker directory holds a symlink named
//! `oxa-askpass` to the running oxa executable; `SUDO_ASKPASS` names it, and `main` hands control
//! here when it is started under that name. The password goes owner -> broker -> this process ->
//! sudo's pipe (stdout): it is never an argument, never logged, never in tool output or the
//! transcript.
use std::io::Write;
use std::path::Path;

/// The name `main` dispatches on (the basename of `argv[0]`).
pub const ENTRY_NAME: &str = oxa_core::owner_confirm::ASKPASS_NAME;

/// `true` when this process was started as the askpass helper.
pub fn invoked_as_askpass(argv0: Option<&std::ffi::OsStr>) -> bool {
    argv0
        .map(Path::new)
        .and_then(Path::file_name)
        .is_some_and(|name| name == ENTRY_NAME)
}

/// Ask the broker at `sock` (the `OXA_CONFIRM_SOCK` of the session) for the password sudo's
/// prompt (`args[0]`) wants, and write it to `out` with a newline. Exit code 0 when printed,
/// 1 when nobody answered (sudo then fails with its own error).
pub fn askpass(args: &[String], sock: Option<&Path>, out: &mut dyn Write) -> i32 {
    let prompt = args.first().map(String::as_str).unwrap_or("Password:");
    let Some(sock) = sock else { return 1 };
    match oxa_core::owner_confirm::ask_secret(sock, prompt) {
        Some(secret) if writeln!(out, "{secret}").and_then(|()| out.flush()).is_ok() => 0,
        _ => 1,
    }
}

/// The process entry: `argv[1]` is sudo's prompt, the socket comes from `OXA_CONFIRM_SOCK`.
pub fn askpass_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sock = std::env::var_os(oxa_core::owner_confirm::SOCK_ENV).map(std::path::PathBuf::from);
    askpass(&args, sock.as_deref(), &mut std::io::stdout())
}
