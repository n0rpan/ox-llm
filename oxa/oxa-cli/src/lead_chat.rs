//! Native fixed chat replies over the existing protected observations.
use crate::lead_page::Prompt;
use oxa_config::Env;
use oxa_proto::{codes, work::Snapshot, OxaError};
use oxa_work::board::Board;
use std::{io::Write, os::unix::fs::MetadataExt, path::Path};

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

pub(crate) fn run_fixed(
    words: &[String],
    project: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let prompt =
        match words {
            [word] if word == "--init" => return provision(project, env, out),
            [word] if word == "status" => Prompt::Status,
            [word] if word == "workers" => Prompt::Workers,
            [word] if word == "attention" => Prompt::Attention,
            _ => return Err(OxaError::new(
                codes::USAGE,
                "Lead chat arguments unavailable. Use oxa lead chat status, workers, attention or --init.",
            )),
        };
    let uid = owner_uid()?;
    let (board, situation) = match prompt {
        Prompt::Status => (
            protected_source(env, "OXA_LEAD_BOARD_LOG", uid, |path, uid| {
                crate::lead_input::board_and_app_quota(path, uid).map(|(board, _)| board)
            })?,
            None,
        ),
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
    let project = project
        .canonicalize()
        .map_err(|_| OxaError::new(codes::INVALID, "Lead project unavailable"))?;
    let uid = owner_uid()?;
    if LeadOwnerBinding::load(&dirs, &project, uid)?.is_some() {
        return Err(OxaError::new(
            codes::USAGE,
            "A dedicated Lead Session is already configured",
        ));
    }
    let id = LeadOwnerBinding::create(&dirs, &project, uid)?;
    writeln!(out, "{id}")
        .map_err(|error| OxaError::io("cannot write the dedicated Lead Session ID", &error))?;
    Ok(0)
}
