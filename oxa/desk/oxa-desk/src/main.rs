//! Process entry of the desktop app: the shim of `oxa-cli/src/main.rs` plus one branch that opens
//! the window. `oxa-desk` must BE an oxa: the secret broker links `oxa-askpass` to this executable
//! and checks its peer against it, and workers, subagents and the local engine child re-run it.
//! Keep the order and the calls equal to oxa's `main`.
fn main() {
    // A1/A2: contained panics (extensions, tools, hooks) print nothing and become errors.
    oxa_proto::panic::install_hook();
    // #784: started as sudo's askpass helper (the `oxa-askpass` symlink in the broker directory):
    // nothing else of oxa starts.
    if oxa_cli::askpass::invoked_as_askpass(std::env::args_os().next().as_deref()) {
        std::process::exit(oxa_cli::askpass::askpass_main());
    }
    if let Some(result) = oxc_local::platform::child_entry(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The one branch oxa does not have.
    if oxa_desk::wants_window(&args) {
        std::process::exit(oxa_desk::run(&args));
    }
    let env = oxa_config::Env::from_process();
    // O-169: the Brave key stays in `env` only; no child process inherits it.
    oxa_web::remove_key_from_process_env();
    let code = oxa_cli::run(&args, &env, oxa_cli::Stdio::real());
    std::process::exit(code);
}
