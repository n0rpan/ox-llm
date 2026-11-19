//! Explicit publishing of the accepted commit; existing queued PRs are never pushed.
use super::{checked, state, tree, CheckpointObserver, LockedTask, Outcome, Record, Result, RunPolicy, Task};
use state::Step;
mod landing;
pub use landing::{
    Admission, AdmissionAcknowledgment, AdmissionReceipt, DeliveryReceipt, LandingPublisher, NoticeIntent,
    RejectedLanding, RetryIntent,
};
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn action_context(
    locked: &LockedTask<'_>,
    observer: &dyn CheckpointObserver,
) -> Result<(state::Writer, state::State)> {
    let path = locked.policy.storage.join(format!("{}.json", locked.task.id));
    let value = state::load(&path)?;
    if value.task != *locked.task {
        return Err("task specification changed".into());
    }
    let mut writer = state::Writer::new(path, true);
    writer.replay(&value, observer)?;
    Ok((writer, value))
}
const MAX_PUBLICATION_RESPONSE_BYTES: usize = 1024 * 1024;

/// One part of the commits a publication would push: the name of a file that a
/// commit adds (`line` is `None`, `text` is empty) or one added line.
pub struct PushedContent<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub text: &'a str,
}

/// Why pushed content must not be published. `label` never contains the content;
/// `secret_path` withholds a path that itself contains secret text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentFinding {
    pub label: &'static str,
    pub secret_path: bool,
}

/// Classifies pushed content before publication.
pub type ContentScan<'a> = &'a dyn Fn(&PushedContent<'_>) -> Option<ContentFinding>;

/// Decode a path Git printed in C-style quotes (`"b/a\"b\\c\303\251"`).
fn unquote(quoted: &str) -> Option<String> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escaped = chars.next()?;
        bytes.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'0'..=b'7' => {
                let mut value = u32::from(escaped - b'0');
                for _ in 0..2 {
                    let digit = chars.next().filter(|digit| (b'0'..=b'7').contains(digit))?;
                    value = value * 8 + u32::from(digit - b'0');
                }
                u8::try_from(value).ok()?
            }
            other => other,
        });
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn diff_path(header: &str) -> Option<String> {
    let path = header.trim_end_matches(['\r', '\n']);
    if path == "/dev/null" {
        return None;
    }
    let path = if path.starts_with('"') {
        unquote(path)?
    } else {
        // Git ends an unquoted `+++` path that contains a space with a tab. A tab in
        // a real name would make Git quote the path, so one trailing tab is never part of it.
        path.strip_suffix('\t').unwrap_or(path).to_string()
    };
    Some(path.strip_prefix("b/").map(str::to_string).unwrap_or(path))
}

fn refusal(path: &str, line: Option<usize>, finding: ContentFinding) -> String {
    let path: String = if finding.secret_path {
        "a file whose path".into()
    } else {
        path.chars()
            .map(|ch| if ch.is_control() { '?' } else { ch })
            .take(200)
            .collect()
    };
    let at = line.map_or(String::new(), |line| format!(":{line}"));
    format!(
        "publishing refused: {path}{at} adds {}; remove it from the task branch history before publishing",
        finding.label
    )
}

/// The commits a push can add: reachable from `head` and from none of `known`.
fn pushed_revisions<'a>(head: &'a str, known: &'a [String]) -> Vec<&'a str> {
    let mut revisions = vec!["--ignore-missing", head, "--not"];
    revisions.extend(known.iter().map(String::as_str));
    revisions
}

/// The branch and tag tips the publication destination itself advertises. Local
/// remote-tracking refs are writable by the task and may describe a different
/// fetch URL, so only the destination's own answer may exclude history.
fn destination_tips(policy: &RunPolicy, repo: &Path, url: &str) -> Result<Vec<String>> {
    if url.starts_with('-') {
        return Err("publishing refused: invalid publication destination".into());
    }
    let mut command = super::git(
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
            "--heads",
            "--tags",
            "--refs",
            url,
        ],
    );
    isolate_github_environment(&mut command);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
    let text = checked(command)
        .map_err(|_| "publishing refused: cannot read the publication destination's branches".to_string())?;
    let mut tips: Vec<String> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, _)| oid)
        .filter(|oid| matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect();
    tips.sort();
    tips.dedup();
    Ok(tips)
}

