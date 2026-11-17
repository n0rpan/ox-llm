//! P6: `oxa wipe` (privacy). Removes what oxa stored about one session (`--session <id>`) or
//! about every session (`--all`): the session files, queued prompts under `<agent dir>/inbox`,
//! the spill directories under `<agent dir>/tmp`, and with `--engine` the engine's session state.
//! Never touches `settings.json`, `models.json`, `trust.json`, `mcp.json`, the OAuth token file
//! or the identity file. Session files are unlinked, not moved to the trash.
//!
//! Exit codes: 0 done; 1 an id matched nothing or matched several, a confirmation was declined,
//! something was kept, or the engine state was not cleared; 2 a usage error or `--all` without
//! a terminal and without `--yes`.
use crate::dispatch::{default_endpoint, engine_for};
use oxa_config::{dirs_from_env, Dirs, Env, ModelsConfig, Settings};
use oxa_proto::{codes, Engine, OxaError};
use oxa_session::{
    purge_session_file, remove_all_spill, remove_inbox, remove_spill_dir, session_files_under, SessionHeader,
};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const USAGE: &str =
    "usage: oxa wipe --session <id> [--engine] | oxa wipe --all [--yes] [--engine]  (both take [--session-dir <dir>])";

/// The `OXA_SESSION_DIR` variable (see `dispatch.rs`, where the session directory is chosen).
const ENV_SESSION_DIR: &str = "OXA_SESSION_DIR";

/// The process streams `oxa wipe` uses, injectable for tests.
pub struct WipeIo<'a> {
    pub stdin: &'a mut dyn Read,
    pub stdin_is_tty: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    session: Option<String>,
    all: bool,
    yes: bool,
    engine: bool,
    session_dir: Option<PathBuf>,
}

fn parse(words: &[String]) -> Result<Plan, OxaError> {
    let usage = |m: &str| OxaError::new(codes::USAGE, m.to_string()).with_hint(USAGE);
    let mut p = Plan::default();
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            "--session" => {
                let v = words
                    .get(i + 1)
                    .ok_or_else(|| usage("--session needs a session id"))?;
                p.session = Some(v.clone());
                i += 2;
                continue;
            }
            "--session-dir" => {
                let v = words
                    .get(i + 1)
                    .ok_or_else(|| usage("--session-dir needs a directory"))?;
                p.session_dir = Some(PathBuf::from(v));
                i += 2;
                continue;
            }
            "--all" => p.all = true,
            "--yes" => p.yes = true,
            "--engine" => p.engine = true,
            other => return Err(usage(&format!("unknown wipe option: {other}"))),
        }
        i += 1;
    }
    match (p.session.is_some(), p.all) {
        (true, true) => Err(usage("--session and --all cannot be combined")),
        (false, false) => Err(usage("wipe needs --session <id> or --all")),
        _ => Ok(p),
    }
}

/// The sessions directory, chosen as `dispatch.rs` chooses it: `--session-dir` > `OXA_SESSION_DIR`
/// > the `sessionDir` user setting > `<agent dir>/sessions`.
fn sessions_dir(dirs: &Dirs, env: &Env, flag: Option<&Path>, cwd: &Path) -> Result<PathBuf, OxaError> {
    let resolved = dirs_from_env(env, flag)?;
    if flag.is_none() && env.get(ENV_SESSION_DIR).is_none() {
        let settings = Settings::load(dirs, cwd, false);
        if let Some(dir) = settings.session_dir(&dirs.home, cwd) {
            return Ok(dir);
        }
    }
    Ok(resolved.sessions_dir)
}

/// `oxa wipe ..`: the words after `wipe`. Returns the exit code.
pub fn run_wipe(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    io: WipeIo<'_>,
) -> Result<i32, OxaError> {
    let plan = parse(words)?;
    let sessions = sessions_dir(dirs, env, plan.session_dir.as_deref(), cwd)?;
    match &plan.session {
        Some(query) => wipe_one(query, plan.engine, dirs, &sessions, cwd, env, io),
        None => wipe_all(&plan, dirs, &sessions, cwd, env, io),
    }
}

