//! Consume the child's headless contract without retaining its event stream.
use crate::{Result, RunPolicy, Task};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

pub mod owner_cli;

const LINE_CAP: usize = 64 * 1024;
const DIAGNOSTIC_CAP: usize = 8192;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    Done,
    NothingToDo,
    Blocked,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerInterruption {
    Deadline,
}

/// Invocation-local token counts reported by the worker at settlement.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkerTokens {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    /// Cached input is included in the prompt count.
    pub cached: Option<u64>,
    pub complete: bool,
    pub unreported_requests: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerResult {
    pub outcome: WorkerOutcome,
    pub exit_code: i32,
    pub error_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<WorkerInterruption>,
    /// Measured process usage. Older workers and interrupted processes may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<WorkerTokens>,
}

impl WorkerResult {
    pub(crate) fn accepted(&self) -> bool {
        matches!(
            (&self.outcome, self.exit_code),
            (WorkerOutcome::Done, 0) | (WorkerOutcome::NothingToDo, 4)
        )
    }
}

pub(crate) struct Worker {
    pub result: WorkerResult,
    pub session: Option<String>,
    pub diagnostic: String,
    pub notes: Vec<crate::ProblemNote>,
    /// Crates the worker declared as acceptance tests in its last message (#1442).
    pub declared: crate::derived::Declaration,
}

impl Worker {
    pub fn failure(&self) -> Option<String> {
        (!self.result.accepted()).then(|| {
            format!(
                "worker {:?} (exit {}, {}): {}",
                self.result.outcome,
                self.result.exit_code,
                self.result.error_class.as_deref().unwrap_or("no error class"),
                self.diagnostic.trim()
            )
        })
    }
}

#[cfg(test)]
mod autonomy_tests {
    use super::{mode_args, worker_authority};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    /// A queue that started in yolo passes `--yolo` only while the owner's level still is yolo:
    /// the level is read at each launch.
    #[test]
    fn a_lowered_owner_level_stops_passing_yolo() {
        let level = Arc::new(Mutex::new(oxa_proto::Autonomy::Yolo));
        let reads = Arc::new(AtomicUsize::new(0));
        let now = {
            let level = level.clone();
            let reads = reads.clone();
            Arc::new(move || {
                reads.fetch_add(1, Ordering::Relaxed);
                let autonomy = *level.lock().unwrap();
                let github = match autonomy {
                    oxa_proto::Autonomy::Yolo => crate::WorkerGithub::Worker,
                    _ => crate::WorkerGithub::Runner,
                };
                (autonomy, github)
            }) as Arc<dyn Fn() -> (oxa_proto::Autonomy, crate::WorkerGithub) + Send + Sync>
        };
        let mut policy = crate::RunPolicy {
            git_binary: std::path::PathBuf::from("/bin/true"),
            host_path: std::ffi::OsString::new(),
            terminate_binary: None,
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            offline: false,
            runner_owns_github: false,
            worker_github: crate::WorkerGithub::default(),
            autonomy: oxa_proto::Autonomy::Yolo,
            authority_now: Some(now),
            storage: std::path::PathBuf::from("/tmp/storage"),
        };
        let (autonomy, github) = worker_authority(&policy);
        assert_eq!(mode_args(autonomy), &["--yolo"]);
        assert_eq!(github, crate::WorkerGithub::Worker);
        *level.lock().unwrap() = oxa_proto::Autonomy::Standard;
        let (autonomy, github) = worker_authority(&policy);
        assert!(
            mode_args(autonomy).is_empty(),
            "a lowered level must not pass --yolo"
        );
        assert_eq!(github, crate::WorkerGithub::Runner);
        assert_eq!(reads.load(Ordering::Relaxed), 2, "one settings read per launch");
        // Without a resolver the snapshot stands (single-shot callers and tests).
        policy.authority_now = None;
        let (autonomy, github) = worker_authority(&policy);
        assert_eq!(mode_args(autonomy), &["--yolo"]);
        assert_eq!(github, crate::WorkerGithub::default());
    }
}

fn worker_authority(policy: &RunPolicy) -> (oxa_proto::Autonomy, crate::WorkerGithub) {
    policy
        .authority_now
        .as_ref()
        .map_or((policy.autonomy, policy.worker_github), |now| now())
}

