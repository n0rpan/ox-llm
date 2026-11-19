//! O-152: `oxa report-issue` (architect-owned), the code behind the built-in `report-issue`
//! skill. `draft` collects a scrubbed digest and saves a draft; `submit` shows the exact text
//! and posts it with `gh issue create` only after the user typed `yes` on the controlling
//! terminal. Reporting is off until `oxa.reportIssue.repo` names the public repository.
//!
//! What a draft may contain: the user's description (scrubbed, capped, no transcript records),
//! the identity facts (version, machine, OS, models) and the controller's `status --json`
//! scalars and `digest --json` (numbers only). Never prompts, transcripts, file contents,
//! emails, home paths, user or host names, IP addresses or secrets.
mod semantic;

use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, OxaError};
use oxa_resources::{read_identity, write_atomic};
use regex::Regex;
use serde_json::{Map, Value as Json};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const USAGE: &str =
    "usage: oxa report-issue draft --title TITLE [--notes-file FILE | --notes TEXT] | oxa report-issue submit DRAFT";

/// The setting that names the target repository (`OWNER/NAME`); unset = reporting is off.
pub const SETTING_REPORT_REPO: &str = "oxa.reportIssue.repo";

/// What `submit` says while no repository is configured.
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

/// The controller's CLI: `oxc` (v0.2.0), else the pre-rename `local-ai`.
const CONTROLLER_CLIS: [&str; 2] = ["oxc", "local-ai"];

/// Run `<controller> args`; the first CLI that runs wins.
fn controller(runner: Runner<'_>, args: &[&str]) -> Option<String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    CONTROLLER_CLIS
        .iter()
        .find_map(|cli| runner(cli, &args))
        .map(|(_, out)| out)
}

/// The top-level scalar fields of a JSON object (no nested objects or arrays: they may hold
/// paths or lists the report does not need).
fn scalars(json: &str) -> Option<String> {
    let Json::Object(obj) = serde_json::from_str::<Json>(json).ok()? else {
        return None;
    };
    let kept: Map<String, Json> = obj
        .into_iter()
        .filter(|(_, v)| !(v.is_object() || v.is_array()))
        .collect();
    serde_json::to_string_pretty(&Json::Object(kept)).ok()
}

fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_SECTION_CHARS {
        text.to_string()
    } else {
        text.chars().take(MAX_SECTION_CHARS).collect::<String>() + "\n[cut]"
    }
}

/// The body of an issue: the description, the identity facts, the controller's status
/// scalars and today's health digest. Every part is scrubbed.
pub fn draft_body(notes: &str, dirs: &Dirs, ctx: &ScrubCtx, runner: Runner<'_>) -> String {
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
    let status = controller(runner, &["status", "--json"]).and_then(|o| scalars(&o));
    parts.push(format!(
        "## Controller status\n\n{}",
        match status {
            Some(s) => format!("```json\n{}\n```", scrub(&cap(&s), ctx)),
            None => "(the controller CLI did not answer)".to_string(),
        }
    ));
    let digest = controller(runner, &["digest", "--json"])
        .and_then(|o| serde_json::from_str::<Json>(&o).ok())
        .and_then(|v| serde_json::to_string_pretty(&v).ok());
    parts.push(format!(
        "## Health digest (today)\n\n{}",
        match digest {
            Some(d) => format!("```json\n{}\n```", scrub(&cap(&d), ctx)),
            None => "(no digest available)".to_string(),
        }
    ));
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
    settings.bool(semantic::SETTING, false)
}

pub(crate) fn report_ledger(dirs: &Dirs) -> PathBuf {
    dirs.agent_dir.join("issue-drafts/report-ledger")
}