/// The ids of the sessions `query` names: the exact id, else every id it is a prefix of.
fn matching_ids(query: &str, ids: &BTreeSet<String>) -> Vec<String> {
    if ids.contains(query) {
        return vec![query.to_string()];
    }
    ids.iter().filter(|id| id.starts_with(query)).cloned().collect()
}

/// `--session <id>`: remove that session's files and spill directory.
fn wipe_one(
    query: &str,
    engine: bool,
    dirs: &Dirs,
    sessions: &Path,
    cwd: &Path,
    env: &Env,
    io: WipeIo<'_>,
) -> Result<i32, OxaError> {
    let WipeIo { out, err, .. } = io;
    let entries = session_files_under(sessions);
    let ids: BTreeSet<String> = entries
        .iter()
        .filter_map(|(_, h)| h.as_ref().map(|h| h.id.clone()))
        .collect();
    if query.is_empty() {
        let _ = writeln!(err, "Error: the session id is empty ({USAGE})");
        return Ok(2);
    }
    let matches = matching_ids(query, &ids);
    match matches.len() {
        0 => {
            let _ = writeln!(
                err,
                "Error: no session matches `{query}` under {}",
                sessions.display()
            );
            return Ok(1);
        }
        1 => {}
        n => {
            let _ = writeln!(
                err,
                "Error: session id prefix `{query}` is ambiguous: {n} sessions match:"
            );
            for id in &matches {
                let path = entries
                    .iter()
                    .find(|(_, h)| h.as_ref().is_some_and(|h| &h.id == id))
                    .map(|(p, _)| p.display().to_string())
                    .unwrap_or_default();
                let _ = writeln!(err, "  {id}  {path}");
            }
            let _ = writeln!(err, "Give more characters of the id.");
            return Ok(1);
        }
    }
    let id = matches[0].clone();

    let files: Vec<PathBuf> = entries
        .iter()
        .filter(|(_, h)| h.as_ref().is_some_and(|h| h.id == id))
        .map(|(p, _)| p.clone())
        .collect();
    let inbox_files = remove_inbox(&dirs.agent_dir, Some(&id))?;
    let _ = writeln!(out, "removed {inbox_files} inbox file(s) for session {id}");
    for path in &files {
        purge_session_file(path)?;
        let _ = writeln!(out, "removed session file {}", path.display());
    }
    // Sessions forked or cloned from a removed file keep their own copy of its messages; they are
    // not removed, but are named so that the user can remove them too.
    let removed: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let kept: Vec<&PathBuf> = entries
        .iter()
        .filter(|(_, h)| {
            h.as_ref().is_some_and(|h: &SessionHeader| {
                h.id != id && h.parent_session.as_ref().is_some_and(|p| removed.contains(p))
            })
        })
        .map(|(p, _)| p)
        .collect();
    let spill = dirs.tmp_dir.join(&id);
    let spill_existed = spill.exists();
    let spill_files = remove_spill_dir(&dirs.tmp_dir, &id)?;
    if spill_existed {
        let _ = writeln!(
            out,
            "removed spill directory {} ({spill_files} files)",
            spill.display()
        );
    }
    let _ = writeln!(out, "removed {} session file(s) for session {id}", files.len());
    let mut ok = true;
    if !kept.is_empty() {
        ok = false;
        let _ = writeln!(
            err,
            "kept {} session(s) forked from it (they hold their own copy of its messages):",
            kept.len()
        );
        for p in kept {
            let _ = writeln!(err, "  {}", p.display());
        }
    }
    if engine {
        ok &= clear_engine(dirs, cwd, env, std::slice::from_ref(&id), out, err);
    }
    Ok(if ok { 0 } else { 1 })
}

