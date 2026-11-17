//! Mandatory process tracing, including checkout-local prerequisite installs.
use std::path::{Path, PathBuf};

pub fn executable() -> PathBuf {
    // Fresh check shells need not inherit the setup shell's PATH. Never fall back
    // after a tracer launch failure: an installed but broken tracer must fail.
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local-checks/strace/bin/strace");
    if local.is_file() {
        local
    } else {
        PathBuf::from("strace")
    }
}
