//! One publication path for reports.
//!
//! A report is approved and posted by the process the owner trusts. A tool child only hands
//! over structured fields ([`BrokerGate`]); the oxa parent prepares the final text, shows it,
//! holds the approved bytes and posts them with `gh` itself ([`ReportPublisher`], installed in
//! the session's owner confirm broker). Nothing the model can write, trace or fake sits between
//! the owner's yes and `gh`. In-process callers (the in-session `report_publish` tool, `oxa
//! work` reports, a manual terminal) use [`LocalGate`], which runs the same `final_text` and
//! posts through the same [`post_with_runner`].
use super::{
    publication_offline, report_destination, scrub, submit_runner, ApprovedBody, Confirm, Runner, ScrubCtx,
    REPORT_DISABLED,
};
use oxa_config::{Dirs, Env, Settings};
use oxa_core::owner_confirm::{self, Publication, Publisher, Request};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// The text that is posted. Control and bidirectional characters, and text in which the scrubber
/// finds a secret (a key, a credential: it adds a `<secret>` marker), are refused in every mode,
/// yolo included. Privacy text (an email, a home path, a host or user name) is not a refusal:
/// it is replaced, and the scrubbed title and body are what the owner sees and what is posted.
pub(crate) fn final_text(title: &str, body: &str, ctx: &ScrubCtx) -> Result<(String, String), String> {
    Ok((checked_text(title, ctx)?, checked_text(body, ctx)?))
}

