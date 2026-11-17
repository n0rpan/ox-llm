//! #868: side-effect-free subcommand help and `oxa help <command>`.
//!
//! Table-driven, against the REAL `oxa` binary in an isolated HOME. Every public command and
//! nested verb answers `--help` with usage, one synthetic example and exit 0; `oxa help
//! <command>` resolves the same text. Help is answered before the working directory, the
//! network guard, the trust store, settings, resources, package execution or sign-in, so a
//! help run makes zero loopback requests, launches nothing and changes no file. Destructive
//! commands (`wipe`, `work`, `login`) get explicit negative side-effect controls.

use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Run the real binary with the isolated environment of `env` and no terminal.
fn run(env: &TestEnv, url: &str, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(url))
        .stdin(Stdio::null())
        .output()
        .expect("run oxa");
    Run {
        code: out.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Every file and directory below `root`, as relative path -> bytes (a directory maps to an
/// empty value). Used to prove a help run changes nothing.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
            if path.is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(&path, root, out);
            } else {
                out.insert(rel, fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Every public command path of this build and the first line its help must start with.
/// Mirrors the `COMMANDS` table of `oxa-cli/src/args.rs` plus the nested public verbs.
fn commands() -> Vec<Vec<&'static str>> {
    let base: Vec<Vec<&'static str>> = vec![
        vec!["status"],
        vec!["install"],
        vec!["remove"],
        vec!["uninstall"],
        vec!["update"],
        vec!["list"],
        vec!["login"],
        vec!["login", "chatgpt"],
        vec!["logout"],
        vec!["logout", "chatgpt"],
        vec!["send"],
        vec!["config"],
        vec!["mcp"],
        vec!["mcp", "add"],
        vec!["mcp", "remove"],
        vec!["mcp", "list"],
        vec!["mcp", "login"],
        vec!["mcp", "logout"],
        vec!["identity"],
        vec!["report-issue"],
        vec!["report-issue", "draft"],
        vec!["report-issue", "submit"],
        vec!["work"],
        vec!["work", "status"],
        vec!["wipe"],
    ];
    #[cfg(not(feature = "share"))]
    {
        base
    }
    #[cfg(feature = "share")]
    {
        let mut all = base;
        all.extend([
            vec!["share"],
            vec!["share", "key"],
            vec!["share", "key", "add"],
            vec!["share", "key", "list"],
            vec!["share", "key", "revoke"],
        ]);
        all
    }
}

/// Every public command answers `--help` and `oxa help <command>` with the same text, exit 0,
/// no loopback request and no file change.
#[test]
fn every_public_command_has_side_effect_free_help() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let before = tree(env.root());
    let mut failures = Vec::new();
    for path in commands() {
        let label = path.join(" ");
        let mut flag_args: Vec<&str> = path.clone();
        flag_args.push("--help");
        let mut help_args: Vec<&str> = vec!["help"];
        help_args.extend(path.iter().copied());

        let flag = run(&env, &server.url(), &flag_args);
        let help = run(&env, &server.url(), &help_args);
        let expected = format!("oxa {label} - ");
        for (how, got) in [("--help", &flag), ("help", &help)] {
            if got.code != 0 {
                failures.push(format!(
                    "`oxa {label}` {how}: exit {} ({})",
                    got.code,
                    got.err.trim()
                ));
                continue;
            }
            if !got.out.starts_with(&expected) {
                failures.push(format!(
                    "`oxa {label}` {how}: first line {:?}",
                    got.out.lines().next()
                ));
            }
            if !got.out.contains("Usage:\n") || !got.out.contains("Example:\n") {
                failures.push(format!("`oxa {label}` {how}: no usage or example"));
            }
            if !got.err.is_empty() {
                failures.push(format!("`oxa {label}` {how}: stderr {:?}", got.err));
            }
        }
        if flag.out != help.out {
            failures.push(format!("`oxa {label}`: `--help` and `help` differ"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
    assert_eq!(tree(env.root()), before, "help changed a file");
}

/// `oxa help` with no command prints the top-level usage, like `oxa --help`.
#[test]
fn bare_help_prints_the_top_level_usage() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let before = tree(env.root());
    let bare = run(&env, &server.url(), &["help"]);
    assert_eq!(bare.code, 0, "stderr: {}", bare.err);
    assert!(bare.out.contains("Usage:"), "{}", bare.out);
    assert!(bare.out.contains("Commands:"), "{}", bare.out);
    assert!(bare.out.contains("oxa help [command]"), "{}", bare.out);
    let flag = run(&env, &server.url(), &["--help"]);
    assert_eq!(flag.code, 0, "stderr: {}", flag.err);
    assert_eq!(bare.out, flag.out, "`oxa help` and `oxa --help` differ");
    // The `help` command itself is a COMMANDS entry: `oxa help --help` is the same text.
    let self_help = run(&env, &server.url(), &["help", "--help"]);
    assert_eq!(self_help.code, 0, "stderr: {}", self_help.err);
    assert_eq!(self_help.out, bare.out, "`oxa help --help` differs");
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
    assert_eq!(tree(env.root()), before, "help changed a file");
}

/// An unknown command is a clear usage error (exit 2), not a silent fallback.
#[test]
fn unknown_help_target_is_a_usage_error() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let before = tree(env.root());
    for words in [
        vec!["help", "frobnicate"],
        vec!["help", "mcp", "frobnicate"],
        vec!["auth", "--help"],
    ] {
        let got = run(&env, &server.url(), &words);
        assert_eq!(got.code, 2, "`oxa {}`: {}", words.join(" "), got.err);
        assert!(got.out.is_empty(), "`oxa {}` wrote stdout", words.join(" "));
        assert!(!got.err.is_empty(), "`oxa {}` had no diagnostic", words.join(" "));
    }
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
    assert_eq!(tree(env.root()), before, "help changed a file");
}

/// A build without the `share` feature refuses `oxa share` with a clear usage error.
#[cfg(not(feature = "share"))]
#[test]
fn feature_gated_help_reflects_the_build() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let flag = run(&env, &server.url(), &["share", "--help"]);
    assert_eq!(flag.code, 2, "`oxa share --help`: {}", flag.err);
    assert!(flag.err.contains("sharing"), "`oxa share --help`: {}", flag.err);
    let help = run(&env, &server.url(), &["help", "share"]);
    assert_eq!(help.code, 2, "`oxa help share`: {}", help.err);
    assert!(
        help.err.contains("no help for `oxa share`"),
        "`oxa help share`: {}",
        help.err
    );
}

/// `oxa wipe --all --yes --help` must not delete anything: help is answered first.
#[test]
fn wipe_help_does_not_delete_sessions() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let session = env.write(
        "home/.oxa/agent/sessions/--work-proj--/2026-11-01T10-00-00-000Z_d0c1-first.jsonl",
        "{\"type\":\"session\",\"version\":3,\"id\":\"d0c1-first\",\"timestamp\":\"2026-11-01T10:00:00.000Z\",\"cwd\":\"/work/proj\"}\n",
    );
    env.write("home/.oxa/agent/tmp/d0c1-first/bash-1.log", "spill");
    let before = tree(env.root());
    for words in [
        vec!["wipe", "--help"],
        vec!["wipe", "--all", "--yes", "--help"],
        vec!["help", "wipe"],
    ] {
        let got = run(&env, &server.url(), &words);
        assert_eq!(got.code, 0, "`oxa {}`: {}", words.join(" "), got.err);
        assert!(got.out.contains("Usage:\n"), "`oxa {}`", words.join(" "));
    }
    assert!(session.exists(), "the session file was deleted");
    assert_eq!(tree(env.root()), before, "wipe help changed a file");
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
}

