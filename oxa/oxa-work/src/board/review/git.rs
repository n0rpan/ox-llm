use super::Reviewer;
use crate::{git, output, Result, RunPolicy};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const MAX_DIFF_BYTES: u64 = 2 * 1024 * 1024;
const MAX_APPROVAL_STEPS: usize = 16;

/// Own-diff evidence from already available Git objects. The caller supplies the
/// trusted base and the GitHub snapshot's exact head, never PR-provided refs.
pub struct GitDiff {
    pub patch_id: String,
    pub main_merge_predecessors: Vec<String>,
    pub owner_approval_predecessors: Vec<String>,
    pub files: Vec<GitFileChange>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GitFileChange {
    pub path: String,
    pub previous_path: Option<String>,
    pub deleted_file: bool,
    pub deleted_lines: u64,
}

impl GitDiff {
    pub fn read(
        policy: &RunPolicy,
        repo: &Path,
        base: &str,
        head: &str,
        approvals: &[(Reviewer, &str)],
        carry_path: impl Fn(&str) -> bool,
    ) -> Result<Option<Self>> {
        if !object_id(base) || !object_id(head) {
            return Err("review evidence requires full Git object IDs".into());
        }
        let start = merge_base(policy, repo, base, head)?;
        let Some(patch_id) = own_patch_id(policy, repo, &start, head)? else {
            return Ok(None);
        };
        let files = own_file_changes(policy, repo, &start, head)?;
        let mut current = head.to_owned();
        let mut main_merge_predecessors = Vec::new();
        let mut owner_approval_predecessors = Vec::new();
        let mut owner_only = false;
        let mut candidates: Vec<_> = approvals
            .iter()
            .copied()
            .filter(|&(_, reviewed)| reviewed != head)
            .collect();
        let mut current_patch = patch_id.clone();
        for _ in 0..MAX_APPROVAL_STEPS {
            if candidates.is_empty() {
                break;
            }
            let parents = git_line(
                policy,
                repo,
                &["rev-list", "--parents", "--max-count=1", &current, "--"],
            )?;
            let fields: Vec<_> = parents.split_ascii_whitespace().collect();
            let [_, previous, rest @ ..] = fields.as_slice() else {
                break;
            };
            if rest.is_empty() {
                // Only an existing owner approval survives an ordinary docs/test delta.
                candidates.retain(|&(reviewer, _)| reviewer == Reviewer::Owner);
                if candidates.is_empty()
                    || !own_file_changes(policy, repo, previous, &current).is_ok_and(|delta| {
                        delta.iter().all(|file| {
                            carry_path(&file.path) && file.previous_path.as_deref().is_none_or(&carry_path)
                        })
                    })
                {
                    break;
                }
                owner_only = true;
            }
            let previous_base = merge_base(policy, repo, base, previous)?;
            if previous_base == *previous {
                break;
            }
            let Some(previous_patch) = own_patch_id(policy, repo, &previous_base, previous)? else {
                break;
            };
            let can_carry = match rest {
                [] => true,
                [main] => {
                    previous_patch == current_patch
                        && is_ancestor(policy, repo, main, base)?
                        && !is_ancestor(policy, repo, main, previous)?
                }
                _ => false,
            };
            if !can_carry {
                break;
            }
            current = (*previous).to_owned();
            current_patch = previous_patch;
            let previous_count = candidates.len();
            candidates.retain(|&(_, reviewed)| reviewed != current);
            if candidates.len() != previous_count {
                if owner_only {
                    owner_approval_predecessors.push(current.clone());
                } else {
                    main_merge_predecessors.push(current.clone());
                }
            }
        }
        Ok(Some(Self {
            patch_id,
            main_merge_predecessors,
            owner_approval_predecessors,
            files,
        }))
    }
}

pub(super) fn object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn git_line(policy: &RunPolicy, repo: &Path, args: &[&str]) -> Result<String> {
    let mut command = git(policy, repo, args);
    command.stderr(Stdio::null());
    let out = output(command)?;
    if !out.status.success() {
        return Err("cannot read Git review evidence".into());
    }
    String::from_utf8(out.stdout)
        .map(|line| line.trim().to_owned())
        .map_err(|_| "invalid Git review evidence".into())
}

fn is_ancestor(policy: &RunPolicy, repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let mut command = git(
        policy,
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    );
    command.stdout(Stdio::null()).stderr(Stdio::null());
    match output(command)?.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("cannot verify Git main ancestry".into()),
    }
}