fn checked_text(text: &str, ctx: &ScrubCtx) -> Result<String, String> {
    if text.chars().any(|ch| {
        (ch.is_control() && !matches!(ch, '\n' | '\t'))
            || matches!(
                ch,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    }) {
        return Err("the text holds control or bidirectional characters, which are never posted".into());
    }
    let scrubbed = scrub(text, ctx);
    let secrets = |text: &str| text.matches("<secret>").count();
    if secrets(&scrubbed) > secrets(text) {
        return Err("the text holds a secret (a key or a credential), which is never posted".into());
    }
    Ok(scrubbed)
}

/// Check all requester-supplied text that will be rendered, including the preview-only note.
fn prepare_text(mut publication: Publication, ctx: &ScrubCtx) -> Result<Publication, String> {
    (publication.title, publication.body) = final_text(&publication.title, &publication.body, ctx)?;
    publication.note = publication
        .note
        .as_deref()
        .map(|note| checked_text(note, ctx))
        .transpose()?;
    Ok(publication)
}

/// The destination a publication may go to, refused when reporting is off, when the text names
/// another repository, or when the process's network policy is offline. Picks the repository
/// the owner's settings name, unless the caller's own text names an explicit non-default
/// repository (a broker request has no other way to name one).
fn destination(settings: &Settings, requested: &str) -> Result<String, String> {
    if let Some(refusal) = publication_offline("posting the issue with `gh`") {
        return Err(refusal);
    }
    let Some((target, explicit)) = report_destination(settings) else {
        return Err(REPORT_DISABLED.to_string());
    };
    if !requested.is_empty() && !requested.eq_ignore_ascii_case(&target) && !explicit {
        return Err(format!(
            "reports go to {target}, not {requested}; nothing was posted"
        ));
    }
    Ok(target)
}

/// Prepare the text to show and the destination to post to. The note is shown but not posted.
pub(crate) fn prepare_publication(
    mut publication: Publication,
    ctx: &ScrubCtx,
    settings: &Settings,
) -> Result<Publication, String> {
    publication.repository = destination(settings, &publication.repository)?;
    prepare_text(publication, ctx)
}

/// Hand `gh` exactly the approved bytes: the body goes through a sealed descriptor nothing else
/// can name or change, never through a file in a model-writable directory or an argument.
pub(crate) fn post_with_runner(publication: &Publication, runner: Runner<'_>) -> Option<(i32, String)> {
    let staged = ApprovedBody::stage(&publication.body).ok()?;
    let body_path = staged.path();
    let number;
    let mut args: Vec<&str> = if let Some(issue) = publication.issue {
        number = issue.to_string();
        vec!["issue", "comment", &number, "--repo", &publication.repository]
    } else {
        vec![
            "issue",
            "create",
            "--repo",
            &publication.repository,
            "--title",
            &publication.title,
        ]
    };
    args.extend(["--body-file", &body_path]);
    let args: Vec<String> = args.into_iter().map(str::to_string).collect();
    let result = runner("gh", &args);
    drop(staged);
    result
}

/// The parent's publication side, installed in the session's broker.
pub(crate) struct ReportPublisher {
    dirs: Dirs,
    env: Env,
    settings: Settings,
    cwd: PathBuf,
    ctx: ScrubCtx,
}

impl ReportPublisher {
    pub(crate) fn new(dirs: &Dirs, env: &Env, settings: &Settings, cwd: &Path) -> Self {
        Self {
            dirs: dirs.clone(),
            env: env.clone(),
            settings: settings.clone(),
            cwd: cwd.to_path_buf(),
            ctx: ScrubCtx::from_env(env, dirs),
        }
    }
}

impl Publisher for ReportPublisher {
    fn prepare(&self, publication: Publication) -> Result<Publication, String> {
        prepare_publication(publication, &self.ctx, &self.settings)
    }

    fn post(&self, publication: &Publication) -> Result<String, String> {
        // The posting process checks the network policy again: the child that asked cannot
        // loosen it, and a held `yes` is never spent on a post the policy refuses.
        if let Err(refusal) = destination(&self.settings, &publication.repository) {
            return Err(refusal);
        }
        let mut runner = submit_runner(&self.dirs, &self.cwd, &self.env, &self.settings);
        match post_with_runner(publication, &mut runner) {
            Some((0, stdout)) => Ok(stdout.trim().to_string()),
            Some((code, _)) => Err(format!("GitHub did not confirm the write (exit {code})")),
            None => Err("the GitHub CLI `gh` could not run".to_string()),
        }
    }
}

/// What `submit_record` needs from whoever approves and posts: exactly one `approve` per post,
/// then `post`.
pub(crate) trait PublishGate {
    /// `Some(true)` approved, `Some(false)` declined or refused (see [`PublishGate::reason`]),
    /// `None` nobody could be asked.
    fn approve(&mut self, publication: &Publication) -> Option<bool>;
    /// Post what was approved. `runner` runs `gh` for in-process gates; the broker gate asks the
    /// parent and ignores it.
    fn post(&mut self, runner: Runner<'_>) -> Option<(i32, String)>;
    /// Why the last approve or post did not go through, when the gate knows.
    fn reason(&self) -> Option<String>;
}

/// In-process approval: `final_text`, a caller-supplied ask on the rendered text, then the
/// caller's runner posts. `suffix` is appended to the text for a terminal prompt (the dialog of
/// the in-session tool shows the text alone).
pub(crate) struct LocalGate<'a> {
    ctx: &'a ScrubCtx,
    ask: Confirm<'a>,
    suffix: &'static str,
    approved: Option<Publication>,
    reason: Option<String>,
}

impl<'a> LocalGate<'a> {
    pub(crate) fn new(ctx: &'a ScrubCtx, ask: Confirm<'a>, suffix: &'static str) -> Self {
        Self {
            ctx,
            ask,
            suffix,
            approved: None,
            reason: None,
        }
    }
}

impl PublishGate for LocalGate<'_> {
    fn approve(&mut self, publication: &Publication) -> Option<bool> {
        self.approved = None;
        self.reason = None;
        let prepared = match prepare_text(publication.clone(), self.ctx) {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.reason = Some(refusal);
                return Some(false);
            }
        };
        let question = format!("{}{}", prepared.render(), self.suffix);
        let answer = (self.ask)(&question);
        if answer == Some(true) {
            self.approved = Some(prepared);
        }
        answer
    }

    fn post(&mut self, runner: Runner<'_>) -> Option<(i32, String)> {
        let approved = self.approved.take()?;
        post_with_runner(&approved, runner)
    }

    fn reason(&self) -> Option<String> {
        self.reason.clone()
    }
}

/// A tool child's side: hand the fields to the session's broker, which prepares, shows, holds
/// and posts. No listener verification is needed, because a fake listener cannot post.
pub(crate) struct BrokerGate {
    sock: PathBuf,
    approved: Option<UnixStream>,
    reason: Option<String>,
}

impl BrokerGate {
    pub(crate) fn new(sock: &Path) -> Self {
        Self {
            sock: sock.to_path_buf(),
            approved: None,
            reason: None,
        }
    }
}

