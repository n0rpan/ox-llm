//! The `oxa-fake` binary (architect-owned shim).
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(oxa_fake::main_from_args(&args));
}
