//! Native fixed chat replies over the existing protected observations.
use crate::lead_page::Prompt;
use oxa_config::Env;
use oxa_proto::{codes, work::Snapshot, OxaError};
use oxa_work::board::Board;
use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Component, Path},
};

pub(crate) mod conversation;
mod evidence;
pub(crate) use conversation::converse;

pub(crate) struct QuickReply {
    pub text: String,
    pub observed_at: u64,
    pub source: &'static str,
    pub href: &'static str,
}

pub(crate) fn quick_reply(
    prompt: Prompt,
    board: Option<&Board>,
    situation: Option<&Snapshot>,
) -> Option<QuickReply> {
    Some(match prompt {
        Prompt::Status => {
            let board = board?;
            QuickReply {
                text: format!(
                    "The board contained {} items at observation.",
                    board.entries.len()
                ),
                observed_at: board.observed_at,
                source: "Board",
                href: "/board",
            }
        }
        Prompt::Workers => {
            let snapshot = situation?;
            QuickReply {
                text: format!(
                    "The runner reported {} running tasks. Verified worker goals are unavailable.",
                    snapshot.running.len()
                ),
                observed_at: snapshot.at_ms / 1000,
                source: "Workers",
                href: "/workers",
            }
        }
        Prompt::Attention => {
            let snapshot = situation?;
            QuickReply {
                text: format!(
                    "The runner reported {} open owner decisions.",
                    snapshot.asks.len()
                ),
                observed_at: snapshot.at_ms / 1000,
                source: "Asks",
                href: "/asks",
            }
        }
    })
}

fn protected_source<T>(
    env: &Env,
    key: &str,
    uid: u32,
    read: impl FnOnce(&Path, u32) -> Result<T, &'static str>,
) -> Result<Option<T>, OxaError> {
    let Some(path) = env.get(key).map(Path::new) else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err(OxaError::new(codes::USAGE, format!("{key} must be absolute")));
    }
    Ok(read(path, uid).ok())
}

fn protected_board(env: &Env, uid: u32) -> Result<Option<Board>, OxaError> {
    protected_source(env, "OXA_LEAD_BOARD_LOG", uid, |path, uid| {
        crate::lead_input::board_and_app_quota(path, uid).map(|(board, _)| board)
    })
}

pub(crate) fn run(words: &[String], project: &Path, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let prompt =
        match words {
            [word] if word == "--init" => return provision(project, env, out),
            [word] if word == "status" => Prompt::Status,
            [word] if word == "workers" => Prompt::Workers,
            [word] if word == "attention" => Prompt::Attention,
            [first, ..] if !first.starts_with("--") => {
                let reply = run_cli_conversation(&words.join(" "), project, env)?;
                write_conversation(&reply, out)
                    .map_err(|error| OxaError::io("cannot write lead chat reply", &error))?;
                return Ok(0);
            }
            _ => return Err(OxaError::new(
                codes::USAGE,
                "Lead chat arguments unavailable. Use oxa lead chat status, workers, attention or --init.",
            )),
        };
    let uid = owner_uid()?;
    let (board, situation) = match prompt {
        Prompt::Status => (protected_board(env, uid)?, None),
        _ => (
            None,
            protected_source(env, "OXA_LEAD_SITUATION", uid, crate::lead_input::situation)?,
        ),
    };
    let reply = quick_reply(prompt, board.as_ref(), situation.as_ref());
    match reply {
        Some(reply) => writeln!(
            out,
            "{}\n{} ({}) · observed at {} (Unix seconds)",
            reply.text, reply.source, reply.href, reply.observed_at
        ),
        None => writeln!(out, "Snapshot observations unavailable for this prompt."),
    }
    .and_then(|()| writeln!(out, "From the snapshot · 0 tokens"))
    .map_err(|error| OxaError::io("cannot write lead chat reply", &error))?;
    Ok(0)
}

fn owner_uid() -> Result<u32, OxaError> {
    std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|_| OxaError::new(codes::INVALID, "OS owner identity unavailable"))
}

fn provision(project: &Path, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    use crate::dispatch::LeadOwnerBinding;

    let dirs = oxa_config::dirs_from_env(env, None)?;
    let project = canonical_project(project)?;
    let uid = owner_uid()?;
    if LeadOwnerBinding::load(&dirs, &project, uid)?.is_some() {
        return Err(OxaError::new(
            codes::USAGE,
            "A dedicated Lead Session is already configured",
        ));
    }
    prepare_default_state(&dirs)?;
    let id = LeadOwnerBinding::create(&dirs, &project, uid)?;
    writeln!(out, "{id}")
        .map_err(|error| OxaError::io("cannot write the dedicated Lead Session ID", &error))?;
    Ok(0)
}

