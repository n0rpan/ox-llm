//! O-152: `oxa report-issue` (architect-owned), the code behind the built-in `report-issue`
//! skill. `draft` collects a scrubbed digest and saves a draft; `submit` shows the exact text
//! and posts it with `gh issue create` only after the user typed `yes` on the controlling
//! terminal, or pressed Approve in the interactive session's approval dialog (#1552, [`PublishTool`]).
//! Reporting goes to `oxstack/ox-llm` unless `oxa.reportIssue.repo` names another repository (or turns it off).
//!
//! What a draft may contain: the user's description (scrubbed, capped, no transcript records),
//! the identity facts (version, machine, OS, models). No controller is called. Never prompts, transcripts, file contents,
//! emails, home paths, user or host names, IP addresses or secrets.
mod publish_tool;
mod pushed;
mod semantic;
pub(crate) use pushed::PushScan;

pub use publish_tool::PublishTool;

use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, OxaError};
use oxa_resources::{read_identity, write_atomic};
use regex::Regex;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const USAGE: &str =
    "usage: oxa report-issue draft --title TITLE [--notes-file FILE | --notes TEXT] | oxa report-issue submit DRAFT";

/// The setting that names the target repository (`OWNER/NAME`); unset = reporting is off.
pub const SETTING_REPORT_REPO: &str = "oxa.reportIssue.repo";

/// Where oxa's own issues go when `oxa.reportIssue.repo` is unset. A value that is not
/// `OWNER/NAME` (for example `off`) turns reporting off.
pub const DEFAULT_REPORT_REPO: &str = "oxstack/ox-llm";

/// What `submit` says while reporting is turned off by the setting.
pub const REPORT_DISABLED: &str = "Issue reporting is off: the public ox-llm repository does not exist yet. \
Nothing was posted. The draft stays where it is; you can file it by hand, or set \
`oxa.reportIssue.repo` (OWNER/NAME) in ~/.oxa/agent/settings.json once the repository exists.";

/// The longest user description kept in a draft (characters).
pub const MAX_NOTES_CHARS: usize = 2000;
/// The longest controller output kept per section (characters).
pub const MAX_SECTION_CHARS: usize = 4000;

/// Runs a program: `(program, args)` -> `Some((exit code, stdout))`, `None` when it could not
/// run (missing, failed to start, timed out).
pub type Runner<'a> = &'a mut dyn FnMut(&str, &[String]) -> Option<(i32, String)>;

/// Asks the user: the question -> `Some(true)` only for an explicit `yes`, `Some(false)` for
/// anything else, `None` when there is nobody to ask (no terminal).
pub type Confirm<'a> = &'a mut dyn FnMut(&str) -> Option<bool>;

/// What the scrubber replaces besides the generic patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubCtx {
    /// The home directory (replaced by `~`).
    pub home: Option<String>,
    /// The user's login name (replaced by `<user>`).
    pub user: Option<String>,
    /// The host name (replaced by `<host>`).
    pub host: Option<String>,
}

impl ScrubCtx {
    /// From the environment (`HOME`, `USER` / `LOGNAME`) and `/etc/hostname`.
    pub fn from_env(env: &Env, dirs: &Dirs) -> ScrubCtx {
        let host = std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());
        ScrubCtx {
            home: Some(dirs.home.to_string_lossy().into_owned()),
            user: env.get("USER").or_else(|| env.get("LOGNAME")).map(str::to_string),
            host,
        }
    }
}

/// Use the existing native worker privacy policy for outbound advisor diagnostics.
pub fn advisor_scrubber(ctx: ScrubCtx) -> oxa_core::advice::AdviceScrubber {
    std::sync::Arc::new(move |text| scrub(text, &ctx))
}

