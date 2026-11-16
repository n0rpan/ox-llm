//! D13: process entry. A thin shim over `oxa_cli::run`.
fn main() {
    // A1/A2: contained panics (extensions, tools, hooks) print nothing and become errors.
    oxa_proto::panic::install_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = oxa_config::Env::from_process();
    // O-169: the Brave key stays in `env` only; no child process inherits it.
    oxa_web::remove_key_from_process_env();
    let code = oxa_cli::run(&args, &env, oxa_cli::Stdio::real());
    std::process::exit(code);
}
