//! O-152: `oxa report-issue` (scrubber, drafts, never posts without confirmation) and
//! `oxa identity` (facts from a synthetic machine tree, the identity file). Synthetic data only;
//! no network, no real `gh`, no real terminal.

use oxa_cli::{
    clean_notes, identity_facts, parse_draft, process_runner, report_repo, run_identity, save_draft, scrub,
    submit, write_identity, ScrubCtx, MAX_NOTES_CHARS, REPORT_DISABLED,
};
use oxa_config::{Dirs, Env, Settings};
use oxa_fake::TestEnv;
use oxa_resources::read_identity;
use serde_json::json;
use std::cell::RefCell;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

fn ctx() -> ScrubCtx {
    ScrubCtx {
        home: Some("/srv/users/carol".to_string()),
        user: Some("carol".to_string()),
        host: Some("carols-laptop".to_string()),
    }
}

/// A writer the confirmation callback can look into.
#[derive(Clone, Default)]
struct Shared(Rc<RefCell<Vec<u8>>>);
impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Shared {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.borrow()).into_owned()
    }
}

fn repo_settings(repo: Option<&str>) -> Settings {
    match repo {
        Some(r) => Settings::from_values(json!({"oxa": {"reportIssue": {"repo": r}}}), json!({})),
        None => Settings::default(),
    }
}

fn write_draft(env: &TestEnv, title: &str, body: &str) -> std::path::PathBuf {
    env.write("drafts/issue.md", &format!("Title: {title}\n\n{body}\n"))
}

// ---------------------------------------------------------------------------------------------
// The scrubber

#[test]
fn scrubber_removes_emails_home_paths_names_and_secrets() {
    let pem =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
    let input = format!(
        "Contact carol.smith+oxa@example.co.uk or Admin@Example.ORG.\n\
         Log: /srv/users/carol/.local/state/oxc/log.txt and /home/alice/Work/x.rs and /Users/bob/x and /root/.ssh\n\
         User carol on carols-laptop (Carols-Laptop) via 192.168.1.23 and box.tail1234.ts.net, local 127.0.0.1:8092\n\
         gh token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 and sk-proj-AbCdEf0123456789xyz and AKIAIOSFODNN7EXAMPLE\n\
         Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig\n\
         {{\"password\": \"hunter2-secret\", \"api_key\":\"k-123\"}} token=abc123 SECRET = s3cr3t\n\
         url https://carol:pa55word@git.example.com/repo.git\n\
         random Zx9Qw8Er7Ty6Ui5Op4As3Df2Gh1Jk0LmNb\n\
         {pem}\n\
         keep: commit 0123456789abcdef0123456789abcdef01234567, \"prompt_tokens\": 5120, context 131072 tokens, oxa 0.2.0"
    );
    let out = scrub(&input, &ctx());
    for gone in [
        "carol.smith",
        "example.co.uk",
        "Admin@Example.ORG",
        "/srv/users/carol",
        "/home/alice",
        "/Users/bob",
        "/root",
        "carol",
        "Carols-Laptop",
        "carols-laptop",
        "192.168.1.23",
        "tail1234",
        "ghp_aBcD",
        "sk-proj",
        "AKIAIOSFODNN7",
        "eyJhbGci",
        "hunter2",
        "k-123",
        "abc123",
        "s3cr3t",
        "pa55word",
        "Zx9Qw8Er7",
        "b3BlbnNzaC1rZXkt",
        "PRIVATE KEY",
    ] {
        assert!(!out.contains(gone), "{gone:?} survived:\n{out}");
    }
    for kept in [
        "<email>",
        "~/.local/state/oxc/log.txt",
        "~/Work/x.rs",
        "<user>",
        "<host>",
        "<ip>",
        "127.0.0.1:8092",
        "Bearer <secret>",
        "0123456789abcdef0123456789abcdef01234567",
        "\"prompt_tokens\": 5120",
        "context 131072 tokens",
        "oxa 0.2.0",
        "git.example.com/repo.git",
    ] {
        assert!(out.contains(kept), "{kept:?} missing:\n{out}");
    }
    assert_eq!(scrub(&out, &ctx()), out, "idempotent");
    assert_eq!(
        scrub("plain text, nothing to hide", &ctx()),
        "plain text, nothing to hide"
    );
}

