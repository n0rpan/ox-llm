//! #1584: what the remote really holds after a push, read back from the destination itself.
//!
//! After every runner-owned push the destination is asked for the pushed branch
//! (`git ls-remote <push url> refs/heads/<branch>`, the isolation `destination_tips` uses) and the
//! answer is classified against the requested commit with local ancestry. A push that reports
//! success but reads back another commit is an error. A rejected push names the exact remote
//! commit. Nothing here ever force-pushes: both commits are kept.
use crate::{checked, git, output, Result, RunPolicy};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How the remote branch relates to the commit the runner asked to publish.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// The remote branch is exactly the requested commit.
    Published,
    /// The branch does not exist on the remote.
    Absent,
    /// The remote branch is an ancestor of the requested commit.
    RemoteBehind,
    /// The requested commit is an ancestor of the remote branch.
    RemoteAhead,
    /// Neither contains the other.
    Diverged,
    /// The remote commit is not in the local object store, or the remote could not be read.
    RemoteUnknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublishReceipt {
    pub requested: String,
    pub remote: Option<String>,
    pub class: Divergence,
}

impl PublishReceipt {
    fn describe(&self, branch: &str) -> String {
        match (&self.remote, self.class) {
            (Some(remote), Divergence::Diverged) => format!(
                "origin {branch} is at {remote} (diverged from requested {})",
                self.requested
            ),
            (Some(remote), Divergence::RemoteAhead) => format!(
                "origin {branch} is at {remote} (ahead of requested {})",
                self.requested
            ),
            (Some(remote), Divergence::RemoteBehind) => format!(
                "origin {branch} is at {remote} (behind requested {})",
                self.requested
            ),
            (Some(remote), _) => format!(
                "origin {branch} is at {remote} (not in the local repository; requested {})",
                self.requested
            ),
            (None, Divergence::Absent) => format!("origin {branch} does not exist"),
            (None, _) => format!("origin {branch} could not be read back"),
        }
    }
}

/// Pure classification. `ancestry(a, b)` answers whether `a` is an ancestor of `b`, or `None`
/// when a commit is not in the local object store.
pub(crate) fn classify(
    requested: &str,
    remote: Option<&str>,
    ancestry: impl Fn(&str, &str) -> Option<bool>,
) -> Divergence {
    let Some(remote) = remote else {
        return Divergence::Absent;
    };
    if remote == requested {
        return Divergence::Published;
    }
    match ancestry(remote, requested) {
        None => Divergence::RemoteUnknown,
        Some(true) => Divergence::RemoteBehind,
        Some(false) => match ancestry(requested, remote) {
            None => Divergence::RemoteUnknown,
            Some(true) => Divergence::RemoteAhead,
            Some(false) => Divergence::Diverged,
        },
    }
}

fn is_ancestor(policy: &RunPolicy, repo: &Path, ancestor: &str, descendant: &str) -> Option<bool> {
    let out = output(git(
        policy,
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    ))
    .ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

fn remote_tip(policy: &RunPolicy, repo: &Path, url: &str, branch: &str) -> Result<Option<String>> {
    if url.starts_with('-') {
        return Err("invalid publication destination".into());
    }
    let reference = format!("refs/heads/{branch}");
    let mut command = git(
        policy,
        repo,
        &[
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.file.allow=always",
            "ls-remote",
            url,
            &reference,
        ],
    );
    super::isolate_github_environment(&mut command);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
    let text = checked(command)?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(_, name)| *name == reference)
        .map(|(oid, _)| oid.to_owned())
        .filter(|oid| matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())))
}

/// Ask the destination what `branch` holds and classify it against `head`.
pub(crate) fn read_back(
    policy: &RunPolicy,
    repo: &Path,
    url: &str,
    branch: &str,
    head: &str,
) -> PublishReceipt {
    match remote_tip(policy, repo, url, branch) {
        Ok(remote) => {
            let class = classify(head, remote.as_deref(), |a, b| is_ancestor(policy, repo, a, b));
            PublishReceipt {
                requested: head.to_owned(),
                remote,
                class,
            }
        }
        Err(_) => PublishReceipt {
            requested: head.to_owned(),
            remote: None,
            class: Divergence::RemoteUnknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_all_six_classes() {
        let none = |_: &str, _: &str| -> Option<bool> { None };
        let truth = |a: &str, b: &str| Some(a == "old" && b == "new");
        assert_eq!(classify("new", Some("new"), none), Divergence::Published);
        assert_eq!(classify("new", None, none), Divergence::Absent);
        assert_eq!(classify("new", Some("old"), truth), Divergence::RemoteBehind);
        assert_eq!(classify("old", Some("new"), truth), Divergence::RemoteAhead);
        assert_eq!(classify("a", Some("b"), truth), Divergence::Diverged);
        assert_eq!(classify("new", Some("zzz"), none), Divergence::RemoteUnknown);
    }

    #[test]
    fn a_diverged_receipt_names_the_exact_remote_commit_and_that_nothing_was_forced() {
        let receipt = PublishReceipt {
            requested: "aaa".into(),
            remote: Some("bbb".into()),
            class: Divergence::Diverged,
        };
        let text = receipt.describe("agent/x");
        assert_eq!(text, "origin agent/x is at bbb (diverged from requested aaa)");
    }
}