fn merge_base(policy: &RunPolicy, repo: &Path, base: &str, head: &str) -> Result<String> {
    let start = git_line(policy, repo, &["merge-base", "--all", base, head])?;
    if !object_id(&start) {
        return Err("review evidence requires one Git merge base".into());
    }
    Ok(start)
}

fn own_patch_id(policy: &RunPolicy, repo: &Path, start: &str, head: &str) -> Result<Option<String>> {
    let diff = git(
        policy,
        repo,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
            "--no-indent-heuristic",
            "--diff-algorithm=myers",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--ignore-submodules=none",
            "--submodule=short",
            start,
            head,
            "--",
        ],
    );
    let patch = read_diff(diff)?;
    if patch.is_empty() {
        return Ok(None);
    }
    let mut child = git(policy, repo, &["patch-id", "--verbatim"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cannot start Git patch identity calculation")?;
    let written = child.stdin.take().expect("piped stdin").write_all(&patch);
    let out = child
        .wait_with_output()
        .map_err(|_| "cannot finish Git patch identity calculation")?;
    if written.is_err() || !out.status.success() {
        return Err("cannot calculate Git patch identity".into());
    }
    let receipt = std::str::from_utf8(&out.stdout).map_err(|_| "invalid Git patch identity")?;
    let fields: Vec<_> = receipt.split_ascii_whitespace().collect();
    match fields.as_slice() {
        [patch_id, commit] if object_id(patch_id) && object_id(commit) => Ok(Some((*patch_id).to_owned())),
        _ => Err("invalid Git patch identity".into()),
    }
}

fn own_file_changes(policy: &RunPolicy, repo: &Path, start: &str, head: &str) -> Result<Vec<GitFileChange>> {
    let raw = read_diff(git(
        policy,
        repo,
        &[
            "diff",
            "--raw",
            "--numstat",
            "-z",
            "--find-renames=50%",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--no-color",
            "--ignore-submodules=none",
            start,
            head,
            "--",
        ],
    ))?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let invalid = "invalid Git review file inventory";
    let raw = std::str::from_utf8(&raw).map_err(|_| invalid)?;
    let mut records = raw.strip_suffix('\0').ok_or(invalid)?.split('\0').peekable();
    let mut files = Vec::new();
    while records.peek().is_some_and(|record| record.starts_with(':')) {
        let status = records
            .next()
            .and_then(|header| header.split_ascii_whitespace().last())
            .ok_or(invalid)?;
        let first = records.next().filter(|path| !path.is_empty()).ok_or(invalid)?;
        let (previous_path, path) = match status.as_bytes() {
            [b'R' | b'C', similarity @ ..]
                if !similarity.is_empty() && similarity.iter().all(u8::is_ascii_digit) =>
            {
                let path = records.next().filter(|path| !path.is_empty()).ok_or(invalid)?;
                (Some(first.to_owned()), path)
            }
            [b'A' | b'D' | b'M' | b'T'] => (None, first),
            _ => return Err(invalid.into()),
        };
        files.push(GitFileChange {
            path: path.to_owned(),
            previous_path,
            deleted_file: status == "D",
            deleted_lines: 0,
        });
    }
    for file in &mut files {
        let row = records.next().ok_or(invalid)?;
        let mut columns = row.splitn(3, '\t');
        let added = columns.next().ok_or(invalid)?;
        let deleted = columns.next().ok_or(invalid)?;
        let path = columns.next().ok_or(invalid)?;
        if let Some(previous) = &file.previous_path {
            if !path.is_empty()
                || records.next() != Some(previous.as_str())
                || records.next() != Some(file.path.as_str())
            {
                return Err(invalid.into());
            }
        } else if path != file.path {
            return Err(invalid.into());
        }
        file.deleted_lines = match (added, deleted) {
            ("-", "-") => 0,
            (added, deleted) if added.parse::<u64>().is_ok() => {
                deleted.parse::<u64>().map_err(|_| invalid)?
            }
            _ => return Err(invalid.into()),
        };
    }
    if files.is_empty() || records.next().is_some() {
        return Err(invalid.into());
    }
    Ok(files)
}

fn read_diff(mut command: Command) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cannot start Git review diff")?;
    let mut patch = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(MAX_DIFF_BYTES + 1)
        .read_to_end(&mut patch);
    let oversized = patch.len() as u64 > MAX_DIFF_BYTES;
    if oversized || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|_| "cannot finish Git review diff")?;
    if oversized {
        return Err("Git review diff exceeds the evidence limit".into());
    }
    if read.is_err() || !status.success() {
        return Err("cannot read Git review diff".into());
    }
    Ok(patch)
}
