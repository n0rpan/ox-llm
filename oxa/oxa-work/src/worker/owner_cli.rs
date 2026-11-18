//! Command and result boundary for an owner-selected vendor worker.
//! The runner resolves the trusted executable and supplies the leased worktree.
use super::{line, WorkerOutcome, WorkerResult, WorkerTokens};
use crate::Result;
use serde::Deserialize;
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read};
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
#[derive(Default)]
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
        let base = if matches!(self, Self::Claude(_)) {
            model.strip_suffix("[1m]").unwrap_or(model)
        } else {
            model
        };
        if !base.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            || !base
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

    /// Keep only terminal outcome and measurements, with no transcript or provider diagnostic.
    pub fn read_invocation(self, stdout: impl Read) -> Result<Invocation> {
        let mut reader = BufReader::new(stdout);
        if matches!(self, Self::Claude(_)) {
            return read_claude(&mut reader);
        }
        let mut invocation = Invocation::default();
        while let Some(bytes) = line(&mut reader)? {
            let Ok(event) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let success = match event["type"].as_str() {
                Some("turn.completed") => true,
                Some("turn.failed") => false,
                _ => continue,
            };
            let usage = &event["usage"];
            let tokens = counts(usage, "input_tokens", "output_tokens", "cached_input_tokens")
                .filter(|counts| counts.cached <= counts.prompt)
                .map(|counts| counts.tokens(success));
            invocation.record(Terminal { success, tokens });
        }
        Ok(invocation)
    }
}

// Claude embeds the final answer inside its result record. Project out the
// fields we need without retaining the answer, and retain the line boundary
// after an invalid or oversized record so it cannot consume the next event.
const CLAUDE_RECORD_LIMIT: u64 = 8 * 1024 * 1024;

struct JsonLine<'a, R>(&'a mut R);

impl<R: BufRead> Read for JsonLine<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let input = self.0.fill_buf()?;
        let input = &input[..input.len().min(out.len())];
        let count = input
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(input.len());
        out[..count].copy_from_slice(&input[..count]);
        self.0.consume(count);
        Ok(count)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeEvent {
    #[serde(rename = "type")]
    kind: ClaudeKind,
    #[serde(default)]
    subtype: String,
    #[serde(default, rename = "is_error")]
    is_error: Option<bool>,
    #[serde(default)]
    usage: Value,
    #[serde(default)]
    model_usage: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaudeKind {
    Result,
    #[serde(other)]
    Progress,
}

fn read_claude(reader: &mut impl BufRead) -> Result<Invocation> {
    let mut invocation = Invocation::default();
    while !reader
        .fill_buf()
        .map_err(|_| "cannot read vendor stdout")?
        .is_empty()
    {
        let event =
            serde_json::from_reader::<_, ClaudeEvent>(JsonLine(&mut *reader).take(CLAUDE_RECORD_LIMIT));
        let at_end = reader
            .fill_buf()
            .map_err(|_| "cannot read vendor stdout")?
            .first()
            .is_none_or(|&byte| byte == b'\n');
        reader
            .skip_until(b'\n')
            .map_err(|_| "cannot drain vendor stdout")?;
        if !at_end {
            continue;
        }
        let Ok(event) = event else { continue };
        if !matches!(event.kind, ClaudeKind::Result) {
            continue;
        }
        let success = match (event.subtype.as_str(), event.is_error) {
            ("success", Some(false)) => true,
            (_, Some(true)) => false,
            _ => {
                invocation.invalid = true;
                continue;
            }
        };
        let tokens = claude_totals(&event.model_usage)
            .map(|totals| totals.tokens(success))
            .or_else(|| {
                // Top-level usage excludes delegated calls. Retain it only as partial evidence.
                claude_counts(
                    &event.usage,
                    "input_tokens",
                    "output_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                )
                .map(|counts| counts.tokens(false))
            });
        invocation.record(Terminal { success, tokens });
    }
    Ok(invocation)
}

#[derive(Default)]
struct Counts {
    prompt: u64,
    completion: u64,
    cached: u64,
}

impl Counts {
    fn tokens(self, complete: bool) -> WorkerTokens {
        WorkerTokens {
            prompt: Some(self.prompt),
            completion: Some(self.completion),
            cached: Some(self.cached),
            complete,
            unreported_requests: None,
        }
    }
}

fn counts(usage: &Value, input: &str, output: &str, cache_read: &str) -> Option<Counts> {
    Some(Counts {
        prompt: usage[input].as_u64()?,
        completion: usage[output].as_u64()?,
        cached: usage[cache_read].as_u64()?,
    })
}

fn claude_counts(
    usage: &Value,
    input: &str,
    output: &str,
    cache_read: &str,
    cache_create: &str,
) -> Option<Counts> {
    let mut counts = counts(usage, input, output, cache_read)?;
    counts.prompt = counts
        .prompt
        .checked_add(counts.cached)?
        .checked_add(usage[cache_create].as_u64()?)?;
    Some(counts)
}

fn claude_totals(usage: &Value) -> Option<Counts> {
    let models = usage.as_object().filter(|models| !models.is_empty())?;
    models.values().try_fold(Counts::default(), |totals, model| {
        let model = claude_counts(
            model,
            "inputTokens",
            "outputTokens",
            "cacheReadInputTokens",
            "cacheCreationInputTokens",
        )?;
        Some(Counts {
            prompt: totals.prompt.checked_add(model.prompt)?,
            completion: totals.completion.checked_add(model.completion)?,
            cached: totals.cached.checked_add(model.cached)?,
        })
    })
}

impl Invocation {
    fn record(&mut self, terminal: Terminal) {
        self.invalid |= self.terminal.replace(terminal).is_some();
    }

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