/// Scan the commits `revisions` select: each raw commit object, its own changes and
/// the lines a merge commit introduces itself. Any failure refuses publication.
fn scan_pushed(policy: &RunPolicy, tree: &Path, revisions: &[&str], scan: ContentScan<'_>) -> Result<()> {
    let unreadable = |_: String| "publishing refused: cannot scan the commits to publish".to_string();
    let log = |options: &[&str]| {
        // No user or repository preference may change what the scan sees.
        let mut args = vec![
            "-c",
            "core.quotePath=false",
            "-c",
            "log.showRoot=true",
            "-c",
            "log.showSignature=false",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "diff.relative=false",
            "-c",
            "color.ui=false",
            "-c",
            "color.diff=false",
            "log",
            "--root",
        ];
        args.extend_from_slice(options);
        args.extend_from_slice(revisions);
        args.push("--");
        checked(super::git(policy, tree, &args)).map_err(unreadable)
    };
    // Every byte of each commit object is pushed: identities, the message and any
    // extra header (gpgsig, mergetag, encoding, unknown ones), so the raw object is
    // scanned rather than a formatted subset of it.
    let mut listing = vec!["rev-list", "--reverse"];
    listing.extend_from_slice(revisions);
    listing.push("--");
    let commits = checked(super::git(policy, tree, &listing)).map_err(unreadable)?;
    for commit in commits.lines().filter(|line| !line.is_empty()) {
        if !valid_object_id(commit) {
            return Err(unreadable(String::new()));
        }
        let raw = checked(super::git(policy, tree, &["cat-file", "commit", commit])).map_err(unreadable)?;
        scan_commit_object(commit, &raw, scan)?;
    }
    let text = log(&[
        "--reverse",
        "--format=%x00%H",
        "--patch",
        "--cc",
        "--text",
        "--unified=0",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-relative",
        "--no-show-signature",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ])?;
    // `--unified=0` shows no unchanged line, so the key that owns an edited block scalar
    // is read from the commit's own copy of the file.
    let file_at = |commit: &str, path: &str| {
        if !valid_object_id(commit) {
            return None;
        }
        let spec = format!("{commit}:{path}");
        checked(super::git(policy, tree, &["show", "--no-textconv", &spec])).ok()
    };
    scan_patch(&text, scan, &file_at)?;
    // An empty added file has no content header in the patch, so the names of all
    // added files are listed separately and checked as well.
    let names = log(&[
        "--format=",
        "--name-only",
        "-z",
        "--diff-filter=A",
        "--cc",
        "--no-renames",
        "--no-relative",
    ])?;
    for name in names.split('\0').filter(|name| !name.is_empty()) {
        let item = PushedContent {
            path: name,
            line: None,
            text: "",
        };
        if let Some(finding) = scan(&item) {
            return Err(refusal(name, None, finding));
        }
    }
    Ok(())
}

