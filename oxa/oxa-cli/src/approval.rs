//! Project trust policy after a positive trust decision. Folder trust reports changes;
//! content trust keeps the existing explicit re-approval path.
use oxa_config::{Dirs, Env, Keymap, Settings};
use oxa_interactive::{ask_trust_interactive, TrustChoice};
use oxa_perm::{Approval, TrustStore};
use oxa_proto::{Level, OxaError};

use std::{io::Write, path::Path};

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
    pub content: bool,
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
    if !c.content {
        if let Some(text) = c.store.folder_changes_notice(c.cwd) {
            c.notices.push((Level::Warning, text));
        }
        if c.via_flag || c.asked_now {
            c.store.approve(&file, c.cwd)?;
        } else {
            c.store.record_baseline(&file, c.cwd)?;
        }
        return Ok(true);
    }
    if c.via_default && !c.via_flag {
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

/// Subcommands that have always required a saved decision (no prompt or extension hook).
pub(crate) fn saved_project_resources(
    dirs: &Dirs,
    cwd: &Path,
    err: &mut dyn Write,
) -> Result<bool, OxaError> {
    let store = TrustStore::load(&dirs.trust_file());
    if store.lookup(cwd) != Some(true) {
        return Ok(false);
    }
    if Settings::load(dirs, cwd, false).project_trust_requires_content() {
        if let Approval::Changed(changed) = store.approval(cwd) {
            let paths: Vec<_> = changed.iter().map(|ch| ch.path_line()).collect();
            writeln!(err, "{}", not_loaded_text(cwd, &paths))
                .map_err(|e| OxaError::io("write output", &e))?;
            return Ok(false);
        }
    } else if let Some(text) = store.folder_changes_notice(cwd) {
        writeln!(err, "Warning: {text}").map_err(|e| OxaError::io("write output", &e))?;
    }
    Ok(true)
}

pub(crate) fn approval_fix(cwd: &Path) -> String {
    format!(
        "start oxa interactively in {} and approve the project (use /trust to change a saved decision)",
        cwd.display()
    )
}