#[test]
fn notes_lose_transcript_records_and_are_capped() {
    let notes = "It hangs after a model switch.\n{\"role\":\"user\",\"content\":\"secret prompt\"}\n\"messages\": [1]\nThen it recovers.";
    let cleaned = clean_notes(notes, &ctx());
    assert_eq!(cleaned, "It hangs after a model switch.\nThen it recovers.");
    let long = "word ".repeat(MAX_NOTES_CHARS);
    let cleaned = clean_notes(&long, &ctx());
    assert!(cleaned.ends_with("[cut]") && cleaned.chars().count() <= MAX_NOTES_CHARS + 6);
}

// ---------------------------------------------------------------------------------------------
// Drafts

#[test]
fn draft_collects_a_scrubbed_digest_and_posts_nothing() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    env.write(
        "home/.oxa/agent/identity.md",
        "oxa is the coding agent of ox-llm.\n\n- oxa version: 0.2.0\n- Machine: Test Laptop, Snapdragon X Elite (X1E80100)\n- System: Omarchy Linux\n",
    );
    let mut calls: Vec<(String, Vec<String>)> = Vec::new();
    let mut runner = |program: &str, args: &[String]| -> Option<(i32, String)> {
        calls.push((program.to_string(), args.to_vec()));
        match (program, args.first().map(String::as_str)) {
            ("oxc", _) => None, // not installed yet: the pre-rename CLI answers
            ("local-ai", Some("status")) => Some((
                0,
                json!({
                    "chat_state": "ready", "available_gib": 12.5, "owner": "carol@example.com",
                    "log": "/srv/users/carol/.local/state/local-ai/x.log",
                    "sessions": [{"prompt": "my secret prompt"}], "nested": {"path": "/home/carol"}
                })
                .to_string(),
            )),
            ("local-ai", Some("digest")) => Some((
                0,
                json!({"requests": 42, "errors": 0, "p50_ms": 81.1}).to_string(),
            )),
            _ => None,
        }
    };
    let (path, title, body) = save_draft(
        "Flash stalls for carol",
        "Flash stalls at /srv/users/carol/proj after switching; mail me at carol@example.com",
        &dirs,
        &ctx(),
        &mut runner,
    )
    .expect("draft");
    assert!(path.starts_with(dirs.agent_dir.join("issue-drafts")));
    assert_eq!(title, "Flash stalls for <user>");
    let saved = std::fs::read_to_string(&path).unwrap();
    assert_eq!(parse_draft(&saved), Some((title.clone(), body.clone())));
    for gone in [
        "carol",
        "example.com",
        "/srv/users",
        "my secret prompt",
        "nested",
        "sessions",
    ] {
        assert!(!body.contains(gone), "{gone:?} in the draft:\n{body}");
    }
    for kept in [
        "## What happened",
        "~/proj",
        "<email>",
        "\"chat_state\": \"ready\"",
        "\"requests\": 42",
        "Snapdragon X Elite",
    ] {
        assert!(body.contains(kept), "{kept:?} missing:\n{body}");
    }
    assert!(
        calls.iter().all(|(p, _)| p == "oxc" || p == "local-ai"),
        "only the controller CLI ran: {calls:?}"
    );
    assert!(!calls.iter().any(|(p, _)| p == "gh"));
}

#[test]
fn a_draft_without_a_description_is_refused() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let mut runner = |_: &str, _: &[String]| None;
    // Empty notes, blank notes, and notes that are only transcript records all leave nothing to report.
    for notes in ["", "  \n ", "{\"role\":\"user\",\"content\":\"x\"}"] {
        let err = save_draft("Report with no findings", notes, &dirs, &ctx(), &mut runner)
            .expect_err("an empty report must not be drafted");
        assert!(err.to_string().contains("no description"), "{err}");
    }
    assert!(!dirs.agent_dir.join("issue-drafts").exists());
}

// ---------------------------------------------------------------------------------------------
// Submit: never without confirmation

