#![forbid(unsafe_code)]

mod bootstrap;
mod bootstrap_install;
mod catalog;
mod cli;
mod cli_args;
mod cli_client;
mod cli_control;
mod cli_status;
mod daemon;
mod diagnostics;
mod engine;
mod engine_cache;
mod engine_ledger;
mod guard;
mod harness;
mod health_digest;
mod ledger;
mod models;
mod paths;
mod prefix_store;
mod proxy;
mod receipt_tools;
mod release_files;
mod release_update;
mod request_ledger;
mod responses;
mod share_network;
mod share_runtime;
mod share_tls;
mod sharing;
mod state_cache;
mod storage;
mod studio;
mod transport;
mod uninstall;

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    match words.first().map(String::as_str) {
        Some("__bootstrap") => std::process::exit(bootstrap::cli(&words[1..])),
        Some("__export_receipt") => std::process::exit(receipt_tools::export_cli(&words[1..])),
        Some("__scrub_receipts") => std::process::exit(receipt_tools::scrub_cli(&words[1..])),
        _ => {}
    }
    if words.first().map(String::as_str) == Some("__studio_exec") {
        use std::os::unix::process::CommandExt;
        if words.len() < 2 {
            std::process::exit(2);
        }
        let prompt = match nix::unistd::dup(std::io::stderr()) {
            Ok(fd) => fd,
            Err(_) => std::process::exit(1),
        };
        let output = match nix::unistd::dup(std::io::stdout()) {
            Ok(fd) => fd,
            Err(_) => std::process::exit(1),
        };
        if nix::fcntl::fcntl(
            &output,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )
        .is_err()
        {
            std::process::exit(1);
        }
        use std::os::fd::AsRawFd;
        let _error = std::process::Command::new(&words[1])
            .args(&words[2..])
            .args([
                "--prompt-file",
                &format!("/proc/self/fd/{}", prompt.as_raw_fd()),
            ])
            .stderr(std::process::Stdio::from(output))
            .exec();
        std::process::exit(1);
    }
    if words.first().map(String::as_str) == Some("__desktop_exec") {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};
        use std::{io::Write, os::unix::process::CommandExt};
        // A CLOEXEC error pipe preserves Popen's exec-success handshake while
        // allowing setsid without an unsafe pre_exec closure. Desktop stdin
        // and umask remain inherited from the CLI.
        let descriptor = nix::unistd::dup(std::io::stderr()).and_then(|fd| {
            fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
            Ok(fd)
        });
        let Ok(descriptor) = descriptor else {
            eprint!("E");
            std::process::exit(1);
        };
        let mut errors = std::fs::File::from(descriptor);
        if words.len() >= 2 && nix::unistd::setsid().is_ok() {
            let _ = std::process::Command::new(&words[1])
                .args(&words[2..])
                .stderr(std::process::Stdio::null())
                .exec();
        }
        let _ = errors.write_all(b"E");
        std::process::exit(1);
    }
    let ledger_exec = words.first().map(String::as_str) == Some("__engine_ledger_exec");
    if ledger_exec || words.first().map(String::as_str) == Some("__engine_exec") {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        if words.len() < 2 {
            std::process::exit(2);
        }
        if nix::unistd::setsid().is_err() {
            std::process::exit(1);
        }
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        let mut args = words[2..].to_vec();
        let mut command = std::process::Command::new(&words[1]);
        // This helper runs before Tokio starts, with all three standard
        // descriptors installed by Command. dup leaves CLOEXEC clear only in
        // this single-threaded child, never in the controller's descriptor table.
        let _ledger_fd = if ledger_exec {
            if args.len() < 2
                || args[args.len() - 2] != "--ledger"
                || args.last().map(String::as_str) != Some(engine_ledger::PIPE_ARGUMENT)
            {
                std::process::exit(2);
            }
            let descriptor = match nix::unistd::dup(std::io::stdin()) {
                Ok(descriptor) => descriptor,
                Err(_) => std::process::exit(1),
            };
            *args.last_mut().expect("ledger argument") =
                format!("/proc/self/fd/{}", descriptor.as_raw_fd());
            command.stdin(std::process::Stdio::null());
            Some(descriptor)
        } else {
            None
        };
        let error = command.args(args).exec();
        eprintln!("local-ai: engine exec failed: {error}");
        std::process::exit(1);
    }
    if matches!(words.first().map(String::as_str), Some("serve" | "studio")) {
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
    let code = if words.first().map(String::as_str) == Some("__install") {
        bootstrap_install::cli(&words[1..]).await
    } else if words.first().map(String::as_str) == Some("studio") {
        studio::run().await
    } else if words.first().map(String::as_str) == Some("serve") {
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
