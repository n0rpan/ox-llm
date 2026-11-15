//! File names are data: newlines, control characters and option-looking names must not turn
//! into extra shell words or extra mount arguments.

use crate::harness::{assert_ok, run, sandbox, write};
use oxa_fake::TestEnv;

/// Threat: a file name with a newline or `;` injects a second shell command.
/// Attempt: create and read `line\nbreak`, `x; touch PWNED`, `-rf` and a control byte.
/// Expected refusal: the shell sees one quoted word; only the literal file is touched.
#[test]
fn control_characters_in_names_do_not_become_commands() {
    let env = TestEnv::new();
    let work = env.work();
    for name in ["line\nbreak", "x; touch PWNED", "-rf", "bell\u{7}name"] {
        write(&work.join(name), "data\n");
    }
    let Some(sb) = sandbox(&env) else { return };
    for name in ["line\nbreak", "x; touch PWNED", "-rf", "bell\u{7}name"] {
        let cmd = format!("cat -- {}", crate::harness::sh(name));
        let (exit, out) = run(&sb, &work, &cmd, true, false);
        assert_ok(exit, &out, name);
        assert_eq!(out, "data\n", "{name:?}");
    }
    assert!(!work.join("PWNED").exists(), "the semicolon ran a second command");
    // Writing the same names through the shell keeps them literal too.
    let (exit, out) = run(&sb, &work, "printf next > 'x; touch PWNED2'", true, false);
    assert_ok(exit, &out, "write a semicolon name");
    assert!(!work.join("PWNED2").exists());
    assert_eq!(
        std::fs::read_to_string(work.join("x; touch PWNED2")).expect("literal file"),
        "next"
    );
}

/// Threat: a cwd or a file name with a newline adds bwrap options (`--bind / /`).
/// Attempt: build the argv for a cwd named `line\nbreak` and read the arguments.
/// Expected refusal: the name is one argument; no option is split out of it.
#[test]
fn a_newline_in_a_path_stays_one_bwrap_argument() {
    let env = TestEnv::new();
    let cwd = env.work().join("a\nb");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let s = oxa_proto::ExecSpec {
        command: "true".into(),
        cwd: cwd.clone(),
        writable: true,
        net: false,
        timeout_s: None,
        env: Vec::new(),
    };
    let args = oxa_sandbox::bwrap_args(&s, &env.home());
    let cwd_s = cwd.to_string_lossy().into_owned();
    assert!(
        args.windows(2).any(|w| w[0] == "--chdir" && w[1] == cwd_s),
        "{args:?}"
    );
    assert!(
        args.iter().filter(|a| a.contains('\n')).all(|a| a == &cwd_s),
        "a path with a newline was split: {args:?}"
    );
}