pub(crate) use semantic::judge as judge_report;

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
    let Some(repo) = report_repo(settings) else {
        writeln!(out, "{REPORT_DISABLED}\nDraft: {}", draft.display()).map_err(w)?;
        return Ok(Submission::Disabled);
    };
    let mut publication = if semantic_reporting(settings) {
        writeln!(out, "Repository: {repo}\nTitle: {title}\n\n{body}\n").map_err(w)?;
        match semantic::plan(storage, &repo, (&title, &body), ctx, confirm, runner) {
            Ok(publication) => Some(publication),
            Err(reason) => {
                writeln!(out, "Kept locally at {} ({reason}).", draft.display()).map_err(w)?;
                return Ok(Submission::Local(reason.into()));
            }
        }
    } else {
        writeln!(out, "Repository: {repo}\nTitle: {title}\n\n{body}\n").map_err(w)?;
        match confirm(&format!("Post this issue publicly to {repo}? Type yes to post: ")) {
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
        if confirm(&format!(
            "Comment on {repo}#{issue}?\n\n{body}\n\nType yes to post: "
        )) != Some(true)
        {
            writeln!(out, "Comment retained locally at {}.", draft.display()).map_err(w)?;
            return Ok(Submission::Local("comment requires confirmation".into()));
        }
        number = issue.to_string();
        vec!["issue", "comment", &number, "--repo", &repo]
    } else {
        vec!["issue", "create", "--repo", &repo, "--title", &title]
    };
    // Keep the bytes that were displayed and approved in this process. A draft lives in a
    // model-writable directory, so handing `gh` a pathname would let another process replace
    // the file between confirmation and the time `gh` opens it.
    args.extend(["--body", body]);
    let args: Vec<String> = args.into_iter().map(str::to_string).collect();
    let result = runner("gh", &args);
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

/// The real confirmation. Under oxa's own tools the session exports `OXA_CONFIRM_SOCK`: the
/// question goes to the session's owner confirm broker, which answers from the owner (yolo, the
/// owner's dialog answer or the owner's typed message), never from text the model read. Without a
/// broker, a tool child (`OXA_CODING_AGENT` set) gets `None`. A manual terminal asks through
/// `/dev/tty` (never stdin, so piped input cannot confirm) and gets `None` without one.
pub(crate) fn tty_confirm(env: &Env) -> impl FnMut(&str) -> Option<bool> + '_ {
    tty_confirm_with(env, oxa_core::owner_confirm::ask)
}

fn tty_confirm_with<'a>(
    env: &'a Env,
    ask: impl Fn(&Path, &str) -> Option<bool> + 'a,
) -> impl FnMut(&str) -> Option<bool> + 'a {
    move |question: &str| {
        if let Some(sock) = env.get(oxa_core::owner_confirm::SOCK_ENV) {
            // A model-run shell can replace environment variables. Under the agent marker,
            // accept only the socket that bwrap pinned at its immutable mount point; never let
            // an environment-selected listener manufacture an owner's approval.
            if env.get("OXA_CODING_AGENT").is_none() || sock == oxa_sandbox::OWNER_CONFIRM_SOCK {
                return ask(Path::new(sock), question);
            }
            return None;
        }
        if env.get("OXA_CODING_AGENT").is_some() {
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

        // The same process may override its environment, but an arbitrary listener is not the
        // session broker mounted by the sandbox and must never count as owner approval.
        let path = "/workspace/fake-confirm.sock";
        let env = Env::from_pairs(&[("OXA_CODING_AGENT", "true"), ("OXA_CONFIRM_SOCK", path)]);
        assert_eq!(tty_confirm(&env)("q"), None);

        // Outside an agent tool, the private host path remains usable.
        let (sock, _seen) = broker("no", "no");
        let path = sock.to_string_lossy().into_owned();
        let env = Env::from_pairs(&[("OXA_CONFIRM_SOCK", &path)]);
        assert_eq!(tty_confirm(&env)("q"), Some(false));
        let (sock, _seen) = broker("none", "none");
        let path = sock.to_string_lossy().into_owned();
        let env = Env::from_pairs(&[("OXA_CONFIRM_SOCK", &path)]);
        assert_eq!(tty_confirm(&env)("q"), None);
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
                "Nothing was posted. To post it, run `oxa report-issue submit {}` in a terminal.",
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
            let mut submit_runner = |program: &str, args: &[String]| {
                if program == "oxa" {
                    return semantic::judge(args.first()?, dirs, &settings, env).map(|reply| (0, reply));
                }
                if semantic_reporting(&settings) {
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
            };
            let draft = cwd.join(draft);
            submit_with_storage(
                &draft,
                &report_ledger(dirs),
                &settings,
                &ctx,
                &mut confirm,
                &mut submit_runner,
                out,
            )
        }
        _ => Err(usage("report-issue needs `draft` or `submit`")),
    }
}
