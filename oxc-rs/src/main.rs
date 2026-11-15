#![forbid(unsafe_code)]

mod cli;

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
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
