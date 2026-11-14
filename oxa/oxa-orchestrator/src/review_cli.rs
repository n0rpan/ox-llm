//! Local review command. No worker/engine/network/configuration setup.
use crate::review::{proposals_from_report, PreparedReview};
use crate::workspace::{RelativePath, Snapshot};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

const HELP: &str = r#"Usage: oxa orchestrate-review [options]

Review selected proposals from a saved orchestrate JSON report. No inference.
Print a JSON preview, including diffs, conflicts and an approval SHA-256.

Required:
  --report <path>               Saved JSON report (plain file, at most 16 MiB)
  --source <path>               Source root for the original approved inputs
  --file <relative-path>        Repeat the exact original approved file scope
  --select <worker-id>          Explicit worker selection; repeat up to 16

Options:
  --allow-new <relative-path>   Explicit eligible new-file scope; repeat
  --directory <path>            Stage in a NEW private candidate directory
  --approve <sha256>            Exact preview digest; requires --directory
  --help                       Show help without reading files

Without --directory/--approve this command only previews. Staging copies the
approved source snapshot and selected proposals into a new candidate, with a
receipt. Source files are never changed. Overlaps require a different explicit
worker selection; there is no automatic conflict resolution or Git merge.
"#;
struct Options {
    report: PathBuf,
    source: PathBuf,
    files: Vec<RelativePath>,
    selected: Vec<String>,
    new_files: Vec<RelativePath>,
    directory: Option<PathBuf>,
    approval: Option<String>,
}
fn parse(args: &[String]) -> Result<Option<Options>, &'static str> {
    let mut report = None;
    let mut source = None;
    let mut files = Vec::new();
    let mut selected = Vec::new();
    let mut new_files = Vec::new();
    let mut directory = None;
    let mut approval = None;
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        if matches!(flag, "--help" | "-h") {
            return Ok(None);
        }
        if !matches!(
            flag,
            "--report" | "--source" | "--file" | "--select" | "--allow-new" | "--directory" | "--approve"
        ) {
            return Err("unknown review option; use --help");
        }
        if !matches!(flag, "--file" | "--select" | "--allow-new") && !seen.insert(flag) {
            return Err("duplicate review option");
        }
        let value = args
            .get(i)
            .filter(|s| !s.is_empty())
            .ok_or("review option needs a nonempty value")?;
        i += 1;
        match flag {
            "--report" => report = Some(PathBuf::from(value)),
            "--source" => source = Some(PathBuf::from(value)),
            "--directory" => directory = Some(PathBuf::from(value)),
            "--approve" => {
                if value.len() != 64
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err("approval must be a lowercase SHA-256 digest");
                }
                approval = Some(value.clone());
            }
            "--select" => {
                if selected.len() >= 16 || selected.contains(value) {
                    return Err("select 1..16 distinct worker IDs");
                }
                selected.push(value.clone());
            }
            "--file" | "--allow-new" => {
                let path =
                    RelativePath::new(value.clone()).map_err(|_| "invalid or protected review path")?;
                let paths = if flag == "--file" {
                    &mut files
                } else {
                    &mut new_files
                };
                if paths.len() >= 32 || paths.contains(&path) {
                    return Err("duplicate or excessive review paths");
                }
                paths.push(path);
            }
            _ => unreachable!(),
        }
    }
    if files.is_empty() || files.len() + new_files.len() > 32 || selected.is_empty() {
        return Err("provide 1..32 eligible paths and at least one --select worker");
    }
    if directory.is_some() != approval.is_some() {
        return Err("--directory and --approve must be supplied together");
    }
    Ok(Some(Options {
        report: report.ok_or("--report is required")?,
        source: source.ok_or("--source is required")?,
        files,
        selected,
        new_files,
        directory,
        approval,
    }))
}

pub(crate) fn run(args: &[String]) -> i32 {
    let options = match parse(args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            let _ = io::stdout().write_all(HELP.as_bytes());
            return 0;
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "oxa orchestrate-review: {error}");
            return 2;
        }
    };
    match execute(options) {
        Ok(review) => {
            let mut stdout = io::stdout().lock();
            let mut serializer =
                serde_json::Serializer::with_formatter(&mut stdout, crate::cli::TerminalJson);
            if review.preview().serialize(&mut serializer).is_err() || stdout.write_all(b"\n").is_err() {
                return 1;
            }
            if review.preview().conflicts.is_empty() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "oxa orchestrate-review: {error}");
            1
        }
    }
}
fn execute(options: Options) -> io::Result<PreparedReview> {
    let proposals = proposals_from_report(&options.report, &options.selected)?;
    let snapshot = Arc::new(Snapshot::approve(&options.source, options.files)?);
    let review = PreparedReview::prepare(snapshot, options.new_files, proposals)?;
    for proposal in &review.preview().proposals {
        proposal.review.verify_current_base(&options.source)?;
    }
    if let Some(directory) = options.directory {
        review.stage(&directory, options.approval.as_deref().unwrap())?;
    }
    Ok(review)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(extra: &[&str]) -> Vec<String> {
        [
            "--report",
            "unused.json",
            "--source",
            "unused-source",
            "--file",
            "a.txt",
            "--select",
            "worker-1",
        ]
        .into_iter()
        .chain(extra.iter().copied())
        .map(str::to_owned)
        .collect()
    }
    #[test]
    fn parsing_is_inert_and_requires_explicit_selection_and_approval() {
        assert!(parse(&["--help".into()]).unwrap().is_none());
        assert!(parse(&args(&[])).unwrap().is_some());
        for extra in [
            vec!["--select", "worker-1"],
            vec!["--directory", "candidate"],
            vec!["--approve", "bad"],
            vec!["--file", "../bad"],
            vec!["--file", ".oxa-review.json"],
            vec!["--engine", "127.0.0.1:1"],
        ] {
            assert!(parse(&args(&extra)).is_err());
        }
    }
}