/// A full lowercase hexadecimal object id (SHA-1 or SHA-256).
fn valid_object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Pass each line of one raw commit object (`git cat-file commit`: all headers,
/// signature blocks and the message) to `scan`.
fn scan_commit_object(commit: &str, raw: &str, scan: ContentScan<'_>) -> Result<()> {
    let place = format!("commit {}", commit.get(..12).unwrap_or(commit));
    for (index, text) in raw.lines().enumerate() {
        let item = PushedContent {
            path: &place,
            line: Some(index + 1),
            text,
        };
        if let Some(finding) = scan(&item) {
            return Err(refusal(&place, Some(index + 1), finding));
        }
    }
    Ok(())
}

/// Whether a YAML line ends in a block scalar indicator (`|`, `>`, with `-`/`+`).
fn opens_block_scalar(row: &str) -> bool {
    // `key: |`, `|2-`, `|+2`, `>2` and a trailing comment are all valid headers. The
    // indicator is the last run of `|`/`>` plus optional chomping and indentation markers.
    let mut text = row.trim_end();
    if let Some((before, _)) = text.split_once(" #") {
        text = before.trim_end();
    }
    let Some(at) = text.rfind(['|', '>']) else {
        return false;
    };
    let marker = &text[at + 1..];
    let mut chars = marker.chars().peekable();
    let mut chomp = false;
    let mut indent = false;
    while let Some(c) = chars.next() {
        match c {
            '+' | '-' if !chomp => chomp = true,
            '1'..='9' if !indent => indent = true,
            '0' => return false,
            _ => return false,
        }
    }
    true
}

/// The line that opens the block scalar `current` (the added line at 1-based `line`)
/// belongs to: the nearest earlier line that is indented less and ends in a block
/// scalar indicator. A less-indented line without one means `current` is not in a block.
fn enclosing_block_header(content: &str, line: usize, current: &str) -> Option<String> {
    let indent = |text: &str| text.len() - text.trim_start().len();
    let wanted = indent(current);
    let lines: Vec<&str> = content.lines().collect();
    for candidate in lines.get(..line.saturating_sub(1))?.iter().rev() {
        if candidate.trim().is_empty() {
            continue;
        }
        if indent(candidate) < wanted {
            return opens_block_scalar(candidate).then(|| candidate.to_string());
        }
    }
    None
}

/// Pass every added file name and added line of `git log --patch --cc -U0` output to `scan`.
fn scan_patch(
    text: &str,
    scan: ContentScan<'_>,
    file_at: &dyn Fn(&str, &str) -> Option<String>,
) -> Result<()> {
    let mut commit = String::new();
    let mut recovered = false;
    let (mut path, mut added, mut hunk, mut parents, mut line) = (None::<String>, false, false, 1, 0);
    let mut block_scalar_header: Option<String> = None;
    let mut previous_added: Option<String> = None;
    for row in text.lines() {
        if let Some(id) = row.strip_prefix('\0') {
            commit = id.trim().to_string();
        }
        if row.starts_with('\0') || row.starts_with("diff --") {
            parents = if row.starts_with("diff --cc ") || row.starts_with("diff --combined ") {
                2
            } else {
                1
            };
            (path, added, hunk) = (None, false, false);
            block_scalar_header = None;
            continue;
        }
        if row.starts_with("@@") {
            let marks = row.bytes().take_while(|byte| *byte == b'@').count();
            parents = marks.saturating_sub(1).max(1);
            line = row[marks..]
                .split_ascii_whitespace()
                .find_map(|range| range.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|start| start.parse().ok())
                .ok_or("publishing refused: cannot scan the commits to publish")?;
            hunk = true;
            recovered = false;
            block_scalar_header = None;
            continue;
        }
        if !hunk {
            if row.starts_with("new file mode ") {
                added = true;
            } else if let Some(header) = row.strip_prefix("+++ ") {
                previous_added = None;
                path = diff_path(header);
                if let Some(name) = path.as_deref().filter(|_| added) {
                    let item = PushedContent {
                        path: name,
                        line: None,
                        text: "",
                    };
                    if let Some(finding) = scan(&item) {
                        return Err(refusal(name, None, finding));
                    }
                }
            }
            continue;
        }
        let Some(name) = path.as_deref() else {
            continue;
        };
        let Some((marks, rest)) = row.split_at_checked(parents) else {
            continue;
        };
        if marks.contains('-') || row.starts_with('\\') {
            block_scalar_header = None;
            previous_added = None;
            continue;
        }
        if marks.contains('+') {
            // `--text` passes UTF-16/UTF-32 files through as NUL-interleaved bytes, in
            // which no pattern matches; the line is also checked with its NULs removed,
            // which is the ASCII text those encodings carry.
            if !recovered {
                recovered = true;
                if !added && block_scalar_header.is_none() && rest.starts_with([' ', '\t']) {
                    block_scalar_header = file_at(&commit, name)
                        .and_then(|content| enclosing_block_header(&content, line, rest));
                }
            }
            let stripped = rest.replace('\0', "");
            let decoded = (stripped.len() != rest.len()).then_some(stripped.as_str());
            for text in std::iter::once(rest).chain(decoded) {
                let item = PushedContent {
                    path: name,
                    line: Some(line),
                    text,
                };
                if let Some(finding) = scan(&item) {
                    return Err(refusal(name, Some(line), finding));
                }
                if let Some(header) = block_scalar_header.as_deref() {
                    let context = format!("{header}\n{text}");
                    let item = PushedContent {
                        path: name,
                        line: Some(line),
                        text: &context,
                    };
                    if let Some(finding) = scan(&item) {
                        return Err(refusal(name, Some(line), finding));
                    }
                }
            }
            // A credential split over two added lines (`"password":` then the value) matches
            // neither line alone, so the pair is scanned with its context.
            if let Some(previous) = previous_added.as_deref() {
                let context = format!("{previous}\n{rest}");
                let item = PushedContent {
                    path: name,
                    line: Some(line),
                    text: &context,
                };
                if let Some(finding) = scan(&item) {
                    return Err(refusal(name, Some(line), finding));
                }
            }
            previous_added = Some(rest.to_string());
            // A line at or above the header's indentation leaves the block scalar: a later
            // key must not be scanned against this header.
            let indent = |text: &str| text.len() - text.trim_start().len();
            if block_scalar_header
                .as_deref()
                .is_some_and(|header| indent(rest) <= indent(header))
            {
                block_scalar_header = None;
            }
            if opens_block_scalar(rest) {
                block_scalar_header = Some(rest.to_string());
            }
        } else {
            block_scalar_header = None;
            previous_added = None;
        }
        line += 1;
    }
    Ok(())
}

fn isolate_github_environment(command: &mut Command) {
    // The destination comes from origin. Let the CLI choose its stored
    // login for that host, independently of host-agnostic shell overrides.
    for key in [
        "GH_HOST",
        "GH_REPO",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        command.env_remove(key);
    }
}

fn gh(
    binary: &Path,
    policy: &RunPolicy,
    token: Option<&str>,
    repo: &Path,
    lock: &File,
    args: &[&str],
) -> Result<String> {
    let mut command = Command::new(binary);
    super::isolate_git_environment(&mut command);
    isolate_github_environment(&mut command);
    if let Some(token) = token {
        command.env("GH_TOKEN", token);
    }
    command
        .env("PATH", &policy.host_path)
        .env("GH_PROMPT_DISABLED", "1")
        .current_dir(repo)
        .args(args);
    match super::command_runner_with_stdin(
        command,
        Some(MAX_PUBLICATION_RESPONSE_BYTES),
        Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
        super::COMMAND_DEADLINE,
    ) {
        Some((0, text)) => Ok(text.trim().into()),
        _ => Err("GitHub publication request failed".into()),
    }
}
fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}
struct Repository {
    qualified: String,
    host: String,
    owner: String,
    name: String,
}
fn repository(value: &serde_json::Value) -> Result<Repository> {
    let identity = value["nameWithOwner"]
        .as_str()
        .ok_or("repository identity missing")?;
    let (owner, name) = identity.split_once('/').ok_or("invalid repository identity")?;
    let url = value["url"].as_str().ok_or("repository URL missing")?;
    let (host, path) = url
        .strip_prefix("https://")
        .and_then(|url| url.split_once('/'))
        .ok_or("repository URL must identify its HTTPS host")?;
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || path.trim_end_matches('/') != identity
        || host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
    {
        return Err("repository URL disagrees with its identity".into());
    }
    Ok(Repository {
        qualified: format!("{host}/{identity}"),
        host: host.into(),
        owner: owner.into(),
        name: name.into(),
    })
}
const BODY_BEGIN: &str = "<!-- oxa-work-acceptance -->";
const BODY_END: &str = "<!-- /oxa-work-acceptance -->";
fn body(task: &Task, head: &str, existing: Option<&str>) -> Result<String> {
    let section = format!("{BODY_BEGIN}\nAccepted commit: `{head}`\nLocal acceptance: passed\nCI: not yet qualified for this commit\n{BODY_END}");
    let default = format!("T2 — task `{}` executed by `oxa work`.\n\nWhat/why: explicitly requested task; review its changes against the base.\n\nChanges: accepted worktree retained.\n\nRemoves: inspect the diff before promotion; no deletion clearance is inferred.\n\nValidation: frozen local acceptance passed; CI qualification is separate.\n", task.id);
    let original = existing.unwrap_or(&default);
    match (
        original.matches(BODY_BEGIN).count(),
        original.matches(BODY_END).count(),
    ) {
        (0, 0) => Ok(format!("{original}\n\n{section}\n")),
        (1, 1) => {
            let (before, rest) = original
                .split_once(BODY_BEGIN)
                .ok_or("invalid managed PR section")?;
            let (_, after) = rest.split_once(BODY_END).ok_or("invalid managed PR section")?;
            Ok(format!("{before}{section}{after}"))
        }
        _ => Err("ambiguous managed PR section; preserve the owner description".into()),
    }
}
fn pr_base(repo: &Path, task: &Task, policy: &RunPolicy) -> Result<String> {
    let reference = checked(super::git(
        policy,
        repo,
        &["rev-parse", "--symbolic-full-name", &task.base],
    ))?;
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "publishing requires a named local or origin base branch".into())
}
pub fn publish(
    repo: &Path,
    binary: &Path,
    task: &Task,
    policy: &RunPolicy,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    scan: ContentScan<'_>,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    LockedTask::open(task, policy, false)?.publish(repo, binary, gh_binary, observer, scan, confirm)
}