/// `oxa work --help` outside a Git repository must not fail on the repository check.
#[test]
fn work_help_does_not_require_a_git_repository() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    assert!(
        !env.root().ancestors().any(|dir| dir.join(".git").exists()),
        "the fixture must live outside a Git repository"
    );
    let before = tree(env.root());
    for words in [
        vec!["work", "--help"],
        vec!["work", "status", "--help"],
        vec!["help", "work"],
    ] {
        let got = run(&env, &server.url(), &words);
        assert_eq!(got.code, 0, "`oxa {}`: {}", words.join(" "), got.err);
        assert!(got.out.contains("Usage:\n"), "`oxa {}`", words.join(" "));
    }
    assert_eq!(tree(env.root()), before, "work help changed a file");
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
}

/// `oxa login chatgpt --help` must not start a sign-in, write a token or reach the network.
#[test]
fn login_help_does_not_sign_in() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let before = tree(env.root());
    for words in [
        vec!["login", "chatgpt", "--help"],
        vec!["logout", "chatgpt", "--help"],
        vec!["help", "login", "chatgpt"],
    ] {
        let got = run(&env, &server.url(), &words);
        assert_eq!(got.code, 0, "`oxa {}`: {}", words.join(" "), got.err);
        assert!(got.out.contains("Usage:\n"), "`oxa {}`", words.join(" "));
        assert!(
            !got.out.contains("http://") && !got.out.contains("https://"),
            "`oxa {}` printed a sign-in URL: {}",
            words.join(" "),
            got.out
        );
    }
    assert_eq!(tree(env.root()), before, "login help changed a file");
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
}

