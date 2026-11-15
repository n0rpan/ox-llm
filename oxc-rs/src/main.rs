#![forbid(unsafe_code)]

mod cli;
mod cli_client;
mod cli_control;

fn main() {
    let words = cli::words(std::env::args_os().skip(1));
    let code = match cli::parse(&words) {
        Err(reply) => {
            print!("{}", reply.stdout);
            eprint!("{}", reply.stderr);
            reply.code
        }
        Ok(args) if cli_control::supports(&args.action) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("CLI runtime");
            runtime.block_on(cli_control::run(&args))
        }
        Ok(_) => {
            eprintln!("oxc-native: this command is unavailable in this experimental CLI build.");
            2
        }
    };
    std::process::exit(code);
}
