//! B6: directory resolution from the environment. Owner: task B6. Signatures below are frozen; bodies are yours.
use crate::env::{HOME, SESSION_DIR, SYSTEM_SKILLS_DIR};
use crate::{Dirs, Env};
use oxa_proto::{codes, names, OxaError};
use std::path::{Path, PathBuf};

/// Resolve [`Dirs`]: `home` = `HOME`; `agent_dir` = `OXA_AGENT_DIR` (with `~` expanded) or
/// `<home>/.oxa/agent`; `sessions_dir` = `session_dir_override` (`--session-dir`, else the
/// `sessionDir` setting passed by the caller) or `<agent_dir>/sessions`; `tmp_dir` =
/// `<agent_dir>/tmp`; `system_skills_dir` = `OXA_SYSTEM_SKILLS_DIR` (empty = none) or
/// [`crate::OMARCHY_SKILLS_DIR`]. pi's directories (`~/.pi`) are never read. No `HOME` = `invalid` error.
///
/// Between the override and the default, `OXA_SESSION_DIR` is honoured (env table row ENV-005).
/// The frozen signature does not tell the flag from the setting, so `OXA_SESSION_DIR` ranks below the
/// override; pi ranks it above the setting.
pub fn dirs_from_env(env: &Env, session_dir_override: Option<&Path>) -> Result<Dirs, OxaError> {
    let home_raw = env.get(HOME).ok_or_else(|| {
        OxaError::new(
            codes::INVALID,
            "HOME is not set; oxa needs a home directory for its agent directory",
        )
        .with_hint("set HOME, or use OXA_AGENT_DIR to name the agent directory explicitly")
    })?;
    let home = PathBuf::from(home_raw);
    let agent_dir = match env.get(names::ENV_AGENT_DIR) {
        Some(v) => expand_tilde(v, &home),
        None => home.join(names::AGENT_DIR),
    };
    let sessions_dir = match session_dir_override {
        Some(p) => expand_tilde(&p.to_string_lossy(), &home),
        None => match env.get(SESSION_DIR) {
            Some(v) => expand_tilde(v, &home),
            None => agent_dir.join("sessions"),
        },
    };
    let tmp_dir = agent_dir.join("tmp");
    // O-152: `OXA_SYSTEM_SKILLS_DIR` names another system skills directory; set and empty = none.
    // (`Env::get` treats an empty value as unset, so the raw pairs are read here.)
    let raw = env
        .pairs()
        .iter()
        .rev()
        .find(|(k, _)| k == SYSTEM_SKILLS_DIR)
        .map(|(_, v)| v.as_str());
    let system_skills_dir = match raw {
        Some("") => None,
        Some(v) => Some(expand_tilde(v, &home)),
        // CT-10: a test process never reads the host's system skills, even when a spawn
        // helper forgot the variable.
        None if env.get(names::ENV_TEST_ISOLATION) == Some("1") => None,
        None => Some(PathBuf::from(crate::OMARCHY_SKILLS_DIR)),
    };
    Ok(Dirs {
        home,
        agent_dir,
        sessions_dir,
        tmp_dir,
        system_skills_dir,
    })
}

/// `~` and `~/rest` expand to `home`; anything else (including `~user`) is taken as written.
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}
