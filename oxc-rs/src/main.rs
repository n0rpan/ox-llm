#![forbid(unsafe_code)]

mod cli;
mod daemon;
mod ledger;
mod models;
mod paths;
mod transport;

#[tokio::main]
async fn main() {
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
