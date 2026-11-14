//! B4 (wave H lane `harden`): project files that run outside the sandbox load only as the user
//! approved them.
//!
//! A trusted project's executable files (`oxa_perm::approval_files`: project settings' hooks
//! and commands, `mcp.json`, extensions, skills, prompt templates, `SYSTEM.md`) are compared
//! with the content hashes recorded in `trust.json` when the user approved them. A new or
//! changed file needs a fresh explicit approval: interactive, the trust question again, listing
//! each file and what it would run; print / JSON / RPC, the project's resources are skipped
//! with one notice naming the files and the way to approve. An extension's `project_trust`
//! answer and a saved decision accept only content that matches the record (the first record
//! of a project is taken when nothing was recorded yet); `--approve` (this run) and
//! `defaultProjectTrust: "always"` are the user's own blanket answers and load as they are.
//! pi trusts a directory, not its content: oxa is stricter here because the model can write
//! these files and has the network.
use oxa_config::{Dirs, Env, Keymap};
use oxa_interactive::{ask_trust_interactive, TrustChoice};
use oxa_perm::{Approval, TrustStore};
use oxa_proto::{Level, OxaError};

use std::path::Path;

pub(crate) struct Check<'a> {
    pub env: &'a Env,
    pub dirs: &'a Dirs,
    pub cwd: &'a Path,
    pub store: &'a mut TrustStore,
    /// `--approve` on this run.
    pub via_flag: bool,
    /// `defaultProjectTrust: "always"` decided (no saved decision, no extension answer).
    pub via_default: bool,
    /// The user answered the trust question in this start.
    pub asked_now: bool,
    pub interactive: bool,
    pub notices: &'a mut Vec<(Level, String)>,
}

/// The notice when changed files are not loaded (R6 shape).
pub(crate) fn not_loaded_text(cwd: &Path, lines: &[String]) -> String {
    format!(
        "Project config not loaded: changed or new since you approved {}: {}\nFix: start oxa there interactively and approve the change, or pass --approve for this run",
        cwd.display(),
        lines.join(", ")
    )
}

/// Whether the trusted project's resources load. Records the approval where the user gave it.
pub(crate) fn check(c: Check<'_>) -> Result<bool, OxaError> {
    let file = c.dirs.trust_file();
    if c.via_default {
        return Ok(true);
    }
    if c.via_flag || c.asked_now {
        c.store.approve(&file, c.cwd)?;
        return Ok(true);
    }
    match c.store.approval(c.cwd) {
        Approval::Matches => Ok(true),
        Approval::NoRecord => {
            c.store.approve(&file, c.cwd)?;
            Ok(true)
        }
        Approval::Changed(changed) => {
            let lines: Vec<String> = changed.iter().map(|ch| ch.line()).collect();
            if !c.interactive {
                c.notices.push((Level::Warning, not_loaded_text(c.cwd, &lines)));
                return Ok(false);
            }
            let (keymap, _) = Keymap::load(c.dirs);
            let choice = ask_trust_interactive(c.env.pairs(), &keymap, &c.cwd.display().to_string(), &lines)?;
            match choice {
                Some(TrustChoice::Trust) => {
                    c.store.save(&file, c.cwd, true)?;
                    c.store.approve(&file, c.cwd)?;
                    Ok(true)
                }
                Some(TrustChoice::TrustParent) => {
                    c.store.save(&file, c.cwd.parent().unwrap_or(c.cwd), true)?;
                    c.store.approve(&file, c.cwd)?;
                    Ok(true)
                }
                Some(TrustChoice::NoTrust) => {
                    c.store.save(&file, c.cwd, false)?;
                    Ok(false)
                }
                Some(TrustChoice::Skip) | None => {
                    c.notices.push((Level::Warning, not_loaded_text(c.cwd, &lines)));
                    Ok(false)
                }
            }
        }
    }
}
