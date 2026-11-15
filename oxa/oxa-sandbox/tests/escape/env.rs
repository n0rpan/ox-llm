//! Environment: the sandbox starts from an empty environment and passes only toolchain
//! variables whose names are not secret-shaped.

use crate::harness::{assert_ok, assert_refused, run, sandbox};
use oxa_fake::TestEnv;
use oxa_sandbox::{bwrap_args, Layout, LayoutInputs};

const SECRET: &str = "SYNTHETIC-ENV-SECRET-42";

/// Threat: the parent process's API keys and tokens appear in a bash command's environment.
/// Attempt: `printenv` a canary variable set in the test process, and `env` for every variable.
/// Expected refusal: bwrap's `--clearenv` drops the parent environment; `printenv` fails.
#[test]
fn parent_environment_is_not_inherited() {
    let env = TestEnv::new();
    std::env::set_var("OXA_ESCAPE_ENV_CANARY", SECRET);
    let Some(sb) = sandbox(&env) else {
        std::env::remove_var("OXA_ESCAPE_ENV_CANARY");
        return;
    };
    let (exit, out) = run(&sb, &env.work(), "printenv OXA_ESCAPE_ENV_CANARY", true, false);
    assert_refused(exit, &out, "the parent environment");
    let (exit, out) = run(&sb, &env.work(), "env", true, false);
    assert_ok(exit, &out, "env");
    std::env::remove_var("OXA_ESCAPE_ENV_CANARY");
    assert!(!out.contains(SECRET), "the canary leaked: {out}");
}

/// Threat: `Layout::build` passes a toolchain variable that is actually a token
/// (`MISE_GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`).
/// Attempt: build a layout from an environment that holds a real toolchain variable and two
/// secret-shaped ones, then inspect the argv.
/// Expected refusal: only the non-secret toolchain variable is passed, and the secret string
/// appears nowhere in the argv.
#[test]
fn secret_shaped_toolchain_variables_are_dropped() {
    let env = TestEnv::new();
    let layout = Layout::build(&LayoutInputs {
        home: env.home(),
        agent_dir: Some(env.agent_dir()),
        path_var: "/usr/bin:/bin".to_string(),
        env: vec![
            (
                "CARGO_HOME".to_string(),
                env.home().join(".cargo").display().to_string(),
            ),
            ("MISE_GITHUB_TOKEN".to_string(), SECRET.to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), SECRET.to_string()),
            (
                "NPM_CONFIG_CACHE".to_string(),
                env.home().join(".npm").display().to_string(),
            ),
        ],
        ..LayoutInputs::default()
    });
    assert!(layout.env.iter().any(|(k, _)| k == "CARGO_HOME"));
    assert!(layout.env.iter().any(|(k, _)| k == "NPM_CONFIG_CACHE"));
    assert!(!layout.env.iter().any(|(k, _)| k.contains("TOKEN")));
    let s = oxa_proto::ExecSpec {
        command: "true".into(),
        cwd: env.work(),
        writable: true,
        net: false,
        timeout_s: None,
        env: Vec::new(),
    };
    let args = bwrap_args(&s, &env.home());
    assert!(!args.iter().any(|a| a.contains(SECRET)), "{args:?}");
}
