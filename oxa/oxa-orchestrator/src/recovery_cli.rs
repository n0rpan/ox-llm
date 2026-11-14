//! Read-only, network-free inspection. Transport credentials are never accepted.
use crate::recovery::InspectedRun;
use serde::Serialize;
use std::io::{self, Write};
pub(crate) fn run(args: &[String]) -> i32 {
    if args == ["--help"] || args == ["-h"] {
        let _=io::stdout().write_all(b"Usage: oxa orchestrate-inspect --journal <path>\n\nInspect a bounded transcript without writing, starting workers, contacting agents,\nreplaying input or signaling recorded PIDs. Missing/torn terminal records remain\nunknown; a historical outcome is not proof of current process state.\n");
        return 0;
    }
    if args.len() != 2 || args[0] != "--journal" || args[1].is_empty() {
        let _ = writeln!(
            io::stderr(),
            "oxa orchestrate-inspect: use --journal <path> or --help"
        );
        return 2;
    }
    match InspectedRun::inspect(std::path::Path::new(&args[1])) {
        Ok(run) => {
            let mut stdout = io::stdout().lock();
            let mut serializer =
                serde_json::Serializer::with_formatter(&mut stdout, crate::cli::TerminalJson);
            if run.snapshot().serialize(&mut serializer).is_err() || stdout.write_all(b"\n").is_err() {
                1
            } else {
                0
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "oxa orchestrate-inspect: {error}");
            1
        }
    }
}