fn canonical_project(project: &Path) -> Result<std::path::PathBuf, OxaError> {
    project
        .canonicalize()
        .map_err(|_| OxaError::new(codes::INVALID, "Lead project unavailable"))
}

fn write_conversation(reply: &conversation::ConversationReply, out: &mut dyn Write) -> std::io::Result<()> {
    let proposal = reply.action.as_ref().map(serde_json::to_string).transpose()?;
    writeln!(out, "{}", reply.text)?;
    for fact in &reply.facts {
        writeln!(
            out,
            "{} ({}) · observed at {} (Unix seconds)",
            fact.label, fact.href, fact.observed_at
        )?;
    }
    if let Some(proposal) = &proposal {
        writeln!(out, "Proposal awaiting owner confirmation · {proposal}")?;
    }
    Ok(())
}

fn run_cli_conversation(
    message: &str,
    project: &Path,
    env: &Env,
) -> Result<conversation::ConversationReply, OxaError> {
    let dirs = oxa_config::dirs_from_env(env, None)?;
    let project = canonical_project(project)?;
    let uid = owner_uid()?;
    let binding = crate::dispatch::LeadOwnerBinding::load(&dirs, &project, uid)?
        .ok_or_else(|| OxaError::new(codes::USAGE, "Lead owner chat is unavailable"))?;
    converse_bound(message, &binding, env).map_err(cli_conversation_error)
}

fn cli_conversation_error(error: OxaError) -> OxaError {
    // Provider errors can carry private response bytes in messages and hints.
    // Keep the typed internal failure for the journal, and render only fixed text here.
    let message = match error.code {
        "budget_not_configured" => {
            "Cloud models need a positive oxa.cloud.daily_budget_usd in oxa user settings."
        }
        "budget_exceeded" | codes::BUDGET_EXHAUSTED => "The daily cloud budget is exhausted.",
        "subscription_limit_exceeded" => "The local subscription limit would be exceeded.",
        "subscription_limits_not_configured" => "Subscription limits are unavailable.",
        "lead_owner_unavailable" => "The selected Lead session is unavailable.",
        "lead_reply_invalid" => "Lead returned an invalid reply.",
        "lead_reply_incomplete" => "Lead reply did not complete.",
        codes::BUSY => "The selected Lead session is busy.",
        codes::NOT_FOUND => "The configured Lead model ID is unavailable.",
        codes::USAGE => "Lead chat requires a bounded message.",
        codes::OFFLINE => "Lead chat cannot reach the configured provider in offline mode.",
        codes::ENGINE_HTTP
        | codes::ENGINE_DOWN
        | codes::ENGINE_BUSY
        | codes::STREAM_BROKEN
        | "cloud_rate_limited" => "The Lead provider request failed.",
        _ => return OxaError::new(codes::INVALID, "Lead reply did not complete."),
    };
    OxaError::new(error.code, message)
}

pub(crate) fn converse_bound(
    message: &str,
    binding: &crate::dispatch::LeadOwnerBinding,
    env: &Env,
) -> Result<conversation::ConversationReply, OxaError> {
    binding.recheck()?;
    let uid = owner_uid()?;
    let board = protected_board(env, uid)?;
    let situation = protected_source(env, "OXA_LEAD_SITUATION", uid, crate::lead_input::situation)?;
    converse(binding, env, message, board.as_ref(), situation.as_ref())
}

fn prepare_default_state(dirs: &oxa_config::Dirs) -> Result<(), OxaError> {
    let agent = dirs.agent_dir.as_path();
    if dirs.sessions_dir.parent() != Some(agent) {
        return Ok(());
    }
    match std::fs::symlink_metadata(agent) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(OxaError::io("cannot inspect Lead state directory", &error)),
    }
    let canonical_parent = agent
        .ancestors()
        .skip(1)
        .find(|path| path.exists())
        .is_some_and(|path| path.canonicalize().ok().as_deref() == Some(path));
    if !canonical_parent || agent.components().any(|part| part == Component::ParentDir) {
        return Err(OxaError::new(
            codes::INVALID,
            "Lead state directory has an unsafe parent",
        ));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(agent)
        .map_err(|error| OxaError::io("cannot create Lead state directory", &error))
}
