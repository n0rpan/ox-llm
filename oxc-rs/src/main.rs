#![forbid(unsafe_code)]

mod cli;

fn main() {
    let words = cli::words(std::env::args_os().skip(1));
    let code = match cli::parse(&words) {
        Err(reply) => {
            print!("{}", reply.stdout);
            eprint!("{}", reply.stderr);
            reply.code
        }
        Ok(_) => {
            eprintln!(
                "oxc-native: command execution is unavailable in this experimental parser build."
            );
            2
        }
    };
    std::process::exit(code);
}