/// The mode flags of a worker child. Only an owner in yolo passes yolo on: the worker then
/// runs without the tool sandbox and without prompts. Every other level keeps the default
/// worker (auto mode inside the sandbox), exactly as before.
pub(crate) fn mode_args(autonomy: oxa_proto::Autonomy) -> &'static [&'static str] {
    match autonomy {
        oxa_proto::Autonomy::Yolo => &["--yolo"],
        _ => &[],
    }
}

/// GitHub access of a worker child. Without runner ownership nothing changes. With it, only
/// `Worker` (yolo) keeps its environment, MCP, web search and subagents; the other levels
/// remove every credential and the adapters that could reach GitHub.
pub(crate) fn github_access(command: &mut Command, runner_owns_github: bool, mode: crate::WorkerGithub) {
    if !runner_owns_github || mode == crate::WorkerGithub::Worker {
        return;
    }
    // The provider connection is host-side. The worker's tool sandbox and
    // independent tool adapters have no GitHub access.
    crate::restrict_child_github(command);
    command.args(["--no-mcp", "--no-extensions", "--no-web-search", "--no-subagent"]);
}

pub(crate) fn run(
    binary: &Path,
    source: &Path,
    root: &Path,
    task: &Task,
    prompt: &str,
    owner_selected_model: bool,
    policy: &RunPolicy,
    lock: &File,
    session: Option<&str>,
    remaining: Duration,
    #[cfg(target_os = "linux")] managed_steering: Option<&crate::managed_steering::Launch>,
    on_session: &mut (dyn FnMut(&str) -> Result<()> + Send),
) -> Result<Worker> {
    // The owner's GitHub authority is read at launch, like the autonomy level: lowering
    // `permissions.defaultMode` (or pinning `runner`) revokes worker GitHub for the tasks
    // a running queue has not started yet.
    let (autonomy, worker_github) = worker_authority(policy);
    let prompt = &*crate::worker_prompt(prompt, worker_github);
    let terminate = policy
        .terminate_binary
        .as_deref()
        .ok_or("no trusted kill executable outside the source project")?;
    let mut command = Command::new(binary);
    if !owner_selected_model {
        command.env(crate::PlannedTask::DELEGATED_MODEL_ENV, "1");
    }
    crate::isolate_git_environment(&mut command);
    #[cfg(target_os = "linux")]
    {
        command.env_remove(crate::managed_steering::FD_ENV);
        if let Some(launch) = managed_steering {
            launch.attach(&mut command)?;
        }
    }
    command
        .env("PATH", &policy.host_path)
        .env_remove("OXA_TRUST_PROJECT")
        .env_remove(crate::WORKTREE_TRUST_SOURCE_ENV)
        .env_remove(oxa_proto::names::ENV_UNATTENDED);
    mark_unattended(&mut command);
    command.env("OXA_WORK_INBOX", "1");
    command.current_dir(root).args([
        "--mode=json",
        "--model",
        &task.model,
        "--max-time",
        &remaining.as_secs_f64().to_string(),
    ]);
    if let Some(source) = source.to_str() {
        command.env(crate::WORKTREE_TRUST_SOURCE_ENV, source);
    } else {
        command.arg("-na");
    }
    // The owner's level is read at launch: a queue that started in yolo stops passing `--yolo`
    // as soon as the owner lowers `permissions.defaultMode`.
    command.args(mode_args(autonomy));
    if policy.offline {
        command.arg("--offline");
    }
    // A lane task's work ends in a runner-driven public push that the owner approves, so the
    // worker never receives the host GitHub credentials: a lane task always gets the runner
    // treatment, whatever the owner's worker GitHub level.
    let worker_github = if task.lane_ci {
        crate::WorkerGithub::Runner
    } else {
        worker_github
    };
    github_access(&mut command, github_isolated(policy, task), worker_github);
    if let Some(id) = session {
        command.arg(format!("--continue={id}"));
    }
    let mut child = command
        .args(["--", prompt])
        .stdin(lock.try_clone().map_err(|e| e.to_string())?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start worker: {e}"))?;
    let stdout = child.stdout.take().ok_or("worker stdout missing")?;
    let stderr = child.stderr.take().ok_or("worker stderr missing")?;
    std::thread::scope(|scope| {
        let (notice_tx, notice_rx) = std::sync::mpsc::channel();
        let header_tx = notice_tx.clone();
        let header = scope.spawn(move || {
            read_session(stdout, &mut |id| {
                let result = on_session(id);
                if let Err(error) = &result {
                    let _ = header_tx.send(Some(error.clone()));
                }
                result
            })
        });
        let terminal = scope.spawn(move || {
            let result = read_result(stderr);
            let _ = notice_tx.send(None);
            result
        });
        let waited = match crate::worker_wait::wait(
            &mut child,
            &notice_rx,
            remaining,
            Duration::from_secs(60),
            Duration::from_secs(10),
            terminate,
        ) {
            Ok(waited) => waited,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = header.join();
                let _ = terminal.join();
                return Err(error);
            }
        };
        let session = header.join().map_err(|_| "worker header reader failed")?;
        let terminal = terminal.join().map_err(|_| "worker result reader failed")?;
        if let Some(error) = waited.failure {
            return Err(error);
        }
        let status = waited.status;
        if waited.deadline {
            let tokens = terminal
                .ok()
                .filter(|(result, _)| status.code() == Some(result.exit_code))
                .and_then(|(result, _)| result.tokens);
            return Ok(Worker {
                result: WorkerResult {
                    outcome: WorkerOutcome::Interrupted,
                    exit_code: status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(9)),
                    error_class: Some("worker_deadline".into()),
                    interruption: Some(WorkerInterruption::Deadline),
                    tokens,
                },
                session: session.ok().and_then(|(id, _, _)| id),
                diagnostic: "worker exceeded max_time and its wrap-up grace; worktree retained".into(),
                notes: vec![],
                declared: crate::derived::Declaration::Absent,
            });
        }
        let (session, notes, declared) = session?;
        let (result, diagnostic) = terminal?;
        if status.code() != Some(result.exit_code) {
            return Err("worker exit code disagrees with its oxa_result".into());
        }
        if result.accepted() && session.is_none() {
            return Err("successful worker did not report a session id".into());
        }
        Ok(Worker {
            result,
            session,
            diagnostic,
            notes,
            declared,
        })
    })
}

