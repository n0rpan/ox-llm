use crate::{git, output, Result, RunPolicy};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const MAX_DIFF_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MAIN_MERGES: usize = 16;

/// Own-diff evidence from already available Git objects. The caller supplies the
/// trusted base and the GitHub snapshot's exact head, never PR-provided refs.
pub struct GitDiff {
    pub patch_id: String,
    pub main_merge_predecessors: Vec<String>,
}

impl GitDiff {
    pub fn read(policy: &RunPolicy, repo: &Path, base: &str, head: &str) -> Result<Option<Self>> {
        if !object_id(base) || !object_id(head) {
            return Err("review evidence requires full Git object IDs".into());
        }
        let Some(patch_id) = own_patch_id(policy, repo, base, head)? else {
            return Ok(None);
        };
        let mut current = head.to_owned();
        let mut main_merge_predecessors = Vec::new();
        for _ in 0..MAX_MAIN_MERGES {
            let parents = git_line(
                policy,
                repo,
                &["rev-list", "--parents", "--max-count=1", &current, "--"],
            )?;
            let fields: Vec<_> = parents.split_ascii_whitespace().collect();
            let [_, previous, main] = fields.as_slice() else {
                break;
            };
            if !is_ancestor(policy, repo, main, base)?
                || is_ancestor(policy, repo, main, previous)?
                || own_patch_id(policy, repo, base, previous)?.as_ref() != Some(&patch_id)
            {
                break;
            }
            current = (*previous).to_owned();
            main_merge_predecessors.push(current.clone());
        }
        Ok(Some(Self {
            patch_id,
            main_merge_predecessors,
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

fn own_patch_id(policy: &RunPolicy, repo: &Path, base: &str, head: &str) -> Result<Option<String>> {
    let start = git_line(policy, repo, &["merge-base", "--all", base, head])?;
    if !object_id(&start) {
        return Err("review evidence requires one Git merge base".into());
    }
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
            &start,
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
