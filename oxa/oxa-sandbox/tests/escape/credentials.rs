//! Dotfiles and credentials: the deny-list must hold for bash in every mode.

use crate::harness::{assert_refused, home_file, run, sandbox, sandbox_with_layout, write};
use oxa_fake::TestEnv;
use oxa_sandbox::Layout;

const SECRET: &str = "SYNTHETIC-PRIVATE-MATERIAL-42";

fn plant(env: &TestEnv) {
    write(&home_file(env, ".ssh/id_test"), SECRET);
    write(&home_file(env, ".aws/credentials"), SECRET);
    write(&home_file(env, ".config/oxc/cloud-secrets.json"), SECRET);
    write(&home_file(env, ".netrc"), SECRET);
    write(&env.agent_dir().join("trust.json"), SECRET);
    write(&home_file(env, ".config/gh/hosts.yml"), SECRET);
}

/// Threat: a steered model reads SSH, cloud, git and agent credentials through bash.
/// Attempt: `cat` each path, then `ls` the directory that holds it.
/// Expected refusal: every read fails and no byte of the secret reaches the output.
#[test]
fn keys_tokens_and_agent_state_are_masked() {
    let env = TestEnv::new();
    plant(&env);
    let Some(sb) = sandbox(&env) else { return };
    for rel in [
        ".ssh/id_test",
        ".aws/credentials",
        ".config/oxc/cloud-secrets.json",
        ".netrc",
        ".config/gh/hosts.yml",
    ] {
        let p = home_file(&env, rel);
        let cmd = format!("cat {}", p.display());
        let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
        assert_refused(exit, &out, rel);
        assert!(!out.contains(SECRET), "{rel} leaked: {out}");
    }
    let cmd = format!("cat {}/trust.json", env.agent_dir().display());
    let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
    assert_refused(exit, &out, "the agent directory");
    assert!(!out.contains(SECRET), "agent state leaked: {out}");
}

/// Threat: a home workspace (bash started in `$HOME`) makes dotfiles writable.
/// Attempt: append to `~/.bashrc`, create `~/.profile`, and write into `~/.ssh`.
/// Expected refusal: the home root and every dot-entry stay read-only; the writes fail.
#[test]
fn home_workspace_dotfiles_stay_read_only() {
    let env = TestEnv::new();
    write(&home_file(&env, ".bashrc"), "export SAFE=1\n");
    write(&home_file(&env, ".ssh/id_test"), SECRET);
    write(&home_file(&env, "project/keep"), "keep\n");
    let layout = Layout {
        allow_home_write: true,
        ..Layout::default()
    };
    let Some(sb) = sandbox_with_layout(&env, layout) else {
        return;
    };
    for cmd in [
        "printf 'PWN\\n' >> .bashrc",
        "printf 'PWN\\n' > .profile",
        "printf 'PWN\\n' > .ssh/authorized_keys",
        "printf 'PWN\\n' > .config/autostart/x.desktop",
    ] {
        let (exit, out) = run(&sb, &env.home(), cmd, true, false);
        assert_refused(exit, &out, cmd);
    }
    assert_eq!(
        std::fs::read_to_string(home_file(&env, ".bashrc")).expect("bashrc"),
        "export SAFE=1\n"
    );
    assert!(!home_file(&env, ".profile").exists());
    assert!(!home_file(&env, ".ssh/authorized_keys").exists());
    // Control: the plain project folder is the writable part of a home workspace.
    let (exit, out) = run(&sb, &env.home(), "printf ok > project/created", true, false);
    assert_eq!(exit, Some(0), "{out}");
    assert_eq!(
        std::fs::read_to_string(home_file(&env, "project/created")).expect("created"),
        "ok"
    );
}

/// Threat: a project symlink points at a credential and bash follows it.
/// Attempt: `cat leak/id_test`, once relative and once absolute.
/// Expected refusal: the home tmpfs and the deny-list masks win; no secret is read.
#[test]
fn symlinks_to_credentials_do_not_leave_the_workspace() {
    let env = TestEnv::new();
    write(&home_file(&env, ".ssh/id_test"), SECRET);
    let work = env.work();
    std::os::unix::fs::symlink("../../home/.ssh", work.join("relative-leak")).expect("symlink");
    std::os::unix::fs::symlink(home_file(&env, ".ssh"), work.join("absolute-leak")).expect("symlink");
    let Some(sb) = sandbox(&env) else { return };
    for link in ["relative-leak", "absolute-leak"] {
        let cmd = format!("cat {link}/id_test");
        let (exit, out) = run(&sb, &work, &cmd, true, false);
        assert_refused(exit, &out, link);
        assert!(!out.contains(SECRET), "{link} leaked: {out}");
    }
}