fn publish_locked(
    repo: &Path,
    binary: &Path,
    locked: &LockedTask<'_>,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    scan: ContentScan<'_>,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    let (mut writer, mut value) = action_context(locked, observer)?;
    let (task, policy, lock) = (locked.task, locked.policy, &locked.lock);
    let accepted = value.record.acceptance_outcome.unwrap_or(value.record.outcome);
    if !super::completed_execution(value.record.step)
        || !matches!(accepted, Outcome::Done | Outcome::NothingToDo)
    {
        return Ok(value.record);
    }
    value.record.acceptance_outcome = Some(accepted);
    writer.save(&mut value, Step::Publishing, observer)?;
    let git = |repo: &Path, args: &[&str]| super::git(policy, repo, args);
    let result = (|| -> Result<(String, bool)> {
        if policy.offline {
            return Err("publishing is unavailable in offline mode".into());
        }
        // The owner's token source, read once for this publication.
        let token = super::policy_token(policy, repo)?;
        let gh = |repo: &Path, lock: &File, args: &[&str]| {
            gh(gh_binary, policy, token.as_deref(), repo, lock, args)
        };
        let tree = &value.record.worktree;
        tree::validate(policy, repo, tree, task)?;
        if super::dirty_worktree(binary, tree, repo, policy, lock)? {
            return Err("uncommitted work must be saved before publishing".into());
        }
        let head = checked(git(tree, &["rev-parse", "HEAD"]))?;
        if value.record.head_sha.as_deref() != Some(head.as_str()) {
            return Err("task branch changed after acceptance; use --rerun before publishing".into());
        }
        let proof = super::committed::verify_commit(repo, binary, policy, &value, lock)?;
        let accepted_base = value
            .record
            .base_sha
            .as_deref()
            .ok_or("saved base is unavailable")?;
        let base = pr_base(tree, task, policy)?;
        if task.branch == base {
            return Err("task branch must differ from its base branch".into());
        }
        // The owner confirmation shows only metadata, so content is checked here,
        // before any remote request, against what the local origin refs record.
        // The destination's own tips are checked again after confirmation.
        scan_pushed(policy, tree, &[head.as_str(), "--not", "--remotes=origin"], scan)?;
        let remote = checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?;
        if remote.lines().count() != 1 {
            return Err("publishing requires one origin push URL".into());
        }
        let repository = repository(&json(&gh(
            tree,
            lock,
            &["repo", "view", &remote, "--json", "nameWithOwner,url"],
        )?)?)?;
        let name = repository.qualified.as_str();
        let owner = repository.owner.as_str();
        let repo_name = repository.name.as_str();
        let prs = json(&gh(
            tree,
            lock,
            &[
                "pr",
                "list",
                "--repo",
                name,
                "--state",
                "open",
                "--head",
                &task.branch,
                "--json",
                "number,url,isDraft,headRepositoryOwner,isCrossRepository",
            ],
        )?)?;
        let prs = prs.as_array().ok_or("invalid PR list")?;
        if prs.len() > 1 {
            return Err("multiple open PRs match the task branch".into());
        }
        if prs.first().is_some_and(|pr| {
            pr["headRepositoryOwner"]["login"].as_str() != Some(owner)
                || pr["isCrossRepository"].as_bool() != Some(false)
        }) {
            return Err("PR head repository does not match origin".into());
        }
        if accepted == Outcome::NothingToDo
            && prs.is_empty()
            && value.record.head_sha == value.record.base_sha
        {
            return Err("nothing to publish".into());
        }
        let existing_body = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            let value = json(&gh(
                tree,
                lock,
                &["pr", "view", &number, "--repo", name, "--json", "body"],
            )?)?;
            Some(value["body"].as_str().ok_or("PR body missing")?.to_owned())
        } else {
            None
        };
        let text = body(task, &head, existing_body.as_deref())?;
        if confirm(&format!(
            "Push accepted commit {head} as {} to origin and create/update its PR in {name} (base {base})? Type yes to publish: ",
            task.branch
        )) != Some(true) {
            return Err("publication requires confirmation through the controlling terminal".into());
        }
        if remote != checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?
            || head != checked(git(tree, &["rev-parse", "HEAD"]))?
        {
            return Err("publication destination or task head changed during confirmation".into());
        }
        proof.verify(policy, &head, accepted_base)?;
        // Every commit the destination lacks is pushed, so the exclusions come from
        // the push URL's own advertised tips rather than from local refs.
        let known = destination_tips(policy, repo, &remote)?;
        scan_pushed(policy, tree, &pushed_revisions(&head, &known), scan)?;
        // Confirmation may remain open indefinitely: queue admission is checked
        // only after it returns, immediately before the push.
        if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){mergeQueueEntry{id}}}}";
            let data = json(&gh(
                tree,
                lock,
                &[
                    "api",
                    "graphql",
                    "--hostname",
                    &repository.host,
                    "-f",
                    &format!("query={query}"),
                    "-F",
                    &format!("owner={owner}"),
                    "-F",
                    &format!("name={repo_name}"),
                    "-F",
                    &format!("number={number}"),
                ],
            )?)?;
            let pull = &data["data"]["repository"]["pullRequest"];
            if !pull.is_object() || pull.get("mergeQueueEntry").is_none() {
                return Err("cannot determine whether PR is in the merge queue".into());
            }
            if !pull["mergeQueueEntry"].is_null() {
                return Err("PR is in the merge queue; use a new task branch for follow-up work".into());
            }
        }
        let mut push = git(
            repo,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "protocol.ssh.allow=always",
                "-c",
                "protocol.file.allow=always",
                // Only the scanned commit is sent: no implicit tags or submodule pushes.
                "-c",
                "push.followTags=false",
                "-c",
                "push.recurseSubmodules=no",
                "push",
                "--no-follow-tags",
                "--recurse-submodules=no",
                "origin",
                &format!("{head}:refs/heads/{}", task.branch),
            ],
        );
        isolate_github_environment(&mut push);
        if let Some(token) = &token {
            push.env("GH_TOKEN", token);
        }
        push.env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            // Metadata probes deny every transport. Only this explicit,
            // confirmed publication may use the normal GitHub/file transports.
            .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
        if !matches!(
            super::command_runner_with_stdin(
                push,
                Some(MAX_PUBLICATION_RESPONSE_BYTES),
                Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
                super::COMMAND_DEADLINE,
            ),
            Some((0, _))
        ) {
            return Err("accepted commit push failed".into());
        }
        let draft = false;
        let body = policy.storage.join(format!("{}.pr-body.md", task.id));
        crate::checkpoint::write_atomic(&body, text.as_bytes(), body.exists())?;
        let body_arg = body.to_str().ok_or("non-UTF8 PR body path")?;
        let result = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            gh(
                tree,
                lock,
                &[
                    "pr",
                    "edit",
                    &number,
                    "--repo",
                    name,
                    "--base",
                    &base,
                    "--body-file",
                    body_arg,
                ],
            )?;
            let was_draft = pr["isDraft"].as_bool().ok_or("PR draft state missing")?;
            if was_draft != draft {
                let mut args = vec!["pr", "ready", &number, "--repo", name];
                if draft {
                    args.push("--undo");
                }
                gh(tree, lock, &args)?;
            }
            pr["url"].as_str().ok_or("PR URL missing")?.to_string()
        } else {
            let title = format!("oxa work: {}", task.id);
            let mut args = vec![
                "pr",
                "create",
                "--repo",
                name,
                "--head",
                &task.branch,
                "--base",
                &base,
                "--title",
                &title,
                "--body-file",
                body_arg,
            ];
            if draft {
                args.push("--draft");
            }
            gh(tree, lock, &args)?
        };
        let _ = std::fs::remove_file(body);
        Ok((result.trim().to_string(), draft))
    })();
    match result {
        Ok((url, draft)) => {
            value.record.pull_request = Some(url);
            value.record.draft = draft;
            value.record.outcome = accepted;
            value.record.detail.clear();
            writer.save(&mut value, Step::Finished, observer)?;
        }
        Err(error) => {
            value.record.outcome = Outcome::Blocked;
            value.record.detail = format!("publishing: {error}");
            writer.save(&mut value, Step::Publishing, observer)?;
        }
    }
    Ok(value.record)
}
/// Keep a report's per-user lease held if the controller dies during a host command.
pub fn inherit_report_lease(command: &mut Command, gate: &File) -> std::io::Result<()> {
    let gate = gate.try_clone()?;
    // SAFETY: the closure owns the descriptor, and only changes its flags
    // in this child. fcntl is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(gate.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

pub fn report_notes(
    task: &Task,
    policy: &RunPolicy,
    observer: &dyn CheckpointObserver,
    reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    LockedTask::open(task, policy, false)?.report_notes(observer, reporter)
}

fn report_locked(
    locked: &LockedTask<'_>,
    observer: &dyn CheckpointObserver,
    reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    let (mut writer, mut value) = action_context(locked, observer)?;
    let (policy, lock) = (locked.policy, &locked.lock);
    let prior_step = value.record.step;
    // Reporting is ancillary: a failed save/publication remains recoverable even
    // if reporting is interrupted between durable report receipts.
    let reporting_step = match prior_step {
        Step::Saving | Step::Publishing => prior_step,
        _ => Step::Reporting,
    };
    for index in 0..value.record.problem_notes.len() {
        if value
            .record
            .reports
            .get(index)
            .is_some_and(|r| !matches!(r.disposition.as_str(), "local" | "failed"))
        {
            continue;
        }
        writer.save(&mut value, reporting_step, observer)?;
        let report = reporter(
            &value.record.problem_notes[index],
            &value.record,
            lock,
            policy.offline || value.offline,
        )
        .unwrap_or_else(|detail| super::ReportRecord {
            disposition: "failed".into(),
            draft: PathBuf::new(),
            url: None,
            detail,
        });
        if index < value.record.reports.len() {
            value.record.reports[index] = report;
        } else {
            value.record.reports.push(report);
        }
        writer.save(&mut value, reporting_step, observer)?;
    }
    writer.save(
        &mut value,
        if matches!(prior_step, Step::Saving | Step::Publishing) {
            prior_step
        } else {
            Step::Finished
        },
        observer,
    )?;
    Ok(value.record)
}

impl LockedTask<'_> {
    pub fn publish(
        &mut self,
        repo: &Path,
        binary: &Path,
        gh_binary: &Path,
        observer: &dyn CheckpointObserver,
        scan: ContentScan<'_>,
        confirm: &mut dyn FnMut(&str) -> Option<bool>,
    ) -> Result<Record> {
        publish_locked(repo, binary, self, gh_binary, observer, scan, confirm)
    }

    pub fn report_notes(
        &mut self,
        observer: &dyn CheckpointObserver,
        reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
    ) -> Result<Record> {
        report_locked(self, observer, reporter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn policy(dir: &Path, command: Vec<String>) -> RunPolicy {
        RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin:/bin".into(),
            terminate_binary: None,
            offline: false,
            runner_owns_github: false,
            github_token_command: command,
            autonomy: Default::default(),
            autonomy_now: None,
            storage: dir.join("runner"),
        }
    }

    #[test]
    fn runner_publish_uses_the_token_source() {
        let dir = crate::test_dir::PrivateDir::new("publish-token-source");
        let repo = dir.path().join("project");
        std::fs::create_dir(&repo).unwrap();
        let fake_gh = script(dir.path(), "gh", "printf '%s' \"$GH_TOKEN\"");
        let source = script(dir.path(), "token", "echo ghs_RUNNER_TOKEN");
        let lock = File::create(dir.path().join("lock")).unwrap();
        // Without the setting the runner keeps the stored login: no token is passed on.
        let plain = policy(dir.path(), Vec::new());
        assert_eq!(super::super::policy_token(&plain, &repo), Ok(None));
        let seen = gh(&fake_gh, &plain, None, dir.path(), &lock, &[]);
        assert_eq!(seen, Ok(String::new()));
        // With it, gh gets exactly the command's token.
        let owned = policy(dir.path(), vec![source.display().to_string()]);
        let token = super::super::policy_token(&owned, &repo).unwrap();
        assert_eq!(token.as_deref(), Some("ghs_RUNNER_TOKEN"));
        let seen = gh(&fake_gh, &owned, token.as_deref(), dir.path(), &lock, &[]);
        assert_eq!(seen, Ok("ghs_RUNNER_TOKEN".into()));
        // A command that fails stops the publication instead of falling back.
        let failing = script(dir.path(), "bad", "exit 1");
        let broken = policy(dir.path(), vec![failing.display().to_string()]);
        assert!(super::super::policy_token(&broken, &repo).is_err());
    }

    #[test]
    fn token_program_requires_a_vetted_location() {
        let dir = crate::test_dir::PrivateDir::new("trusted-token-source");
        let repo = dir.path().join("project");
        let tools = dir.path().join("tools");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&tools).unwrap();
        let safe = script(&tools, "mint-token", "exit 0");
        let untrusted = script(&repo, "mint-token", "exit 0");
        let link = tools.join("shortcut");
        std::os::unix::fs::symlink(&untrusted, &link).unwrap();
        let resolve = |name| crate::trusted_token_program(name, Some(tools.as_os_str()), Some(&repo));
        assert_eq!(resolve("mint-token"), Some(safe.canonicalize().unwrap()));
        assert!(resolve("missing-tool").is_none());
        assert!(resolve("./mint-token").is_none());
        assert!(resolve(untrusted.to_str().unwrap()).is_none());
        assert!(resolve("shortcut").is_none());
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(resolve(safe.to_str().unwrap()).is_none());
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn repo() -> (crate::test_dir::PrivateDir, RunPolicy) {
        let dir = crate::test_dir::PrivateDir::new("publish-scan");
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: std::env::var_os("PATH").unwrap(),
            terminate_binary: None,
            offline: false,
            runner_owns_github: false,
            github_token_command: Vec::new(),
            autonomy: Default::default(),
            autonomy_now: None,
            storage: dir.path().join("storage"),
        };
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        (dir, policy)
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git")
            .current_dir(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn containing(needle: &'static str) -> impl Fn(&PushedContent<'_>) -> Option<ContentFinding> {
        move |item| {
            (item.path.contains(needle) || item.text.contains(needle)).then_some(ContentFinding {
                label: "a test secret",
                secret_path: item.path.contains(needle),
            })
        }
    }

    #[test]
    fn quoted_paths_are_decoded_before_the_secret_check() {
        assert_eq!(
            diff_path("\"b/a\\\"q\\\\z\\303\\251\""),
            Some("a\"q\\z\u{e9}".to_string())
        );
        let (dir, policy) = repo();
        let secret = "sy\"nth\\etic-value";
        std::fs::write(dir.path().join(format!("{secret}.txt")), "plain\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "fixture"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let error = scan_pushed(&policy, dir.path(), &[head.as_str()], &containing(secret)).unwrap_err();
        assert!(!error.contains("etic-value"), "{error}");
        assert!(!error.contains("nth"), "{error}");
    }

    #[test]
    fn a_tab_after_an_unquoted_patch_path_is_not_part_of_the_name() {
        assert_eq!(
            diff_path("b/app config/.env\t"),
            Some("app config/.env".to_string())
        );
        assert_eq!(diff_path("b/plain/.env"), Some("plain/.env".to_string()));
        let (dir, policy) = repo();
        std::fs::create_dir(dir.path().join("app config")).unwrap();
        std::fs::write(dir.path().join("app config/.env"), "PLACEHOLDER=1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        let base = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(dir.path().join("app config/.env"), "PLACEHOLDER=opaque\n").unwrap();
        git_ok(dir.path(), &["commit", "-q", "-am", "edit"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let revisions = [head.as_str(), "--not", base.as_str()];
        let error = scan_pushed(&policy, dir.path(), &revisions, &|item| {
            (item.line.is_some() && item.path.ends_with("/.env")).then_some(ContentFinding {
                label: "content to a file name used for secrets",
                secret_path: false,
            })
        })
        .unwrap_err();
        assert!(error.contains("app config/.env:1"), "{error}");
    }

    #[test]
    fn the_root_commit_is_scanned_whatever_log_show_root_says() {
        let (dir, policy) = repo();
        git_ok(dir.path(), &["config", "log.showRoot", "false"]);
        std::fs::write(dir.path().join("root.txt"), "synthetic-root-secret\n").unwrap();
        git_ok(dir.path(), &["add", "root.txt"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "root"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[head.as_str()],
            &containing("synthetic-root-secret"),
        )
        .unwrap_err();
        assert!(error.contains("root.txt:1"), "{error}");
    }

    #[test]
    fn utf16_additions_are_scanned_as_decoded_text() {
        let secret = format!("synthetic-{}-secret", "utf16");
        for (name, big_endian) in [("le.txt", false), ("be.txt", true)] {
            let (dir, policy) = repo();
            let text = format!("first line\n{secret}\nlast line\n");
            let mut bytes = if big_endian {
                vec![0xfe, 0xff]
            } else {
                vec![0xff, 0xfe]
            };
            for unit in text.encode_utf16() {
                bytes.extend(if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                });
            }
            std::fs::write(dir.path().join(name), bytes).unwrap();
            git_ok(dir.path(), &["add", name]);
            git_ok(dir.path(), &["commit", "-q", "-m", name]);
            let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
            let error = scan_pushed(
                &policy,
                dir.path(),
                &[head.as_str()],
                &containing("synthetic-utf16-secret"),
            )
            .unwrap_err();
            assert!(error.contains(name), "{error}");
            assert!(!error.contains(secret.as_str()), "{error}");
        }
    }

    fn items(text: &str) -> Vec<(String, Option<usize>, String)> {
        let seen = std::cell::RefCell::new(vec![]);
        scan_patch(
            text,
            &|item| {
                // Pair and block-scalar contexts join lines with a newline; only the lines
                // themselves are listed here.
                if !item.text.contains('\n') {
                    seen.borrow_mut()
                        .push((item.path.to_string(), item.line, item.text.to_string()));
                }
                None
            },
            &|_, _| None,
        )
        .unwrap();
        seen.into_inner()
    }

    #[test]
    fn added_names_and_lines_are_scanned_in_ordinary_and_merge_commits() {
        let patch = "\0aaaa\n\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+first\n+++ looks like a header\ndiff --git a/old.txt b/old.txt\nindex 1111111..2222222 100644\n--- a/old.txt\n+++ b/old.txt\n@@ -3 +3 @@\n-before\n+after\n\0bbbb\n\ndiff --cc merged.txt\nindex 1111111,2222222..3333333\n--- a/merged.txt\n+++ b/merged.txt\n@@@ -4,1 -4,1 +4,2 @@@\n- ours\n -theirs\n++introduced by the merge\n+ kept from the first parent\n";
        let seen = items(patch);
        let expected = [
            ("new.txt", None, ""),
            ("new.txt", Some(1), "first"),
            ("new.txt", Some(2), "++ looks like a header"),
            ("old.txt", Some(3), "after"),
            ("merged.txt", Some(4), "introduced by the merge"),
            ("merged.txt", Some(5), "kept from the first parent"),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(p, l, t)| (p.to_string(), *l, t.to_string()))
            .collect();
        assert_eq!(seen, expected);
    }

    /// A YAML header may carry chomping/indentation in either order and a trailing comment; a
    /// JSON pair may split across two added lines. Both must still reach the scanners.
    #[test]
    fn split_and_indented_credentials_are_found() {
        for header in ["|2-", "|+2", ">-2", "|2 # credential", "|", r#"|-"#] {
            let patch = format!(
                "\0aaaa\n\ndiff --git a/settings.yml b/settings.yml\nnew file mode 100644\n--- /dev/null\n+++ b/settings.yml\n@@ -0,0 +1,2 @@\n+password: {header}\n+  opaque-pass-value-17\n"
            );
            assert!(
                scan_patch(&patch, &containing("opaque-pass-value-17"), &|_, _| None).is_err(),
                "header {header:?} was not scanned as a block scalar"
            );
        }
        let split = "\0aaaa\n\ndiff --git a/settings.json b/settings.json\nnew file mode 100644\n--- /dev/null\n+++ b/settings.json\n@@ -0,0 +1,3 @@\n+{\n+  \"password\":\n+  \"opaque-pass-value-17\"\n";
        // A quoted YAML key opens a block scalar too (`"password": |-`).
        let quoted = "\0aaaa\n\ndiff --git a/settings.yml b/settings.yml\nnew file mode 100644\n--- /dev/null\n+++ b/settings.yml\n@@ -0,0 +1,2 @@\n+\"password\": |-\n+  opaque-pass-value-17\n";
        assert!(
            scan_patch(quoted, &containing("opaque-pass-value-17"), &|_, _| None).is_err(),
            "a quoted YAML key did not open the block scalar"
        );
        assert!(
            scan_patch(split, &containing("opaque-pass-value-17"), &|_, _| None).is_err(),
            "a JSON credential split over two lines was not scanned"
        );
    }

    /// A later key at the header's own indentation leaves the block scalar, so a placeheld
    /// password cannot refuse an ordinary description line below it.
    #[test]
    fn a_block_scalar_ends_at_the_next_key() {
        let patch = "\0aaaa\n\ndiff --git a/settings.yml b/settings.yml\nnew file mode 100644\n--- /dev/null\n+++ b/settings.yml\n@@ -0,0 +1,3 @@\n+password: |-\n+  ${DB_PASSWORD}\n+description: release-notes-17\n";
        let scanned = std::cell::RefCell::new(vec![]);
        scan_patch(
            patch,
            &|item| {
                scanned.borrow_mut().push(item.text.to_string());
                None
            },
            &|_, _| None,
        )
        .expect("a placeholder is not a credential");
        let texts = scanned.into_inner();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("description: release-notes-17") && !text.contains("password")),
            "the description line was still scanned against the password header: {texts:?}"
        );
    }

    #[test]
    fn a_secret_path_is_withheld_from_the_refusal() {
        let patch =
            "\0aaaa\n\ndiff --git a/hidden b/hidden\nnew file mode 100644\n--- /dev/null\n+++ b/hidden\n";
        let error = scan_patch(
            patch,
            &|_| {
                Some(ContentFinding {
                    label: "a known access token format",
                    secret_path: true,
                })
            },
            &|_, _| None,
        )
        .unwrap_err();
        assert!(!error.contains("hidden"), "{error}");
    }

    fn craft_commit(dir: &Path, header: &str) -> String {
        let raw = git_ok(dir, &["cat-file", "commit", "HEAD"]);
        let (head, message) = raw.split_once("\n\n").unwrap();
        let file = dir.join("crafted-commit");
        std::fs::write(&file, format!("{head}\n{header}\n\n{message}\n")).unwrap();
        git_ok(
            dir,
            &[
                "hash-object",
                "-t",
                "commit",
                "-w",
                "--literally",
                "crafted-commit",
            ],
        )
    }

    #[test]
    fn an_extra_commit_header_is_scanned() {
        let (dir, policy) = repo();
        git_ok(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
        let value = ["synthetic", "-header-", "value"].concat();
        let crafted = craft_commit(dir.path(), &format!("x-note {value}"));
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[crafted.as_str()],
            &containing("header-value"),
        )
        .unwrap_err();
        assert!(error.contains("commit "), "{error}");
        assert!(!error.contains("header-value"), "{error}");
    }

    #[test]
    fn a_multi_line_signature_header_is_scanned() {
        let (dir, policy) = repo();
        git_ok(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
        let value = ["synthetic", "-signature-", "value"].concat();
        let header =
            format!("gpgsig -----BEGIN PGP SIGNATURE-----\n \n {value}\n -----END PGP SIGNATURE-----");
        let crafted = craft_commit(dir.path(), &header);
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[crafted.as_str()],
            &containing("signature-value"),
        )
        .unwrap_err();
        assert!(error.contains("commit "), "{error}");
    }

    #[test]
    fn an_edited_block_scalar_recovers_its_unchanged_key() {
        let (dir, policy) = repo();
        std::fs::write(
            dir.path().join("settings.yml"),
            "name: x\ndb:\n  password: |-\n    placeholder-value\n",
        )
        .unwrap();
        git_ok(dir.path(), &["add", "."]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        let base = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(
            dir.path().join("settings.yml"),
            "name: x\ndb:\n  password: |-\n    opaque-pass-value-17\n",
        )
        .unwrap();
        git_ok(dir.path(), &["commit", "-q", "-a", "-m", "edit"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let scan = |item: &PushedContent<'_>| {
            (item.text.contains("password: |-") && item.text.contains("opaque-pass-value-17")).then_some(
                ContentFinding {
                    label: "a test secret",
                    secret_path: false,
                },
            )
        };
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[head.as_str(), "--not", base.as_str()],
            &scan,
        )
        .unwrap_err();
        assert!(error.contains("settings.yml"), "{error}");
    }

    #[test]
    fn only_full_object_ids_reach_cat_file() {
        assert!(valid_object_id(&"a".repeat(40)));
        assert!(valid_object_id(&"0".repeat(64)));
        assert!(!valid_object_id("--output=x"));
        assert!(!valid_object_id(&"A".repeat(40)));
    }
}