#[test]
fn submit_is_off_until_a_repository_is_configured() {
    let env = TestEnv::new();
    let draft = write_draft(&env, "A title", "A body.");
    let mut asked = false;
    let mut ran = false;
    let mut out = Vec::new();
    let code = submit(
        &draft,
        &repo_settings(None),
        &ctx(),
        &mut |_: &str| {
            asked = true;
            Some(true)
        },
        &mut |_: &str, _: &[String]| {
            ran = true;
            Some((0, String::new()))
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(code, 3);
    assert!(!asked && !ran, "nothing asked, nothing run");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(REPORT_DISABLED) && text.contains(&draft.display().to_string()));
    assert!(draft.exists(), "the draft stays");
    // An invalid repository value is the same as none.
    assert_eq!(report_repo(&repo_settings(Some("not a repo"))), None);
    assert_eq!(
        report_repo(&repo_settings(Some("owner/ox-llm"))).as_deref(),
        Some("owner/ox-llm")
    );
}

#[test]
fn submit_never_posts_without_an_explicit_yes() {
    let env = TestEnv::new();
    let draft = write_draft(&env, "Engine stalls", "## What happened\n\nIt stalls.");
    for answer in [Some(false), None] {
        let mut ran = false;
        let out = Shared::default();
        let seen = out.clone();
        let mut asked_after_showing = false;
        let code = submit(
            &draft,
            &repo_settings(Some("owner/ox-llm")),
            &ctx(),
            &mut |q: &str| {
                // The exact title and body are on screen before the question.
                let shown = seen.text();
                asked_after_showing = shown.contains("Title: Engine stalls")
                    && shown.contains("It stalls.")
                    && q.contains("owner/ox-llm");
                answer
            },
            &mut |_: &str, _: &[String]| {
                ran = true;
                Some((0, String::new()))
            },
            &mut out.clone(),
        )
        .unwrap();
        assert_eq!(code, 1, "{answer:?}");
        assert!(asked_after_showing, "{answer:?}");
        assert!(!ran, "no gh without a yes ({answer:?})");
        let text = out.text();
        assert!(text.contains("Not posted"), "{text}");
        if answer.is_none() {
            assert!(text.contains("yourself in a terminal"), "{text}");
        }
    }
}

#[test]
fn submit_refuses_a_draft_with_unscrubbed_text() {
    let env = TestEnv::new();
    let draft = write_draft(&env, "Bug", "Reach me at someone@example.com from /home/dave/x");
    let mut asked = false;
    let mut ran = false;
    let mut out = Vec::new();
    let code = submit(
        &draft,
        &repo_settings(Some("owner/ox-llm")),
        &ctx(),
        &mut |_: &str| {
            asked = true;
            Some(true)
        },
        &mut |_: &str, _: &[String]| {
            ran = true;
            Some((0, String::new()))
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(code, 1);
    assert!(!asked && !ran);
    assert!(String::from_utf8(out).unwrap().contains("scrubber"));
}

#[test]
fn submit_posts_the_exact_text_after_yes() {
    let env = TestEnv::new();
    let body = "## What happened\n\nIt stalls.\n\n## System\n\n- oxa version: 0.2.0";
    let draft = write_draft(&env, "Engine stalls", body);
    let mut calls: Vec<(String, Vec<String>, String)> = Vec::new();
    let mut out = Vec::new();
    let code = submit(
        &draft,
        &repo_settings(Some("owner/ox-llm")),
        &ctx(),
        &mut |_: &str| Some(true),
        &mut |p: &str, args: &[String]| {
            let posted = args
                .iter()
                .position(|a| a == "--body")
                .map(|i| args[i + 1].clone())
                .unwrap();
            // Replacing the model-writable draft after approval cannot change the bytes passed
            // to `gh`: the approved body is an owned argument, not a pathname that `gh` reopens.
            std::fs::write(&draft, "Title: Replaced\n\nunapproved body\n").unwrap();
            calls.push((p.to_string(), args.to_vec(), posted));
            Some((0, "https://github.com/owner/ox-llm/issues/1\n".to_string()))
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(calls.len(), 1);
    let (program, args, posted) = &calls[0];
    assert_eq!(program, "gh");
    assert_eq!(
        &args[..6],
        [
            "issue",
            "create",
            "--repo",
            "owner/ox-llm",
            "--title",
            "Engine stalls"
        ]
    );
    assert_eq!(posted, body, "the body posted is the body shown");
    assert!(!args.iter().any(|arg| arg == "--body-file"));
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("Posted: https://github.com/owner/ox-llm/issues/1"));
}

/// A fake host tool: a shell script that records `who` in `marker` and prints it.
fn fake_tool(dir: &Path, name: &str, who: &str, marker: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let file = dir.join(name);
    let script = format!("#!/bin/sh\necho {who} >> '{}'\necho {who}\n", marker.display());
    std::fs::write(&file, script).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn the_real_runner_never_runs_a_project_gh_first_on_path() {
    let env = TestEnv::new();
    let project = env.work();
    let marker = env.root().join("ran.log");
    fake_tool(&project, "gh", "project", &marker);
    fake_tool(&env.root().join("tools"), "gh", "trusted", &marker);
    let args = vec!["--version".to_string()];

    // The project's copy comes first on PATH: the trusted one runs instead.
    let path = format!("{}:{}", project.display(), env.root().join("tools").display());
    let vars = Env::from_pairs(&[("PATH", path.as_str())]);
    let home = env.home();
    let result = process_runner(&vars, &project, &home)("gh", &args);
    assert_eq!(result, Some((0, "trusted\n".to_string())));

    // Only the project's copy on PATH: refused, nothing runs.
    let path = project.display().to_string();
    let vars = Env::from_pairs(&[("PATH", path.as_str())]);
    assert_eq!(process_runner(&vars, &project, &home)("gh", &args), None);

    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(ran, "trusted\n", "the project's gh never ran");
}

// ---------------------------------------------------------------------------------------------
// Identity

#[test]
fn identity_facts_from_a_synthetic_machine() {
    let env = TestEnv::new();
    let root = env.root().join("machine");
    let put = |rel: &str, text: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    put("sys/firmware/devicetree/base/model", "Test Laptop 7x\0");
    put(
        "sys/firmware/devicetree/base/compatible",
        "vendor,test\0qcom,x1e80100\0",
    );
    put("proc/meminfo", "MemTotal:       32212254 kB\nMemFree: 1 kB\n");
    put(
        "etc/os-release",
        "NAME=\"Omarchy\"\nPRETTY_NAME=\"Omarchy\"\nID=omarchy\nID_LIKE=arch\n",
    );
    put("etc/hostname", "secret-host\n");
    let f = identity_facts(&root, None);
    assert_eq!(f.machine.as_deref(), Some("Test Laptop 7x"));
    assert_eq!(f.chip.as_deref(), Some("Snapdragon X Elite (X1E80100)"));
    assert_eq!(f.memory_gib, Some(31));
    assert!(f.omarchy);
    assert!(f.models.is_empty(), "no engine = no model list");

    let dirs = Dirs::under(env.root());
    assert!(write_identity(&dirs, &f).unwrap(), "first write");
    assert!(
        !write_identity(&dirs, &f).unwrap(),
        "same facts = the file is untouched"
    );
    let text = read_identity(&dirs).expect("identity");
    assert!(text.contains("- Machine: Test Laptop 7x, Snapdragon X Elite (X1E80100), 31 GiB memory"));
    assert!(text.contains("- System: Omarchy Linux"));
    assert!(text.contains("- Models: not known"));
    assert!(!text.contains("secret-host"), "no host name");

    // A bare tree (another machine, a container): only the version, never a panic.
    let bare = identity_facts(&env.root().join("empty"), None);
    assert_eq!(
        (bare.machine, bare.chip, bare.memory_gib, bare.omarchy),
        (None, None, None, false)
    );
}

#[test]
fn identity_command_prints_the_fallback_and_refuses_unknown_flags() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let mut out = Vec::new();
    assert_eq!(
        run_identity(&[], &dirs, &Env::from_pairs(&[]), &mut out).unwrap(),
        0
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("No identity installed") && text.contains("oxa identity --refresh"));
    let err = run_identity(
        &["--bogus".to_string()],
        &dirs,
        &Env::from_pairs(&[]),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(err.code, oxa_proto::codes::USAGE);
    assert!(!Path::new(&dirs.agent_dir).join("identity.md").exists());
}