struct Patterns {
    pem: Regex,
    pem_begin: Regex,
    url_cred: Regex,
    known: Regex,
    bearer: Regex,
    keyed: Regex,
    email: Regex,
    home: Regex,
    ipv4: Regex,
    tailnet: Regex,
    long: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("scrub pattern");
        Patterns {
            pem: re(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
            pem_begin: re(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----"),
            url_cred: re(r"://[^/\s:@]+:[^/\s@]+@"),
            known: re(concat!(
                r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{16,}",
                r"|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}",
                r"|glpat-[A-Za-z0-9_-]{20,}|hf_[A-Za-z0-9]{30,})"
            )),
            bearer: re(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}"),
            keyed: re(concat!(
                r#"(?i)("?\b(?:api[_-]?key|apikey|access[_-]?token|auth[_-]?token|refresh[_-]?token|token"#,
                r#"|secret|client[_-]?secret|password|passwd|passphrase|authorization|share[_-]?key|key"#,
                // O-169: the Brave Search key variable (`\b` does not split `SEARCH_API_KEY`).
                r#"|brave[_-]?search[_-]?api[_-]?key|x-subscription-token)"#,
                r#""?\s*[:=]\s*"?)([^\s"',;}]+)"#
            )),
            email: re(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
            home: re(r"/(?:var/)?home/[^/\s\x22':]+|/Users/[^/\s\x22':]+|/root\b"),
            ipv4: re(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
            tailnet: re(r"\b[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.ts\.net\b"),
            long: re(r"[A-Za-z0-9_+/=-]{32,}"),
        }
    })
}

/// Remove what must never leave the machine: private keys, credentials in URLs, known token
/// formats, `Bearer ..`, values of secret-named keys (`token=`, `"password": ..`), long
/// mixed-case random strings, emails, home paths (`~`), the user and host names, IPv4
/// addresses other than loopback and tailnet host names. Idempotent.
pub fn scrub(text: &str, ctx: &ScrubCtx) -> String {
    let p = patterns();
    let mut s = p.pem.replace_all(text, "<secret>").into_owned();
    s = p.url_cred.replace_all(&s, "://<secret>@").into_owned();
    s = p.known.replace_all(&s, "<secret>").into_owned();
    s = p.bearer.replace_all(&s, "$1 <secret>").into_owned();
    s = p
        .keyed
        .replace_all(&s, |c: &regex::Captures| {
            // Already scrubbed, or an auth scheme whose credential the `bearer` rule took.
            let v = &c[2];
            if v == "<secret>" || v.eq_ignore_ascii_case("bearer") || v.eq_ignore_ascii_case("basic") {
                c[0].to_string()
            } else {
                format!("{}<secret>", &c[1])
            }
        })
        .into_owned();
    s = p
        .long
        .replace_all(&s, |c: &regex::Captures| {
            let t = &c[0];
            let mixed = t.chars().any(|ch| ch.is_ascii_uppercase())
                && t.chars().any(|ch| ch.is_ascii_lowercase())
                && t.chars().any(|ch| ch.is_ascii_digit());
            if mixed {
                "<secret>".to_string()
            } else {
                t.to_string()
            }
        })
        .into_owned();
    s = p.email.replace_all(&s, "<email>").into_owned();
    if let Some(home) = ctx.home.as_deref().filter(|h| h.len() > 1) {
        s = s.replace(home.trim_end_matches('/'), "~");
    }
    s = p.home.replace_all(&s, "~").into_owned();
    s = p.tailnet.replace_all(&s, "<host>").into_owned();
    s = p
        .ipv4
        .replace_all(&s, |c: &regex::Captures| {
            let ip = &c[0];
            if ip.starts_with("127.") || ip == "0.0.0.0" {
                ip.to_string()
            } else {
                "<ip>".to_string()
            }
        })
        .into_owned();
    for (name, mark) in [(&ctx.host, "<host>"), (&ctx.user, "<user>")] {
        // A name that is part of a marker (`<user>`, `<secret>`, ..) would break idempotence.
        let marker_word = |n: &str| ["user", "host", "secret", "email"].contains(&n.to_lowercase().as_str());
        if let Some(n) = name.as_deref().filter(|n| n.len() >= 3 && !marker_word(n)) {
            let word = Regex::new(&format!(r"(?i)\b{}\b", regex::escape(n))).expect("name pattern");
            s = word.replace_all(&s, mark).into_owned();
        }
    }
    s
}

/// The user's description as it may enter a draft: lines that look like transcript or
/// session records dropped, capped at [`MAX_NOTES_CHARS`], scrubbed.
pub fn clean_notes(notes: &str, ctx: &ScrubCtx) -> String {
    let record =
        Regex::new(r#""(?:role|content|messages|toolCallId|parentId)"\s*:"#).expect("record pattern");
    let kept: Vec<&str> = notes.lines().filter(|l| !record.is_match(l)).collect();
    let mut text = kept.join("\n").trim().to_string();
    if text.chars().count() > MAX_NOTES_CHARS {
        text = text.chars().take(MAX_NOTES_CHARS).collect::<String>() + " [cut]";
    }
    scrub(&text, ctx)
}

fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_SECTION_CHARS {
        text.to_string()
    } else {
        text.chars().take(MAX_SECTION_CHARS).collect::<String>() + "\n[cut]"
    }
}

/// The body of an issue: the description and the identity facts. Every part is scrubbed.
pub fn draft_body(notes: &str, dirs: &Dirs, ctx: &ScrubCtx, _runner: Runner<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let notes = clean_notes(notes, ctx);
    parts.push(format!("## What happened\n\n{}", notes));
    let mut system: Vec<String> = vec![format!("- oxa version: {}", env!("CARGO_PKG_VERSION"))];
    if let Some(identity) = read_identity(dirs) {
        system.extend(
            identity
                .lines()
                .filter(|l| (l.starts_with("- ") || l.starts_with("  - ")) && !l.starts_with("- oxa version"))
                .map(str::to_string),
        );
    } else {
        system.push("- identity: not installed".to_string());
    }
    parts.push(format!("## System\n\n{}", scrub(&system.join("\n"), ctx)));
    parts.push(
        "Drafted by `oxa report-issue`: scrubbed of emails, home paths, user and host names, IP addresses and secrets; no prompts or transcripts."
            .to_string(),
    );
    parts.join("\n\n")
}

/// The draft file: `Title: <title>`, a blank line, the body.
fn draft_text(title: &str, body: &str) -> String {
    format!("Title: {title}\n\n{body}\n")
}

/// `(title, body)` of a draft file's text.
pub fn parse_draft(text: &str) -> Option<(String, String)> {
    let (first, rest) = text.split_once('\n')?;
    let title = first.strip_prefix("Title: ")?.trim().to_string();
    let body = rest.trim_matches('\n').to_string();
    (!title.is_empty() && !body.is_empty()).then_some((title, body))
}

/// Save a draft under `<agent dir>/issue-drafts/` (named by its content hash). Returns the
/// path and the exact `(title, body)` saved.
pub fn save_draft(
    title: &str,
    notes: &str,
    dirs: &Dirs,
    ctx: &ScrubCtx,
    runner: Runner<'_>,
) -> Result<(PathBuf, String, String), OxaError> {
    let title = scrub(title.trim(), ctx).replace('\n', " ");
    if title.is_empty() {
        return Err(OxaError::new(codes::USAGE, "a title is required").with_hint(USAGE));
    }
    if clean_notes(notes, ctx).is_empty() {
        return Err(OxaError::new(
            codes::USAGE,
            "no description: the notes are empty, so there is nothing to report; write what happened and draft again",
        )
        .with_hint(USAGE));
    }
    let body = draft_body(notes, dirs, ctx, runner);
    let text = draft_text(&title, &body);
    let hash = Sha256::digest(text.as_bytes());
    let name: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let path = dirs
        .agent_dir
        .join("issue-drafts")
        .join(format!("issue-{name}.md"));
    write_atomic(&path, text.as_bytes())
        .map_err(|e| OxaError::io(&format!("cannot write the draft {}", path.display()), &e))?;
    Ok((path, title, body))
}

/// [`report_repo`], or [`DEFAULT_REPORT_REPO`] when the setting is unset.
pub(crate) fn report_target(settings: &Settings) -> Option<String> {
    match settings.string(SETTING_REPORT_REPO) {
        None => Some(DEFAULT_REPORT_REPO.to_string()),
        Some(_) => report_repo(settings),
    }
}

/// `OWNER/NAME` from the settings; `None` = reporting is off.
pub fn report_repo(settings: &Settings) -> Option<String> {
    let repo = settings.string(SETTING_REPORT_REPO)?;
    let valid = Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").expect("repo pattern");
    valid.is_match(repo.trim()).then(|| repo.trim().to_string())
}

/// Post a saved draft. Order: read the draft; refuse one with text the scrubber would still
/// change; refuse while no repository is configured ([`REPORT_DISABLED`]); print the exact
/// title and body and ask `confirm` before a GitHub write. Semantic mode also
/// verifies the report against main and routes duplicates to comments. Exit codes:
/// 0 posted, 1 kept locally, 3 reporting is off.
pub fn submit(
    draft: &Path,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let storage = draft
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("report-ledger");
    submit_with_storage(draft, &storage, settings, ctx, confirm, runner, out)
}

pub(super) fn submit_with_storage(
    draft: &Path,
    storage: &Path,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    Ok(
        match submit_record(draft, storage, settings, ctx, confirm, runner, out)? {
            Submission::Created(_) | Submission::Commented(_) => 0,
            Submission::Local(_) => 1,
            Submission::Disabled => 3,
        },
    )
}

pub(crate) enum Submission {
    Created(String),
    Commented(String),
    Local(String),
    Disabled,
}

pub(crate) fn semantic_reporting(settings: &Settings) -> bool {
    // The consent is the owner's: a trusted project's `.oxa/settings.json` must not enable it.
    settings
        .user_value(semantic::SETTING)
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

pub(crate) fn report_ledger(dirs: &Dirs) -> PathBuf {
    dirs.agent_dir.join("issue-drafts/report-ledger")
}

pub(crate) use semantic::judge as judge_report;

/// The question before a dedupe comment: the exact comment and where it goes (issue and URL).
pub(crate) fn comment_question(repo: &str, issue: u64, body: &str) -> String {
    format!(
        "Comment on {repo}#{issue}?\nDestination: https://github.com/{repo}/issues/{issue}\n\n{body}\n\nType yes to post: "
    )
}

pub(crate) fn submit_record(
    draft: &Path,
    storage: &Path,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
    out: &mut dyn Write,
) -> Result<Submission, OxaError> {
    oxa_proto::check_online("posting the issue with `gh`")?; // P4
    let w = |e: std::io::Error| OxaError::io("cannot write to the terminal", &e);
    let text = std::fs::read_to_string(draft)
        .map_err(|e| OxaError::io(&format!("cannot read the draft {}", draft.display()), &e))?;
    let (title, body) = parse_draft(&text).ok_or_else(|| {
        OxaError::new(
            codes::INVALID,
            "not an oxa issue draft (expected `Title: ..`, a blank line, the body)",
        )
    })?;
    let control = |text: &str| {
        text.chars().any(|ch| (ch.is_control() && !matches!(ch, '\n' | '\t'))
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
    };
    if scrub(&title, ctx) != title || scrub(&body, ctx) != body || control(&title) || control(&body) {
        writeln!(
            out,
            "Not posted: the draft contains text the scrubber removes (an email, a home path, a name or a secret). Run `oxa report-issue draft` again."
        )
        .map_err(w)?;
        return Ok(Submission::Local("draft needs scrubbing".into()));
    }
    let Some(repo) = report_target(settings) else {
        writeln!(out, "{REPORT_DISABLED}\nDraft: {}", draft.display()).map_err(w)?;
        return Ok(Submission::Disabled);
    };
    let mut publication = if semantic_reporting(settings) {
        writeln!(out, "Repository: {repo}\nTitle: {title}\n\n{body}\n").map_err(w)?;
        let daily_limit = semantic::daily_new(settings);
        match semantic::plan(storage, &repo, (&title, &body), ctx, daily_limit, confirm, runner) {
            Ok(publication) => Some(publication),
            Err(reason) => {
                writeln!(out, "Kept locally at {} ({reason}).", draft.display()).map_err(w)?;
                return Ok(Submission::Local(reason.into()));
            }
        }
    } else {
        writeln!(out, "Repository: {repo}\nTitle: {title}\n\n{body}\n").map_err(w)?;
        // The broker displays and records this string, so keep the exact publication in the
        // confirmation itself. The caller's stdout may be redirected or hidden from the owner.
        match confirm(&format!(
            "Post this issue publicly to {repo}?\n\nTitle: {title}\n\n{body}\n\nType yes to post: "
        )) {
            Some(true) => {}
            Some(false) => {
                writeln!(out, "Not posted.").map_err(w)?;
                return Ok(Submission::Local("publication was declined".into()));
            }
            None => {
                writeln!(
                    out,
                    "Not posted: confirmation needs a terminal. Run `oxa report-issue submit {}` yourself in a terminal.",
                    draft.display()
                )
                .map_err(w)?;
                return Ok(Submission::Local(
                    "publication requires confirmation through the controlling terminal".into(),
                ));
            }
        }
        None
    };
    let body = publication
        .as_ref()
        .map_or(body.as_str(), |plan| plan.body.as_str());
    let issue = publication.as_ref().and_then(|plan| plan.issue);
    let number;
    let mut args = if let Some(issue) = issue {
        if confirm(&comment_question(&repo, issue, body)) != Some(true) {
            writeln!(out, "Comment retained locally at {}.", draft.display()).map_err(w)?;
            return Ok(Submission::Local("comment requires confirmation".into()));
        }
        number = issue.to_string();
        vec!["issue", "comment", &number, "--repo", &repo]
    } else {
        vec!["issue", "create", "--repo", &repo, "--title", &title]
    };
    // Hand `gh` exactly the bytes that were displayed and approved. The draft lives in a
    // model-writable directory, so its pathname could be swapped between the owner's yes and
    // `gh` opening it; the body as an argument fails past the kernel's argument size. The
    // approved bytes go into a staged descriptor nothing else can name or change.
    let staged =
        ApprovedBody::stage(body).map_err(|e| OxaError::io("cannot stage the approved issue body", &e))?;
    let body_path = staged.path();
    args.extend(["--body-file", &body_path]);
    let args: Vec<String> = args.into_iter().map(str::to_string).collect();
    let result = runner("gh", &args);
    drop(staged);
    match result {
        Some((0, stdout)) => {
            if let Some(plan) = &mut publication {
                if plan.record_created(&repo, &stdout).is_err() {
                    writeln!(out, "Posted, but the local issue receipt could not be saved.").map_err(w)?;
                }
            }
            writeln!(out, "Posted: {}", stdout.trim()).map_err(w)?;
            let url = stdout.trim().to_owned();
            Ok(if issue.is_some() {
                Submission::Commented(url)
            } else {
                Submission::Created(url)
            })
        }
        Some((code, _)) => {
            writeln!(
                out,
                "GitHub did not confirm the write (exit {code}). Draft retained at {}.",
                draft.display()
            )
            .map_err(w)?;
            Ok(Submission::Local(format!(
                "GitHub did not confirm the write (exit {code})"
            )))
        }
        None => {
            writeln!(out, "Not posted: the GitHub CLI `gh` could not run.").map_err(w)?;
            Ok(Submission::Local("GitHub CLI could not run".into()))
        }
    }
}

/// The approved issue body, staged for `gh --body-file /dev/fd/N`. The descriptor has no
/// close-on-exec flag, so the `gh` child inherits it; it is closed when this value drops.
///
/// On Linux it is a memfd: it has no pathname a model-run process could replace, and it is
/// sealed against writes, growth and shrinking before `gh` starts, so nobody (not even a
/// process that reopens it through `/proc`) can change the bytes after the owner's yes. Other
/// platforms have no OS sandbox (commands run with the owner's full rights); there it is a
/// private file unlinked as soon as it is created.
pub(crate) struct ApprovedBody {
    file: std::fs::File,
}

impl ApprovedBody {
    pub(crate) fn stage(body: &str) -> std::io::Result<Self> {
        Ok(Self {
            file: stage_bytes(body.as_bytes())?,
        })
    }

    /// The path `gh` opens: the inherited descriptor, never a file in the draft directory.
    pub(crate) fn path(&self) -> String {
        use std::os::fd::AsRawFd;
        format!("/dev/fd/{}", self.file.as_raw_fd())
    }
}

#[cfg(target_os = "linux")]
fn stage_bytes(bytes: &[u8]) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // No MFD_CLOEXEC: the `gh` child inherits the descriptor.
    // SAFETY: a NUL-terminated name and valid flags; the returned descriptor is owned below.
    let fd = unsafe { libc::memfd_create(c"oxa-approved-body".as_ptr(), libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor that nothing else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: fcntl on a descriptor this function owns.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(not(target_os = "linux"))]
fn stage_bytes(bytes: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::{Seek, SeekFrom};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("oxa-approved-body-{}-{nanos}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    // SAFETY: fcntl on a descriptor this function owns; clears close-on-exec for `gh`.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// Check reachability for agent-launched work without prompting or accepting any answer.
pub(crate) fn has_session_broker(env: &Env) -> bool {
    env.get(oxa_core::owner_confirm::SOCK_ENV)
        .is_some_and(|sock| connect_verified(Path::new(sock)).is_some())
}

/// Ask the authenticated session broker when exported, otherwise ask a direct owner on
/// `/dev/tty`. Agent markers and piped input never supply an answer.
pub(crate) fn tty_confirm(env: &Env) -> impl FnMut(&str) -> Option<bool> + '_ {
    tty_confirm_with(env, |sock, question| {
        // Authenticate and ask on the same connection; never fall back when a broker was exported.
        oxa_core::owner_confirm::ask_on(connect_verified(sock)?, question)
    })
}

fn connect_verified(sock: &Path) -> Option<std::os::unix::net::UnixStream> {
    let stream = std::os::unix::net::UnixStream::connect(sock).ok()?;
    peer_is_session_oxa(&stream, sock).then_some(stream)
}

/// A sandbox can use only the pinned, same-user host peer. Elsewhere require a same-user
/// peer and an unambiguous oxa ancestor; a same-user listener alone is not authority.
#[cfg(target_os = "linux")]
fn peer_is_session_oxa(stream: &std::os::unix::net::UnixStream, sock: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt on a socket this function owns, into a ucred of the stated length.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    // SAFETY: geteuid has no preconditions.
    if rc != 0 || len as usize != std::mem::size_of::<libc::ucred>() || cred.uid != unsafe { libc::geteuid() }
    {
        return false;
    }
    if sock == Path::new(oxa_sandbox::OWNER_CONFIRM_SOCK) {
        // bwrap pins this socket and denies nested user namespaces. A visible peer
        // cannot be the session outside the sandbox PID namespace.
        return cred.pid == 0;
    }
    if !std::fs::read_to_string("/proc/self/uid_map")
        .is_ok_and(|map| map.split_whitespace().eq(["0", "0", "4294967295"]))
    {
        return false;
    }
    let Ok(peer) = u32::try_from(cred.pid) else {
        return false;
    };
    use std::os::unix::fs::MetadataExt;
    verify_ancestor(peer, |pid| {
        let exe = std::fs::metadata(format!("/proc/{pid}/exe")).ok()?;
        Some((exe.dev(), exe.ino()))
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_ancestor<E: PartialEq>(peer: u32, exe_of: impl Fn(u32) -> Option<E>) -> bool {
    is_session_broker(peer, std::process::id(), parent_pid, exe_of)
}

#[cfg(target_os = "macos")]
fn peer_is_session_oxa(stream: &std::os::unix::net::UnixStream, _sock: &Path) -> bool {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let fd = stream.as_raw_fd();
    let (mut uid, mut gid, mut pid) = (0, 0, 0 as libc::pid_t);
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: fd is live; each output pointer is valid and has the required size.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0
        || uid != unsafe { libc::geteuid() }
        || unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        } != 0
        || len as usize != std::mem::size_of::<libc::pid_t>()
        || pid <= 0
    {
        return false;
    }
    verify_ancestor(pid as u32, |pid| {
        let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: buf is writable for the byte count passed to proc_pidpath.
        let size =
            unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
        let end = buf.iter().position(|&byte| byte == 0)?;
        (size > 0 && end > 0).then(|| PathBuf::from(OsStr::from_bytes(&buf[..end])))
    })
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: info provides a suitably aligned writable buffer of size bytes. Read it only
    // after the kernel reports a complete PROC_PIDTBSDINFO result.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    (read == size).then(|| unsafe { info.assume_init() }.pbi_ppid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_is_session_oxa(_stream: &std::os::unix::net::UnixStream, _sock: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`: comm may hold spaces and parentheses, so cut after the last `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The peer must be the only ancestor running the consumer's executable. If multiple
/// nested oxa processes are present, an exported path alone cannot identify the owner.
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn is_session_broker<E: PartialEq>(
    peer: u32,
    mut pid: u32,
    parent_pid: impl Fn(u32) -> Option<u32>,
    exe_of: impl Fn(u32) -> Option<E>,
) -> bool {
    let Some(own) = exe_of(pid) else { return false };
    let mut found = false;
    for _ in 0..64 {
        let Some(parent) = parent_pid(pid) else {
            return false;
        };
        if parent <= 1 {
            return found;
        }
        if exe_of(parent).is_some_and(|exe| exe == own) {
            if found || parent != peer {
                return false;
            }
            found = true;
        }
        pid = parent;
    }
    false
}

fn tty_confirm_with<'a>(
    env: &'a Env,
    ask_verified: impl Fn(&Path, &str) -> Option<bool> + 'a,
) -> impl FnMut(&str) -> Option<bool> + 'a {
    move |question: &str| {
        if let Some(sock) = env.get(oxa_core::owner_confirm::SOCK_ENV) {
            return ask_verified(Path::new(sock), question);
        }
        if crate::work_cmd::agent_launched(env) {
            return None;
        }
        // Keep concurrent workers' questions and answers together.
        static TERMINAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _terminal = TERMINAL.lock().ok()?;
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        tty.write_all(question.as_bytes()).ok()?;
        tty.flush().ok()?;
        let mut line = String::new();
        std::io::BufReader::new(tty).read_line(&mut line).ok()?;
        Some(line.trim().eq_ignore_ascii_case("yes"))
    }
}

#[cfg(test)]
mod confirm_tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A one-shot fake broker that answers `answer` and reports the question it saw.
    fn broker(name: &str, answer: &'static str) -> (std::path::PathBuf, std::thread::JoinHandle<String>) {
        let sock = std::env::temp_dir().join(format!("oxa-s2-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind");
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .expect("read");
            writeln!(&stream, "{{\"answer\": \"{answer}\", \"source\": \"typed\"}}").expect("write");
            line
        });
        (sock, handle)
    }

    #[test]
    fn confirm_uses_broker_under_agent_marker() {
        let env = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            ("OXA_CONFIRM_SOCK", oxa_sandbox::OWNER_CONFIRM_SOCK),
        ]);
        let mut confirm = tty_confirm_with(&env, |sock, question| {
            assert_eq!(sock, Path::new(oxa_sandbox::OWNER_CONFIRM_SOCK));
            assert_eq!(question, "Post this issue? ");
            Some(true)
        });
        assert_eq!(confirm("Post this issue? "), Some(true));

        // The same process may override its environment (and drop the agent marker), but an
        // arbitrary listener is not the session's broker and must never count as owner approval:
        // a fake listener that answers yes is rejected with or without the marker.
        for marker in [true, false] {
            let (sock, _seen) = broker("fake", "yes");
            let path = sock.to_string_lossy().into_owned();
            let mut pairs = vec![("OXA_CONFIRM_SOCK", path.as_str())];
            if marker {
                pairs.push(("OXA_CODING_AGENT", "true"));
            }
            let env = Env::from_pairs(&pairs);
            assert_eq!(tty_confirm(&env)("q"), None, "marker {marker}");
        }
        let env = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            ("OXA_CONFIRM_SOCK", "/workspace/fake-confirm.sock"),
        ]);
        assert_eq!(tty_confirm(&env)("q"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pinned_path_does_not_authorize_a_visible_peer() {
        let (client, _visible_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!peer_is_session_oxa(
            &client,
            Path::new(oxa_sandbox::OWNER_CONFIRM_SOCK)
        ));
    }

    #[test]
    fn broker_is_trusted_only_as_the_unique_ancestor_running_the_same_executable() {
        let parent = |pid| match pid {
            30 => Some(20),
            20 => Some(10),
            10 => Some(1),
            _ => None,
        };
        let exe = |pid| Some(matches!(pid, 30 | 10 | 99));
        assert!(is_session_broker(10, 30, parent, exe));
        assert!(!is_session_broker(99, 30, parent, exe)); // sibling
        assert!(!is_session_broker(20, 30, parent, exe)); // wrapper
        for peer in [10, 20] {
            assert!(!is_session_broker(peer, 30, parent, |_| Some(true))); // ambiguous
        }
    }

    #[test]
    fn approved_body_is_staged_unchangeable_and_unbounded_by_argv() {
        // Far past the ~128 KiB a single argument may hold.
        let body = "\u{2603}".repeat(60_000);
        let staged = ApprovedBody::stage(&body).expect("stage");
        let path = staged.path();
        assert!(path.starts_with("/dev/fd/"), "{path}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), body);
        #[cfg(target_os = "linux")]
        {
            // Sealed: neither writing through a reopened descriptor nor truncating it can
            // change the approved bytes.
            if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(&path) {
                assert!(file.write_all(b"unapproved").is_err());
            }
            let _ = std::fs::OpenOptions::new().write(true).truncate(true).open(&path);
            assert_eq!(std::fs::read_to_string(&path).expect("read"), body);
        }
        drop(staged);
    }

    #[test]
    fn confirm_none_without_broker_or_tty() {
        // The marker without a broker never reaches /dev/tty; a dead socket path is no answer.
        let env = Env::from_pairs(&[("OXA_CODING_AGENT", "true")]);
        assert_eq!(tty_confirm(&env)("q"), None);
        let env = Env::from_pairs(&[("OXA_CONFIRM_SOCK", "/nonexistent/oxa-confirm.sock")]);
        assert_eq!(tty_confirm(&env)("q"), None);
    }
}

use oxa_work::{command_runner, COMMAND_DEADLINE};

/// The real runner: the program's stdout, killed after 30 s. The program is resolved through
/// the trusted host-tool lookup (`PATH` from `env`, never inside `cwd` or a group- or
/// world-writable directory), the same lookup `oxa work` uses for `gh`.
pub fn process_runner<'a>(
    env: &'a Env,
    cwd: &'a Path,
    home: &'a Path,
) -> impl FnMut(&str, &[String]) -> Option<(i32, String)> + 'a {
    move |program: &str, args: &[String]| {
        let path = env.get("PATH").map(std::ffi::OsStr::new);
        let Some(exe) = crate::netguard::trusted_executable_for(program, path, Some(cwd), Some(home)) else {
            if path.is_some_and(|p| std::env::split_paths(p).any(|d| d.join(program).is_file())) {
                eprintln!("{}", untrusted_tool(program));
            }
            return None;
        };
        let mut command = std::process::Command::new(exe);
        command.args(args);
        command_runner(command, None, COMMAND_DEADLINE)
    }
}

/// Why a host tool on `PATH` was not run.
fn untrusted_tool(program: &str) -> String {
    format!(
        "oxa: `{program}` was not run: the `{program}` on PATH is not in a trusted location \
(it is inside the project or in a group- or world-writable directory). Install `{program}` \
system-wide or put it in a trusted PATH entry outside the project (for example ~/.local/bin)."
    )
}

/// The runner `submit` and the in-session publish tool share: the semantic judge for `oxa`,
/// the trusted `gh` (home as its directory, `GH_HOST` pinned) in semantic mode, the plain
/// process runner otherwise. One implementation of the publication path.
pub(crate) fn submit_runner<'a>(
    dirs: &'a Dirs,
    cwd: &'a Path,
    env: &'a Env,
    settings: &'a Settings,
) -> impl FnMut(&str, &[String]) -> Option<(i32, String)> + 'a {
    let mut runner = process_runner(env, cwd, &dirs.home);
    move |program: &str, args: &[String]| {
        if program == "oxa" {
            return semantic::judge(args.first()?, dirs, settings, env).map(|reply| (0, reply));
        }
        if semantic_reporting(settings) {
            let project = cwd
                .ancestors()
                .find(|path| path.join(".git").exists())
                .unwrap_or(cwd);
            let gh = crate::netguard::trusted_executable_for(
                "gh",
                env.get("PATH").map(OsStr::new),
                Some(project),
                Some(&dirs.home),
            )?;
            let mut command = std::process::Command::new(gh);
            command
                .args(args)
                .current_dir(&dirs.home)
                .env("GH_HOST", "github.com");
            command_runner(command, Some(1_048_576), COMMAND_DEADLINE)
        } else {
            runner(program, args)
        }
    }
}

/// `oxa report-issue draft|submit ..`. Returns the exit code.
pub fn run_report_issue(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let usage = |m: &str| OxaError::new(codes::USAGE, m.to_string()).with_hint(USAGE);
    let ctx = ScrubCtx::from_env(env, dirs);
    let mut runner = process_runner(env, cwd, &dirs.home);
    match words.first().map(String::as_str) {
        Some("draft") => {
            let mut title: Option<String> = None;
            let mut notes = String::new();
            let mut i = 1;
            while i < words.len() {
                let next = words.get(i + 1).cloned();
                match words[i].as_str() {
                    "--title" => title = Some(next.ok_or_else(|| usage("--title needs a value"))?),
                    "--notes" => notes = next.ok_or_else(|| usage("--notes needs a value"))?,
                    "--notes-file" => {
                        let f = next.ok_or_else(|| usage("--notes-file needs a value"))?;
                        let p = if Path::new(&f).is_absolute() {
                            PathBuf::from(&f)
                        } else {
                            cwd.join(&f)
                        };
                        notes = std::fs::read_to_string(&p)
                            .map_err(|e| OxaError::io(&format!("cannot read {}", p.display()), &e))?;
                    }
                    other => return Err(usage(&format!("unknown report-issue option: {other}"))),
                }
                i += 2;
            }
            let title = title.ok_or_else(|| usage("a title is required (--title)"))?;
            let (path, title, body) = save_draft(&title, &notes, dirs, &ctx, &mut runner)?;
            let w = |e: std::io::Error| OxaError::io("cannot write the draft", &e);
            writeln!(out, "Draft: {}\n\nTitle: {title}\n\n{body}\n", path.display()).map_err(w)?;
            writeln!(
                out,
                "Nothing was posted. To post it, run `oxa report-issue submit {}` in a terminal. \
In an interactive oxa session, ask for it to be published: the approval dialog shows this exact text and only the owner's key press posts it.",
                path.display()
            )
            .map_err(w)?;
            Ok(0)
        }
        Some("submit") => {
            let [_, draft] = words else {
                return Err(usage("submit takes the draft path"));
            };
            let settings = Settings::load(dirs, cwd, false);
            let mut confirm = tty_confirm(env);
            let mut publish_runner = submit_runner(dirs, cwd, env, &settings);
            let draft = cwd.join(draft);
            submit_with_storage(
                &draft,
                &report_ledger(dirs),
                &settings,
                &ctx,
                &mut confirm,
                &mut publish_runner,
                out,
            )
        }
        _ => Err(usage("report-issue needs `draft` or `submit`")),
    }
}

#[cfg(test)]
mod consent_tests {
    use super::semantic_reporting;
    use oxa_config::Settings;
    use serde_json::json;

    /// The semantic-dedupe consent is the owner's: a trusted project's settings must not grant
    /// it, because that consent sends the report text to GitHub and to the configured model.
    #[test]
    fn a_project_cannot_grant_the_semantic_consent() {
        let key = "oxa.reportIssue.semanticDedupe";
        let project_only = Settings::from_values(
            json!({}),
            json!({"oxa": {"reportIssue": {"semanticDedupe": true}}}),
        );
        assert!(
            !semantic_reporting(&project_only),
            "a project file granted consent"
        );
        let user_on = Settings::from_values(
            json!({"oxa": {"reportIssue": {"semanticDedupe": true}}}),
            json!({}),
        );
        assert!(semantic_reporting(&user_on), "the owner's own switch must work");
        let user_off_project_on = Settings::from_values(
            json!({"oxa": {"reportIssue": {"semanticDedupe": false}}}),
            json!({"oxa": {"reportIssue": {"semanticDedupe": true}}}),
        );
        assert!(!semantic_reporting(&user_off_project_on));
        assert!(!semantic_reporting(&Settings::from_values(json!({}), json!({}))));
        assert_eq!(key, super::semantic::SETTING);
    }
}
