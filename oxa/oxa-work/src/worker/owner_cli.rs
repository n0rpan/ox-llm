//! Command and result boundary for an owner-selected vendor worker.
//! The runner resolves the trusted executable and supplies the leased worktree.
use super::{line, WorkerOutcome, WorkerResult, WorkerTokens};
use crate::Result;
use serde_json::Value;
use std::io::{BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// This launch selection belongs to the owner. It is not part of a task packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerCli {
    Claude(ClaudePermission),
    Codex(CodexPermission),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudePermission {
    Default,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
    SkipAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexPermission {
    Sandbox {
        mode: CodexSandbox,
        approval: CodexApproval,
    },
    SkipAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexApproval {
    OnRequest,
    Never,
}

/// Structured CLI evidence for one invocation. Worktree and done checks still
/// determine whether the board can accept its work.
pub struct Invocation {
    terminal: Option<Terminal>,
    invalid: bool,
}

struct Terminal {
    success: bool,
    tokens: Option<WorkerTokens>,
}

impl OwnerCli {
    /// Build argv directly. The caller supplies a trusted executable and the
    /// existing runner remains responsible for environment and process ownership.
    pub fn command(self, executable: &Path, worktree: &Path, model: &str, prompt: &str) -> Result<Command> {
        if !executable.is_absolute() || !worktree.is_absolute() {
            return Err("vendor executable and worktree must be absolute".into());
        }
        if !model.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
        {
            return Err("invalid vendor model identifier".into());
        }

        let mut command = Command::new(executable);
        command.current_dir(worktree);
        match self {
            Self::Claude(permission) => {
                let mode = match permission {
                    ClaudePermission::Default => "default",
                    ClaudePermission::AcceptEdits => "acceptEdits",
                    ClaudePermission::Plan => "plan",
                    ClaudePermission::Auto => "auto",
                    ClaudePermission::DontAsk => "dontAsk",
                    ClaudePermission::SkipAll => "bypassPermissions",
                };
                command.args([
                    "-p",
                    "--verbose",
                    "--output-format",
                    "stream-json",
                    "--permission-mode",
                    mode,
                ]);
            }
            Self::Codex(permission) => {
                if let CodexPermission::Sandbox { approval, .. } = permission {
                    let approval = match approval {
                        CodexApproval::OnRequest => "on-request",
                        CodexApproval::Never => "never",
                    };
                    command.args(["--ask-for-approval", approval]);
                }
                command.args(["exec", "--json"]);
                match permission {
                    CodexPermission::Sandbox { mode, .. } => {
                        let mode = match mode {
                            CodexSandbox::ReadOnly => "read-only",
                            CodexSandbox::WorkspaceWrite => "workspace-write",
                            CodexSandbox::FullAccess => "danger-full-access",
                        };
                        command.args(["--sandbox", mode]);
                    }
                    CodexPermission::SkipAll => {
                        command.arg("--dangerously-bypass-approvals-and-sandbox");
                    }
                }
            }
        }
        command.args(["--model", model, "--", prompt]);
        Ok(command)
    }

    /// Drain stdout using the existing worker's line cap. Keep only terminal
    /// outcome and measurements, with no transcript or provider diagnostic.
    pub fn read_invocation(self, stdout: impl Read) -> Result<Invocation> {
        let mut reader = BufReader::new(stdout);
        let mut terminal = None;
        let mut invalid = false;
        while let Some(bytes) = line(&mut reader)? {
            let Ok(event) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let success = match (self, event["type"].as_str()) {
                (Self::Claude(_), Some("result")) => {
                    match (event["subtype"].as_str(), event["is_error"].as_bool()) {
                        (Some("success"), Some(false)) => true,
                        (_, Some(true)) => false,
                        _ => {
                            invalid = true;
                            continue;
                        }
                    }
                }
                (Self::Codex(_), Some("turn.completed")) => true,
                (Self::Codex(_), Some("turn.failed")) => false,
                _ => continue,
            };
            let tokens = self.tokens(&event["usage"], success);
            invalid |= terminal.replace(Terminal { success, tokens }).is_some();
        }
        Ok(Invocation { terminal, invalid })
    }

    fn tokens(self, usage: &Value, success: bool) -> Option<WorkerTokens> {
        let (prompt, cached) = match self {
            Self::Claude(_) => {
                let cached = usage["cache_read_input_tokens"].as_u64()?;
                let created = usage["cache_creation_input_tokens"].as_u64()?;
                (
                    usage["input_tokens"]
                        .as_u64()?
                        .checked_add(cached)?
                        .checked_add(created)?,
                    cached,
                )
            }
            Self::Codex(_) => (
                usage["input_tokens"].as_u64()?,
                usage["cached_input_tokens"].as_u64()?,
            ),
        };
        (cached <= prompt).then_some(WorkerTokens {
            prompt: Some(prompt),
            completion: Some(usage["output_tokens"].as_u64()?),
            cached: Some(cached),
            complete: success,
            unreported_requests: None,
        })
    }
}

impl Invocation {
    pub fn finish(self, status: ExitStatus) -> WorkerResult {
        let exit_code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(9));
        let terminal = self.terminal.filter(|_| !self.invalid);
        let error_class = match &terminal {
            None => Some("vendor_worker_receipt"),
            Some(result) if !status.success() || !result.success => Some("vendor_worker_failed"),
            Some(_) => None,
        };
        let mut tokens = terminal.and_then(|result| result.tokens);
        if let Some(tokens) = &mut tokens {
            tokens.complete &= error_class.is_none();
        }
        WorkerResult {
            outcome: if error_class.is_none() {
                WorkerOutcome::Done
            } else {
                WorkerOutcome::Failed
            },
            exit_code,
            error_class: error_class.map(str::to_owned),
            interruption: None,
            tokens,
        }
    }
}

#[cfg(test)]
mod tests;
