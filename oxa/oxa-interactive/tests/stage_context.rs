//! The tool-event signals (plan C.2, slice X1): table-driven and pure. Public API only.
use oxa_interactive::skin::stage::context::{
    err_class, exit_class, lang_of_path, pr_class, pr_from_branch, signals_of, ToolEvent,
};

fn bash(command: &str) -> ToolEvent<'_> {
    ToolEvent {
        tool: "bash",
        command: Some(command),
        ..Default::default()
    }
}

#[test]
fn edit_of_a_rust_file_is_lang_rs() {
    let edit = ToolEvent {
        tool: "edit",
        path: Some("~/project/oxa/oxa-core/src/lib.rs"),
        ..Default::default()
    };
    assert_eq!(signals_of(&edit).lang, Some("rs"));
    assert_eq!(lang_of_path("~/project/.config/hypr/hyprland.conf"), Some("hypr"));
    assert_eq!(lang_of_path("~/project/Dockerfile"), Some("docker"));
    assert_eq!(lang_of_path("~/project/PKGBUILD"), Some("pkgbuild"));
    assert_eq!(lang_of_path("~/project/notes.unknownext"), None);
    assert_eq!(lang_of_path("~/project/.bashrc"), None);
    // Only edit, write and read carry a language; a bash call has none.
    assert_eq!(signals_of(&bash("cat ~/project/src/lib.rs")).lang, None);
}

#[test]
fn cargo_test_is_cmd_cargo_test() {
    assert_eq!(
        signals_of(&bash("cargo test -p oxa-core")).cmd,
        Some("cargo-test")
    );
    assert_eq!(signals_of(&bash("cargo nextest run")).cmd, Some("cargo-test"));
    assert_eq!(signals_of(&bash("FOO=1 cargo build")).cmd, Some("cargo-build"));
    assert_eq!(signals_of(&bash("python3 -m pytest -q")).cmd, Some("pytest"));
    assert_eq!(signals_of(&bash("ls -la")).cmd, None);
}

#[test]
fn git_rebase_is_seen_in_a_compound_command() {
    let rebase = signals_of(&bash("cd ~/project && git -C ~/project rebase -i origin/main"));
    assert_eq!(rebase.git, Some("rebase"));
    let forced = signals_of(&bash("git fetch origin && git push --force-with-lease"));
    assert_eq!(forced.git, Some("force-push"));
    assert_eq!(signals_of(&bash("git push origin main")).git, None);
    assert_eq!(signals_of(&bash("echo ok; git stash pop")).git, Some("stash"));
    // A redirection is not a separator: the git call is still the first simple command.
    assert_eq!(signals_of(&bash("git merge main 2>&1")).git, Some("merge"));
}

#[test]
fn exit_127_and_stderr_tables_map() {
    assert_eq!(exit_class(127), Some("127"));
    assert_eq!(exit_class(137), Some("137"));
    assert_eq!(exit_class(1), None);
    assert_eq!(
        err_class("error[E0499]: cannot borrow `x` as mutable"),
        Some("borrow")
    );
    assert_eq!(err_class("Traceback (most recent call last):"), Some("traceback"));
    assert_eq!(err_class("write failed: No space left on device"), Some("enospc"));
    assert_eq!(err_class("HTTP 429 Too Many Requests"), Some("http-429"));
    assert_eq!(err_class("bash: foo: command not found"), None);
    // Only the first 2 KiB of a result are read.
    let late = format!("{}permission denied", "x".repeat(3000));
    assert_eq!(err_class(&late), None);

    let failed = ToolEvent {
        tool: "bash",
        command: Some("cargo test"),
        exit_code: Some(127),
        output: Some("bash: cargo: command not found"),
        ..Default::default()
    };
    let signals = signals_of(&failed);
    assert_eq!(signals.exit, Some("127"));
    assert_eq!(signals.err, None);
}

#[test]
fn pr_number_classes() {
    assert_eq!(pr_class(404), Some("404"));
    assert_eq!(pr_class(1337), Some("1337"));
    assert_eq!(pr_class(1200), Some("round"));
    assert_eq!(pr_class(1221), Some("palindrome"));
    assert_eq!(pr_class(256), Some("power-of-two"));
    assert_eq!(pr_class(123), None);
    assert_eq!(pr_class(1), None);
    assert_eq!(pr_class(0), None);
    assert_eq!(pr_from_branch("agent/1586-animations-context"), Some(1586));
    assert_eq!(pr_from_branch("agent/1586"), Some(1586));
    assert_eq!(pr_from_branch("agent/12x-slug"), None);
    assert_eq!(pr_from_branch("main"), None);
}

#[test]
fn signals_never_carry_text_from_the_tool() {
    let event = ToolEvent {
        tool: "bash",
        command: Some("cargo test --token=SECRET123 && echo /root/SECRET123"),
        exit_code: Some(139),
        output: Some("Permission denied: /root/SECRET123 out of memory"),
        pr_number: Some(404),
        ..Default::default()
    };
    let pairs = signals_of(&event).pairs();
    assert_eq!(
        pairs,
        vec![
            ("cmd", "cargo-test"),
            ("exit", "139"),
            ("err", "eacces"),
            ("pr_is", "404"),
        ]
    );
    for (key, value) in &pairs {
        assert!(!value.contains("SECRET"), "{key} leaked text: {value}");
        assert!(!value.contains('/') && !value.contains(' ') && !value.contains('='));
    }
}