/// `--all`: remove every session file and the whole spill directory, after a confirmation.
fn wipe_all(
    plan: &Plan,
    dirs: &Dirs,
    sessions: &Path,
    cwd: &Path,
    env: &Env,
    io: WipeIo<'_>,
) -> Result<i32, OxaError> {
    let WipeIo {
        stdin,
        stdin_is_tty,
        out,
        err,
    } = io;
    let entries = session_files_under(sessions);
    // Only files whose first line is a session header are oxa's; any other `.jsonl` under the
    // sessions directory is kept (and named), since a user may have pointed the setting at a
    // directory that holds other files.
    let (sessions_found, others): (Vec<_>, Vec<_>) = entries.into_iter().partition(|(_, h)| h.is_some());
    let tmp = &dirs.tmp_dir;
    if !plan.yes {
        if !stdin_is_tty {
            let _ = writeln!(
                err,
                "Error: `oxa wipe --all` removes every session, queued inbox prompt and spill file. Run it in a terminal to confirm, or pass --yes. Nothing was removed."
            );
            return Ok(2);
        }
        let _ = write!(
            out,
            "This removes {} session file(s) under {}, every queued inbox prompt and every spill file under {}.\nType yes to continue: ",
            sessions_found.len(),
            sessions.display(),
            tmp.display()
        );
        let _ = out.flush();
        let mut line = String::new();
        let _ = BufReader::new(&mut *stdin).read_line(&mut line);
        if line.trim() != "yes" {
            let _ = writeln!(err, "Cancelled. Nothing was removed.");
            return Ok(1);
        }
    }

    let inbox_files = remove_inbox(&dirs.agent_dir, None)?;
    let _ = writeln!(out, "removed {inbox_files} inbox file(s)");
    let mut ids = Vec::new();
    let mut parents = BTreeSet::new();
    for (path, header) in &sessions_found {
        purge_session_file(path)?;
        if let Some(h) = header {
            ids.push(h.id.clone());
        }
        if let Some(parent) = path.parent() {
            parents.insert(parent.to_path_buf());
        }
    }
    for dir in parents {
        // Removes a cwd directory only when it is empty now.
        let _ = std::fs::remove_dir(&dir);
    }
    let _ = writeln!(
        out,
        "removed {} session file(s) under {}",
        sessions_found.len(),
        sessions.display()
    );
    let existed = tmp.exists();
    let spill_files = remove_all_spill(tmp)?;
    if existed {
        let _ = writeln!(
            out,
            "removed spill directory {} ({spill_files} files)",
            tmp.display()
        );
    }
    let mut ok = true;
    if !others.is_empty() {
        ok = false;
        let _ = writeln!(
            err,
            "kept {} .jsonl file(s) that are not oxa sessions:",
            others.len()
        );
        for (p, _) in &others {
            let _ = writeln!(err, "  {}", p.display());
        }
    }
    if plan.engine && !ids.is_empty() {
        ok &= clear_engine(dirs, cwd, env, &ids, out, err);
    }
    Ok(if ok { 0 } else { 1 })
}

/// `--engine`: ask the engine to drop the state of each session hint, through the same
/// selector and `Engine::clear_hint` boundary used by normal sessions. Native local
/// clearing retains its owned-child/inert-cold contract; explicit HTTP selection
/// retains the loopback guard. Offline refusal is unchanged.
fn clear_engine(
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    ids: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    if oxa_proto::offline() {
        let _ = writeln!(
            err,
            "engine-side state not cleared: offline mode is on (no network)"
        );
        return false;
    }
    // Project settings cannot choose a native binary/provider for a privacy command.
    let user = Settings::load(dirs, cwd, false);
    let engine: std::sync::Arc<dyn Engine> = match crate::in_process::select(&user, &user, dirs, None, || {
        let (models, _) = ModelsConfig::load(dirs, env);
        default_endpoint(&models).and_then(|ep| engine_for(&ep))
    }) {
        Ok(engine) => engine,
        Err(e) => {
            let _ = writeln!(err, "engine-side state not cleared: {}", e.message);
            return false;
        }
    };
    let mut ok = true;
    for id in ids {
        match engine.clear_hint(id) {
            Ok(()) => {
                let _ = writeln!(out, "engine: cleared the state of session {id}");
            }
            Err(e) => {
                ok = false;
                let _ = writeln!(err, "engine: state of session {id} not cleared: {}", e.message);
            }
        }
    }
    ok
}