/// Help runs no subprocess: `PATH` is replaced by a directory of trap programs that would
/// record their own invocation, and every help run still exits 0 and leaves no trace.
#[cfg(unix)]
#[test]
fn help_runs_no_subprocess() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let bin = env.root().join("trap-bin");
    fs::create_dir_all(&bin).expect("trap directory");
    let log = env.root().join("trap.log");
    let script = format!("#!/bin/sh\necho \"$0 $*\" >> {}\nexit 0\n", log.display());
    for name in ["git", "gh", "bwrap", "curl", "wget", "sh", "bash", "open"] {
        let path = bin.join(name);
        fs::write(&path, &script).expect("write trap program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make it executable");
    }
    for words in [
        vec!["help"],
        vec!["work", "--help"],
        vec!["wipe", "--help"],
        vec!["login", "chatgpt", "--help"],
        vec!["config", "--help"],
        vec!["install", "https://example.invalid/tools.git", "--help"],
        vec!["mcp", "list", "--help"],
        vec!["report-issue", "draft", "--help"],
        vec!["send", "--help"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(&words)
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()))
            .env("PATH", &bin)
            .stdin(Stdio::null())
            .output()
            .expect("run oxa");
        assert_eq!(
            out.status.code(),
            Some(0),
            "`oxa {}`: {}",
            words.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        !log.exists(),
        "a help run started a subprocess: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
}

/// The commands that used to reject `--help` as an unknown option now answer it.
#[test]
fn previously_rejected_help_flags_now_exit_zero() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let before = tree(env.root());
    for words in [
        vec!["config", "--help"],
        vec!["status", "--help"],
        vec!["send", "--help"],
        vec!["identity", "--help"],
        vec!["report-issue", "--help"],
        vec!["mcp", "--help"],
        vec!["mcp", "add", "--help"],
        vec!["install", "--help"],
        vec!["list", "--help"],
        vec!["update", "--help"],
        vec!["remove", "--help"],
    ] {
        let got = run(&env, &server.url(), &words);
        assert_eq!(got.code, 0, "`oxa {}`: {}", words.join(" "), got.err);
        assert!(got.out.contains("Usage:\n"), "`oxa {}`", words.join(" "));
        assert!(got.err.is_empty(), "`oxa {}`: {}", words.join(" "), got.err);
    }
    assert_eq!(tree(env.root()), before, "help changed a file");
    assert_eq!(server.requests().len(), 0, "help made a loopback request");
}

/// `--` separates oxa's words from the command it stores: `oxa mcp add demo -- server --help`
/// saves that command, exactly as before help detection existed.
#[test]
fn mcp_add_still_saves_a_command_that_contains_a_help_flag() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let got = run(
        &env,
        &server.url(),
        &["mcp", "add", "demo", "--", "server", "--help"],
    );
    assert_eq!(got.code, 0, "oxa mcp add: {}{}", got.out, got.err);
    assert!(
        got.out.contains("Saved MCP server \"demo\""),
        "a child --help flag was taken as oxa help: {}{}",
        got.out,
        got.err
    );
    let saved = fs::read_to_string(env.agent_dir().join("mcp.json")).expect("saved mcp.json");
    assert!(saved.contains("\"server\""), "{saved}");
    assert!(saved.contains("\"--help\""), "{saved}");
    assert_eq!(
        server.requests().len(),
        0,
        "adding an MCP server made a loopback request"
    );
}
