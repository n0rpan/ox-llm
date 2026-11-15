//! Read-only, network-free inspection. Transport credentials are never accepted.
use crate::recovery::InspectedRun;
use serde::Serialize;
use std::io::{self, Write};
pub(crate) fn run(args: &[String]) -> i32 {
    if args == ["--help"] || args == ["-h"] {
        let _=io::stdout().write_all(b"Usage: oxa orchestrate-inspect --journal <path> [--worker <id> --checkpoint <path>]\n       oxa orchestrate-inspect --lineage <root-journal> [--lineage <child-journal> ...]\n\nInspect a bounded transcript without writing, starting workers, contacting agents,\nreplaying input or signaling recorded PIDs. Optionally recover a journal-bound\nworker checkpoint as a one-worker run report, or exclusively collect a selected\ncontinuation lineage as a full root report. Missing/torn terminal records remain\nunknown; a historical outcome is not proof of current process state.\n");
        return 0;
    }
    if args.chunks(2).any(|pair| pair[0] == "--lineage") {
        if args.is_empty()
            || args.len() > 32
            || !args
                .chunks(2)
                .all(|pair| pair.len() == 2 && pair[0] == "--lineage" && !pair[1].is_empty())
        {
            let _ = writeln!(
                io::stderr(),
                "oxa orchestrate-inspect: use 1..16 repeated --lineage <journal> paths in root-to-leaf order"
            );
            return 2;
        }
        let result = args
            .chunks(2)
            .map(|pair| crate::recovery::ExclusiveRun::claim(std::path::Path::new(&pair[1])))
            .collect::<io::Result<Vec<_>>>()
            .and_then(|owners| crate::lineage::collect(&owners.iter().collect::<Vec<_>>()));
        return match result {
            Ok(report) => print(&report),
            Err(error) => {
                let _ = writeln!(io::stderr(), "oxa orchestrate-inspect: {error}");
                1
            }
        };
    }
    let mut journal = None;
    let mut worker = None;
    let mut checkpoint = None;
    let valid = args.chunks(2).all(|pair| {
        if pair.len() != 2 || pair[1].is_empty() {
            return false;
        }
        let slot = match pair[0].as_str() {
            "--journal" => &mut journal,
            "--worker" => &mut worker,
            "--checkpoint" => &mut checkpoint,
            _ => return false,
        };
        slot.replace(pair[1].as_str()).is_none()
    });
    if !valid || journal.is_none() || worker.is_some() != checkpoint.is_some() {
        let _ = writeln!(
            io::stderr(),
            "oxa orchestrate-inspect: use --journal <path> [--worker <id> --checkpoint <path>] or --help"
        );
        return 2;
    }
    match InspectedRun::inspect(std::path::Path::new(journal.unwrap())) {
        Ok(run) => {
            if let (Some(worker), Some(checkpoint)) = (worker, checkpoint) {
                match run.load_checkpoint(worker, std::path::Path::new(checkpoint)) {
                    Ok(report) => print(&crate::a2a_run::A2ARunReport {
                        run: run.snapshot().run.clone(),
                        workers: vec![report],
                    }),
                    Err(error) => {
                        let _ = writeln!(io::stderr(), "oxa orchestrate-inspect: {error}");
                        1
                    }
                }
            } else {
                print(run.snapshot())
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "oxa orchestrate-inspect: {error}");
            1
        }
    }
}

fn print(value: &impl Serialize) -> i32 {
    let mut stdout = io::stdout().lock();
    let mut serializer = serde_json::Serializer::with_formatter(&mut stdout, crate::cli::TerminalJson);
    if value.serialize(&mut serializer).is_err() || stdout.write_all(b"\n").is_err() {
        1
    } else {
        0
    }
}
