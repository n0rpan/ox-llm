#![forbid(unsafe_code)]

mod cli;
mod daemon;
mod diagnostics;
mod engine;
mod guard;
mod ledger;
mod models;
mod paths;
mod proxy;
mod responses;
mod transport;

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    if words.first().map(String::as_str) == Some("__engine_exec") {
        use std::os::unix::process::CommandExt;
        if words.len() < 2 {
            std::process::exit(2);
        }
        if nix::unistd::setsid().is_err() {
            std::process::exit(1);
        }
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        let error = std::process::Command::new(&words[1])
            .args(&words[2..])
            .exec();
        eprintln!("local-ai: engine exec failed: {error}");
        std::process::exit(1);
    }
    if words.first().map(String::as_str) == Some("serve") {
        if let Err(error) =
            nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_CORE, 0, 0)
        {
            eprintln!("local-ai: cannot disable core dumps: {error}");
            std::process::exit(1);
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("async runtime");
    runtime.block_on(run());
}

async fn run() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let code = if words.first().map(String::as_str) == Some("serve") {
        daemon::run().await
    } else {
        match cli::parse(&words) {
            Ok(args) => cli::run(args).await,
            Err(reply) => {
                print!("{}", reply.stdout);
                eprint!("{}", reply.stderr);
                reply.code
            }
        }
    };
    std::process::exit(code);
}