fn read_session(
    pipe: impl Read,
    on_session: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<(
    Option<String>,
    Vec<crate::ProblemNote>,
    crate::derived::Declaration,
)> {
    let mut reader = BufReader::new(pipe);
    let session = record(&mut reader)?.and_then(|line| {
        let Line::Complete(line) = line else { return None };
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
        (value["type"] == "session")
            .then(|| value["id"].as_str().map(str::to_owned))
            .flatten()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
    });
    let recorded = session.as_deref().map(on_session).transpose();
    let mut notes = vec![];
    // The worker's last message carries its declarations; a later message replaces them.
    let mut declared = crate::derived::Declaration::Absent;
    while let Some(record) = record(&mut reader)? {
        let bytes = match record {
            Line::Complete(bytes) => bytes,
            Line::Discarded {
                repeats_evidence: true,
            } => continue,
            Line::Discarded {
                repeats_evidence: false,
            } => {
                // The discarded event could have supplied the final assistant reply.
                declared = crate::derived::Declaration::Unreadable;
                notes.clear();
                continue;
            }
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value["type"] == "message_end" && value["message"]["role"] == "assistant" {
            let mut text = String::new();
            if let Some(parts) = value["message"]["content"].as_array() {
                for part in parts {
                    if part["type"] == "text" {
                        if let Some(value) = part["text"].as_str() {
                            text.push_str(value);
                        }
                    }
                }
            }
            notes = crate::notes::parse(&text);
            declared = crate::derived::parse_declaration(&text);
        }
    }
    recorded?;
    Ok((session, notes, declared))
}

fn read_result(pipe: impl Read) -> Result<(WorkerResult, String)> {
    let mut reader = BufReader::new(pipe);
    let mut result = None;
    let mut duplicate = false;
    let mut diagnostic = Vec::new();
    while let Some(bytes) = line(&mut reader)? {
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        if let Some(mut value) = value.filter(|v| v["type"] == "oxa_result") {
            let measured_process = value["usage_scope"] == "process";
            // Usage is optional evidence. Strip it before parsing the outcome so a
            // missing or malformed measurement cannot change the worker's disposition.
            let tokens = value.get_mut("tokens").map(std::mem::take);
            let mut parsed: WorkerResult =
                serde_json::from_value(value).map_err(|e| format!("worker result: {e}"))?;
            if measured_process {
                parsed.tokens = tokens.and_then(|tokens| serde_json::from_value(tokens).ok());
            }
            duplicate |= result.replace(parsed).is_some();
        } else {
            keep_tail(&mut diagnostic, &bytes, DIAGNOSTIC_CAP);
        }
    }
    if duplicate {
        return Err("worker reported more than one oxa_result".into());
    }
    let diagnostic = String::from_utf8_lossy(&diagnostic).into_owned();
    Ok((
        result.ok_or_else(|| format!("worker did not report oxa_result: {diagnostic}"))?,
        diagnostic,
    ))
}

enum Line {
    Complete(Vec<u8>),
    Discarded { repeats_evidence: bool },
}

fn line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    Ok(record(reader)?.map(|line| match line {
        Line::Complete(bytes) => bytes,
        Line::Discarded { .. } => Vec::new(),
    }))
}

// Discard an oversized record while still draining the pipe. Event/progress size
// cannot increase the runner's memory use or block a child writing to either pipe.
fn record(reader: &mut impl BufRead) -> Result<Option<Line>> {
    let mut line = Vec::new();
    let mut discarded = None;
    loop {
        let bytes = reader.fill_buf().map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Ok(discarded.or_else(|| (!line.is_empty()).then_some(Line::Complete(line))));
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let count = end.map_or(bytes.len(), |i| i + 1);
        if discarded.is_none() {
            let kept = count.min(LINE_CAP - line.len());
            line.extend_from_slice(&bytes[..kept]);
            if kept < count {
                // The JSON event writer serializes the type first. These aggregates
                // repeat records already emitted individually by that same writer.
                let repeats_evidence = line.starts_with(br#"{"type":"agent_end","#)
                    || line.starts_with(br#"{"type":"turn_end","#);
                discarded = Some(Line::Discarded { repeats_evidence });
                line.clear();
            }
        }
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(discarded.unwrap_or(Line::Complete(line))));
        }
    }
}

pub(crate) fn keep_tail(tail: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    if bytes.len() >= cap {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - cap..]);
    } else {
        let discard = (tail.len() + bytes.len()).saturating_sub(cap);
        tail.drain(..discard);
        tail.extend_from_slice(bytes);
    }
}