impl PublishGate for BrokerGate {
    fn approve(&mut self, publication: &Publication) -> Option<bool> {
        self.approved = None;
        self.reason = None;
        let Ok(stream) = UnixStream::connect(&self.sock) else {
            return None;
        };
        let reply = owner_confirm::exchange(&stream, &Request::Publish(publication.clone()));
        self.reason = reply.reason.clone();
        if reply.approved {
            self.approved = Some(stream);
            return Some(true);
        }
        reply.answer()
    }

    fn post(&mut self, _runner: Runner<'_>) -> Option<(i32, String)> {
        let stream = self.approved.take()?;
        match owner_confirm::post_on(&stream) {
            Ok(url) => Some((0, url)),
            Err(detail) => {
                self.reason = Some(detail.clone());
                Some((1, detail))
            }
        }
    }

    fn reason(&self) -> Option<String> {
        self.reason.clone()
    }
}

/// A gate for a tool child that has no broker: nothing can be approved.
pub(crate) struct NoGate;

impl PublishGate for NoGate {
    fn approve(&mut self, _: &Publication) -> Option<bool> {
        None
    }
    fn post(&mut self, _: Runner<'_>) -> Option<(i32, String)> {
        None
    }
    fn reason(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report_cmd::run_report_issue;
    use oxa_core::owner_confirm::OwnerConfirm;
    use oxa_fake::TestEnv;
    use oxa_proto::{Autonomy, YoloSwitch};
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    struct Fixture {
        env: TestEnv,
        draft: PathBuf,
        log: PathBuf,
        bodies: PathBuf,
        path: String,
    }

    /// A draft and a fake `gh` that records its arguments and the body file it is handed.
    fn fixture(body: &str) -> Fixture {
        let env = TestEnv::new();
        let draft = env.write(
            "home/.oxa/agent/issue-drafts/issue-aaaaaaaaaaaa.md",
            &format!("Title: Engine stalls\n\n{body}\n"),
        );
        let log = env.root().join("gh.log");
        let bodies = env.root().join("gh-bodies.log");
        let bin = env.root().join("tools");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\nfor a in \"$@\"; do case \"$a\" in /dev/fd/*) cat \"$a\" >> '{}';; esac; done\necho https://github.com/oxstack/ox-llm/issues/7\n",
                log.display(),
                bodies.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fixture {
            path: bin.display().to_string(),
            env,
            draft,
            log,
            bodies,
        }
    }

    fn child_env(f: &Fixture, sock: &Path) -> Env {
        Env::from_pairs(&[
            ("PATH", f.path.as_str()),
            ("OXA_CODING_AGENT", "true"),
            ("OXA_CONFIRM_SOCK", &sock.display().to_string()),
        ])
    }

    fn submit_as_child(f: &Fixture, env: &Env) -> (i32, String) {
        let dirs = Dirs::under(f.env.root());
        let mut out = Vec::new();
        let code = run_report_issue(
            &["submit".to_string(), f.draft.display().to_string()],
            &dirs,
            &f.env.work(),
            env,
            &mut out,
        )
        .unwrap();
        (code, String::from_utf8_lossy(&out).into_owned())
    }

    /// #1925: a tool child posts through the session's broker with the agent marker set and no
    /// sandbox. The parent posts the approved bytes; the child never runs `gh`.
    #[test]
    fn a_tool_child_posts_through_the_broker_with_the_agent_marker_and_no_sandbox() {
        let f = fixture("It stalls.");
        let dirs = Dirs::under(f.env.root());
        let broker =
            OwnerConfirm::start(&f.env.root().join("broker"), YoloSwitch::new(Autonomy::Yolo)).unwrap();
        let vars = Env::from_pairs(&[("PATH", f.path.as_str())]);
        broker.set_publisher(Arc::new(ReportPublisher::new(
            &dirs,
            &vars,
            &Settings::default(),
            &f.env.work(),
        )));
        let (code, out) = submit_as_child(&f, &child_env(&f, broker.sock_path()));
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("Posted: https://github.com/oxstack/ox-llm/issues/7"),
            "{out}"
        );
        let calls = std::fs::read_to_string(&f.log).unwrap();
        assert!(
            calls.contains("issue create --repo oxstack/ox-llm --title Engine stalls --body-file /dev/fd/"),
            "{calls}"
        );
        assert_eq!(std::fs::read_to_string(&f.bodies).unwrap(), "It stalls.");
        let records = broker.take_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].repository.as_deref(), Some("oxstack/ox-llm"));
        assert_eq!(
            records[0].url.as_deref(),
            Some("https://github.com/oxstack/ox-llm/issues/7")
        );
        broker.close();
    }

    /// Offline refuses a publication in the parent before a dialog or `gh`: the broker runs
    /// `gh` outside the tool's network namespace, so the process policy is checked there too.
    #[test]
    fn the_broker_publisher_refuses_a_publication_offline_before_the_dialog_or_gh() {
        let f = fixture("It stalls.");
        let dirs = Dirs::under(f.env.root());
        let vars = Env::from_pairs(&[("PATH", f.path.as_str())]);
        let publisher = ReportPublisher::new(&dirs, &vars, &Settings::default(), &f.env.work());
        let publication = Publication {
            repository: "oxstack/ox-llm".into(),
            issue: None,
            title: "Engine stalls".into(),
            body: "It stalls.".into(),
            note: None,
        };
        oxa_proto::set_net_policy(true, false);
        let prepared = publisher.prepare(publication.clone());
        oxa_proto::set_net_policy(false, false);
        let refusal = prepared.expect_err("offline refuses the publication");
        assert!(refusal.contains("offline mode is on"), "{refusal}");
        // The same publication prepares and posts online: the refusal is the policy, not the text.
        let prepared = publisher.prepare(publication).expect("online prepares");
        oxa_proto::set_net_policy(true, false);
        let posted = publisher.post(&prepared);
        oxa_proto::set_net_policy(false, false);
        let refusal = posted.expect_err("offline refuses the post");
        assert!(refusal.contains("offline mode is on"), "{refusal}");
        assert!(!f.log.exists(), "no gh while offline");
    }

    /// A listener the model started can answer yes, but it cannot post: the child runs no `gh`.
    #[test]
    fn a_fake_listener_answering_yes_makes_the_child_post_nothing() {
        let f = fixture("It stalls.");
        let sock = f.env.root().join("fake.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let _ = writeln!(stream, r#"{{"answer":"yes","source":"yolo","approved":true}}"#);
            let _ = stream.read(&mut buf);
        });
        let (code, out) = submit_as_child(&f, &child_env(&f, &sock));
        fake.join().unwrap();
        assert_eq!(code, 1, "{out}");
        assert!(!f.log.exists(), "the child never runs gh");
    }

    #[test]
    fn a_tool_child_without_a_broker_asks_nobody_and_posts_nothing() {
        let f = fixture("It stalls.");
        let env = Env::from_pairs(&[("PATH", f.path.as_str()), ("OXA_CODING_AGENT", "true")]);
        let (code, _) = submit_as_child(&f, &env);
        assert_eq!(code, 1);
        assert!(!f.log.exists());
    }

    #[test]
    fn the_preview_only_note_is_scrubbed_without_changing_the_post() {
        let ctx = ScrubCtx::default();
        let publication = Publication {
            repository: "oxstack/ox-llm".into(),
            issue: None,
            title: "Engine stalls".into(),
            body: "It stalls.".into(),
            note: Some("Warning: contact owner@example.invalid.".into()),
        };
        let prepared = prepare_text(publication, &ctx).expect("safe note");
        assert!(prepared.render().starts_with("Warning: contact <email>.\n\n"));
        assert_eq!(prepared.body, "It stalls.", "the note is not part of the post");
    }

    #[test]
    fn final_text_scrubs_privacy_text_and_refuses_secrets_and_control_text() {
        let ctx = ScrubCtx {
            home: Some("/home/dave".to_string()),
            ..ScrubCtx::default()
        };
        let (title, body) = final_text("Bug", "mail a@example.com from /home/dave/x", &ctx)
            .expect("privacy text is replaced");
        assert_eq!(title, "Bug");
        assert!(!body.contains("example.com") && !body.contains("dave"), "{body}");
        let label = "OPENSSH PRIVATE KEY";
        let key = format!("-----BEGIN {label}-----\nsynthetic\n-----END {label}-----");
        assert!(final_text("Bug", &key, &ctx).is_err());
        assert!(final_text("Bug", "text\u{202e}", &ctx).is_err());
        // Text that already holds the marker is not a new secret.
        assert!(final_text("Bug", "the log says <secret> here", &ctx).is_ok());
    }
}
