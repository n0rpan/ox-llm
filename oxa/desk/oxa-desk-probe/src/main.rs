//! oxa-desk-probe: measures the desktop app against `gate.toml` (the prototype gate and the CI
//! budget). Never shipped. It measures nothing yet and says so with a failing exit code, so no
//! script can read an empty run as a passed one.
fn main() {
    eprintln!("oxa-desk-probe: no measurement exists yet");
    std::process::exit(2);
}
