use super::*;
use crate::test_dir::PrivateDir;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

fn fake_run(cli: OwnerCli, model: &str, script: &str) -> WorkerResult {
    let fixture = PrivateDir::new("vendor-worker");
    let worktree = fixture.path().join("task");
    fs::create_dir(&worktree).unwrap();
    fs::write(worktree.join("leased-worktree"), b"synthetic").unwrap();
    let executable = fixture.path().join("fake-vendor");
    fs::write(&executable, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
    fs::set_permissions(&executable, Permissions::from_mode(0o700)).unwrap();

    let mut child = cli
        .command(&executable, &worktree, model, "-- literal prompt with spaces")
        .unwrap()
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let facts = cli.read_invocation(child.stdout.take().unwrap()).unwrap();
    facts.finish(child.wait().unwrap())
}

#[test]
fn claude_runs_the_chosen_model_and_mode_in_the_task_worktree() {
    let worker = fake_run(
        OwnerCli::Claude(ClaudePermission::SkipAll),
        "fable",
        r#"
test -f leased-worktree
test "$#" = 10
test "$1" = -p
test "$2" = --verbose
test "$3" = --output-format
test "$4" = stream-json
test "$5" = --permission-mode
test "$6" = bypassPermissions
test "$7" = --model
test "$8" = fable
test "$9" = --
test "${10}" = '-- literal prompt with spaces'
printf '%s\n' '{"type":"assistant","message":{"content":"unretained synthetic progress"}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":6,"cache_read_input_tokens":5,"cache_creation_input_tokens":4,"output_tokens":3}}'
"#,
    );
    assert!(worker.accepted());
    assert_eq!(
        worker.tokens,
        Some(WorkerTokens {
            prompt: Some(15),
            completion: Some(3),
            cached: Some(5),
            complete: false,
            unreported_requests: None,
        })
    );
}

#[test]
fn codex_preserves_owner_sandbox_and_passes_the_argument_boundary() {
    let worker = fake_run(
        OwnerCli::Codex(CodexPermission::Sandbox {
            mode: CodexSandbox::WorkspaceWrite,
            approval: CodexApproval::Never,
        }),
        "gpt-6.2",
        r#"
test -f leased-worktree
test "$#" = 10
test "$1" = --ask-for-approval
test "$2" = never
test "$3" = exec
test "$4" = --json
test "$5" = --sandbox
test "$6" = workspace-write
test "$7" = --model
test "$8" = gpt-6.2
test "$9" = --
test "${10}" = '-- literal prompt with spaces'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":7,"output_tokens":2,"reasoning_output_tokens":1}}'
"#,
    );
    assert!(worker.accepted());
    assert_eq!(
        worker.tokens,
        Some(WorkerTokens {
            prompt: Some(10),
            completion: Some(2),
            cached: Some(7),
            complete: true,
            unreported_requests: None,
        })
    );
}

#[test]
fn codex_skip_all_is_explicit_and_exit_zero_alone_cannot_succeed() {
    let worker = fake_run(
        OwnerCli::Codex(CodexPermission::SkipAll),
        "gpt-6.2",
        r#"
test "$#" = 7
test "$1" = exec
test "$2" = --json
test "$3" = --dangerously-bypass-approvals-and-sandbox
test "$4" = --model
test "$5" = gpt-6.2
printf '%s\n' '{"type":"turn.failed","error":{"message":"unretained synthetic diagnostic"}}'
"#,
    );
    assert_eq!(worker.exit_code, 0);
    assert!(!worker.accepted());
    assert_eq!(worker.error_class.as_deref(), Some("vendor_worker_failed"));
}

#[test]
fn terminal_and_exit_must_agree_and_invalid_usage_does_not_become_measured() {
    let cli = OwnerCli::Codex(CodexPermission::SkipAll);
    let terminal =
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":2,"output_tokens":3}}"#;
    let reported = |output: &str, code| {
        cli.read_invocation(output.as_bytes())
            .unwrap()
            .finish(ExitStatus::from_raw(code << 8))
    };
    assert!(reported(terminal, 0).accepted());
    assert!(reported(terminal, 0).tokens.is_none());
    assert!(!reported(terminal, 3).accepted());
    assert!(!reported(&format!("{terminal}\n{terminal}"), 0).accepted());
    assert!(!reported("", 0).accepted());
    assert!(cli
        .command(Path::new("/vendor"), Path::new("/task"), "--sandbox", "synthetic")
        .is_err());
}

fn claude_result(receipt: &serde_json::Value) -> WorkerResult {
    OwnerCli::Claude(ClaudePermission::Default)
        .read_invocation(receipt.to_string().as_bytes())
        .unwrap()
        .finish(ExitStatus::from_raw(0))
}

#[test]
fn claude_accepts_the_documented_context_suffix_as_one_argument() {
    let cli = OwnerCli::Claude(ClaudePermission::Default);
    let command = cli
        .command(
            Path::new("/vendor"),
            Path::new("/task"),
            "claude-opus-4-6[1m]",
            "synthetic",
        )
        .unwrap();
    assert_eq!(command.get_args().nth(7).unwrap(), "claude-opus-4-6[1m]");
    for invalid in ["--model[1m]", "opus[1m][1m]", "opus[1m] --verbose"] {
        assert!(cli
            .command(Path::new("/vendor"), Path::new("/task"), invalid, "synthetic")
            .is_err());
    }
}

#[test]
fn claude_preserves_a_terminal_receipt_with_a_long_final_answer() {
    let result = claude_result(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "result": "\\\" context \n".repeat(super::super::LINE_CAP),
        "modelUsage": {"opus": {
            "inputTokens": 3, "outputTokens": 2, "cacheReadInputTokens": 1, "cacheCreationInputTokens": 0
        }}
    }));
    assert!(result.accepted());
    assert_eq!(result.tokens.unwrap().prompt, Some(4));
}

#[test]
fn claude_usage_covers_subagents_or_is_explicitly_partial() {
    let mut receipt = serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "usage": {"input_tokens": 20, "output_tokens": 3, "cache_read_input_tokens": 4,
                  "cache_creation_input_tokens": 5},
        "modelUsage": {
            "opus": {"inputTokens": 20, "outputTokens": 3, "cacheReadInputTokens": 4,
                     "cacheCreationInputTokens": 5},
            "sonnet": {"inputTokens": 1000, "outputTokens": 15, "cacheReadInputTokens": 40,
                       "cacheCreationInputTokens": 10}
        }
    });
    let tokens = claude_result(&receipt).tokens.unwrap();
    assert_eq!(
        (tokens.prompt, tokens.completion, tokens.cached),
        (Some(1079), Some(18), Some(44))
    );
    assert!(tokens.complete);
    receipt["modelUsage"] = serde_json::Value::Null;
    let tokens = claude_result(&receipt).tokens.unwrap();
    assert_eq!(tokens.prompt, Some(29));
    assert!(!tokens.complete);
}