/// Whether this worker's launch strips the host's GitHub credentials: a runner-owned task
/// (the runner alone talks to GitHub), and every lane task, whose work ends in a
/// runner-driven public push that the owner approves.
pub(crate) fn github_isolated(policy: &RunPolicy, task: &Task) -> bool {
    policy.runner_owns_github || task.lane_ci
}

/// #1512 S3: a worker is unattended: no person watches it, so the OS sandbox stays on. The
/// inherited marker is dropped first, then the worker's own is set.
pub(crate) fn mark_unattended(command: &mut Command) {
    command.env(oxa_proto::names::ENV_UNATTENDED, "1");
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::Autonomy;
    use serde_json::json;

    #[test]
    fn a_lane_task_worker_launch_strips_the_host_github_credentials() {
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin:/bin".into(),
            terminate_binary: None,
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            offline: true,
            runner_owns_github: false,
            autonomy: oxa_proto::Autonomy::default(),
            worker_github: Default::default(),
            authority_now: None,
            storage: std::env::temp_dir(),
        };
        let mut task = Task {
            id: "lane-1".into(),
            model: "fake".into(),
            branch: "agent/lane-1".into(),
            base: "main".into(),
            prompt: "Synthetic lane task".into(),
            max_time: 2700,
            reviewed_pull_request: None,
            lane_ci: true,
        };
        assert!(
            github_isolated(&policy, &task),
            "a lane task's worker never receives the host GitHub credentials"
        );
        task.lane_ci = false;
        assert!(
            !github_isolated(&policy, &task),
            "a plain task keeps the normal worker environment"
        );
        let runner_owned = RunPolicy {
            runner_owns_github: true,
            ..policy
        };
        assert!(
            github_isolated(&runner_owned, &task),
            "a runner-owned task stays isolated without lane CI"
        );
    }

    #[test]
    fn oxa_work_worker_command_sets_oxa_unattended() {
        let mut command = Command::new("true");
        mark_unattended(&mut command);
        assert!(command
            .get_envs()
            .any(|(k, v)| k == "OXA_UNATTENDED" && v == Some(std::ffi::OsStr::new("1"))));
    }

    #[test]
    fn yolo_runner_starts_yolo_worker() {
        assert_eq!(mode_args(Autonomy::Yolo), ["--yolo"]);
    }

    #[test]
    fn non_yolo_runner_worker_keeps_bwrap() {
        // No yolo flag means the worker keeps its default sandboxed mode.
        assert!(mode_args(Autonomy::Auto).is_empty());
        assert!(mode_args(Autonomy::Standard).is_empty());
    }

    #[test]
    fn worker_argv_has_no_yolo_by_default() {
        assert_eq!(Autonomy::default(), Autonomy::Standard);
        assert!(mode_args(Autonomy::default()).is_empty());
    }

    fn env_of(command: &Command, name: &str) -> Option<Option<String>> {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn yolo_worker_keeps_github_env() {
        let mut command = Command::new("oxa");
        github_access(&mut command, true, crate::WorkerGithub::Worker);
        assert_eq!(env_of(&command, crate::RUNNER_GITHUB_ENV), None);
        assert_eq!(env_of(&command, "GH_TOKEN"), None);
        assert_eq!(env_of(&command, "GITHUB_TOKEN"), None);
        assert_eq!(env_of(&command, "GH_CONFIG_DIR"), None);
    }

    #[test]
    fn yolo_worker_keeps_mcp_web_subagent() {
        let mut command = Command::new("oxa");
        github_access(&mut command, true, crate::WorkerGithub::Worker);
        assert!(args_of(&command).is_empty());
    }

    #[test]
    fn a_lowered_owner_level_revokes_worker_github() {
        // The launch reads the current authority, exactly as it reads the current autonomy
        // level: a queue that started while the owner was in yolo must not keep handing
        // out GitHub credentials after the owner lowered the mode or pinned `runner`.
        let level = std::sync::Arc::new(std::sync::Mutex::new(crate::WorkerGithub::Worker));
        let now = {
            let level = level.clone();
            std::sync::Arc::new(move || *level.lock().unwrap())
                as std::sync::Arc<dyn Fn() -> crate::WorkerGithub + Send + Sync>
        };
        let mut command = Command::new("oxa");
        let worker_github = now();
        github_access(&mut command, true, worker_github);
        assert!(
            args_of(&command).is_empty() && env_of(&command, crate::RUNNER_GITHUB_ENV).is_none(),
            "while the owner is in yolo the worker keeps its GitHub environment"
        );
        *level.lock().unwrap() = crate::WorkerGithub::Runner;
        let mut command = Command::new("oxa");
        let worker_github = now();
        github_access(&mut command, true, worker_github);
        assert_eq!(env_of(&command, crate::RUNNER_GITHUB_ENV), Some(Some("1".into())));
        assert_eq!(
            args_of(&command),
            ["--no-mcp", "--no-extensions", "--no-web-search", "--no-subagent"]
        );
        // Without a resolver the snapshot stands (single-shot callers and tests).
        let mut command = Command::new("oxa");
        let worker_github =
            std::option::Option::<std::sync::Arc<dyn Fn() -> crate::WorkerGithub + Send + Sync>>::None
                .as_ref()
                .map_or(crate::WorkerGithub::Worker, |_| unreachable!());
        github_access(&mut command, true, worker_github);
        assert!(args_of(&command).is_empty());
    }

    #[test]
    fn runner_owned_stays_default_without_yolo() {
        use crate::WorkerGithub::*;
        assert_eq!(crate::WorkerGithub::default(), Runner);
        for mode in [Runner, Broker] {
            let mut command = Command::new("oxa");
            github_access(&mut command, true, mode);
            assert_eq!(env_of(&command, crate::RUNNER_GITHUB_ENV), Some(Some("1".into())));
            assert_eq!(env_of(&command, "GH_TOKEN"), Some(None));
            assert_eq!(env_of(&command, "GITHUB_TOKEN"), Some(None));
            assert_eq!(
                args_of(&command),
                ["--no-mcp", "--no-extensions", "--no-web-search", "--no-subagent"]
            );
        }
        assert_eq!(
            crate::WorkerGithub::resolve(None, oxa_proto::PermissionMode::Standard),
            Runner
        );
        // A task the runner does not own is untouched in every mode.
        let mut command = Command::new("oxa");
        github_access(&mut command, false, Broker);
        assert!(args_of(&command).is_empty());
    }

    #[test]
    fn yolo_packet_guidance_allows_gh() {
        let packet = format!("{}ISSUE BODY", crate::github_runner_guidance("issue"));
        let yolo = crate::worker_prompt(&packet, crate::WorkerGithub::Worker);
        assert!(yolo.ends_with("ISSUE BODY"));
        assert!(yolo.contains("`gh`") && !yolo.contains("Do not invoke gh"));
        assert!(!yolo.starts_with(crate::GITHUB_RUNNER_LEAD));
        assert_eq!(crate::worker_prompt(&packet, crate::WorkerGithub::Broker), packet);
        assert_eq!(crate::worker_prompt(&packet, crate::WorkerGithub::Runner), packet);
        // A prompt without the runner lead is never rewritten.
        assert_eq!(
            crate::worker_prompt("plain", crate::WorkerGithub::Worker),
            "plain"
        );
    }

    #[test]
    fn a_discarded_assistant_event_leaves_declaration_evidence_unreadable() {
        let message = json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "plain text ".repeat(LINE_CAP / 8)}],
            }
        });
        let events = format!("{{\"type\":\"session\",\"id\":\"fixture\"}}\n{message}\n");
        let (_, _, declaration) = read_session(events.as_bytes(), &mut |_| Ok(())).unwrap();
        assert_eq!(declaration, crate::derived::Declaration::Unreadable);
    }

    #[test]
    fn a_discarded_aggregate_keeps_a_complete_assistant_declaration() {
        let reply = json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "<oxa-acceptance>[\"one\"]</oxa-acceptance>"}],
            }
        });
        let aggregate = json!({"type": "agent_end", "messages": ["plain text ".repeat(LINE_CAP / 8)]});
        let events = format!("{{\"type\":\"session\",\"id\":\"fixture\"}}\n{reply}\n{aggregate}\n");
        let (_, _, declaration) = read_session(events.as_bytes(), &mut |_| Ok(())).unwrap();
        assert_eq!(
            declaration,
            crate::derived::Declaration::Tests(vec!["one".into()])
        );
    }

    #[test]
    fn terminal_process_usage_survives_serializing_the_worker_result() {
        let receipt = json!({
            "type": "oxa_result", "outcome": "done", "exit_code": 0, "usage_scope": "process",
            "tokens": {
                "prompt": 480, "completion": 21, "cached": null, "complete": false,
                "unreported_requests": 1,
            }
        });
        let (result, diagnostic) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(result.accepted());
        assert!(diagnostic.is_empty());
        let saved = serde_json::to_value(result).unwrap();
        assert_eq!(saved["tokens"], receipt["tokens"]);
        let restored: WorkerResult = serde_json::from_value(saved).unwrap();
        assert_eq!(
            restored.tokens,
            Some(WorkerTokens {
                prompt: Some(480),
                completion: Some(21),
                cached: None,
                complete: false,
                unreported_requests: Some(1),
            })
        );
    }

    #[test]
    fn legacy_or_unusable_usage_remains_unknown_without_changing_outcomes() {
        let mut receipt = json!({"type": "oxa_result", "outcome": "done", "exit_code": 0});
        let (legacy, _) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(legacy.accepted());
        assert!(legacy.tokens.is_none());
        assert!(serde_json::to_value(legacy).unwrap().get("tokens").is_none());

        receipt["usage_scope"] = json!("merged_task");
        receipt["tokens"] = json!({
            "prompt": 0, "completion": 0, "cached": 0, "complete": true, "unreported_requests": 0
        });
        let (other_scope, _) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(other_scope.accepted());
        assert!(other_scope.tokens.is_none());

        receipt["usage_scope"] = json!("process");
        receipt["tokens"]["prompt"] = json!("unknown");
        let (malformed, _) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(malformed.accepted());
        assert!(malformed.tokens.is_none());
    }
}
