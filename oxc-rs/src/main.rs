#![forbid(unsafe_code)]

mod cli;
mod cli_client;
mod cli_control;
mod ledger;
mod paths;

fn main() {
    let words = cli::words(std::env::args_os().skip(1));
    let code = match cli::parse(&words) {
        Err(reply) => {
            print!("{}", reply.stdout);
            eprint!("{}", reply.stderr);
            reply.code
        }
        Ok(args) if args.action == "ledger" => {
            let limit = args
                .value("--limit")
                .and_then(cli::integer)
                .unwrap_or(50)
                .clamp(1, 200);
            let value = ledger::recent(limit as usize);
            if args.flags.contains("--json") {
                println!("{}", paths::python_json(&value));
            } else if value["ledger_available"] == true {
                println!(
                    "{} requests recorded",
                    value["rows"].as_array().map_or(0, Vec::len)
                );
            } else {
                println!("No request ledger");
            }
            0
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
