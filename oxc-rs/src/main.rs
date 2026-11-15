#![forbid(unsafe_code)]

mod cli_args;
mod cli_client;
mod cli_control;

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let code = match cli_args::parse(&words) {
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
