//! Desk Z07 (architect): the effects of an [`AppAction`](crate::AppAction) that need no terminal.
//!
//! Carrying out an action has two halves. One half is the front end's own state (the editor,
//! the selector, the drawn theme); that stays with the front end. The other half writes a
//! setting, a session file, the trust store or a report, and then tells the user what happened.
//! That half lives here, so the terminal driver and the desktop app run the same code and show
//! the same words.
//!
//! Every function takes plain inputs and answers with a [`Notice`] (or an error the caller
//! shows as one, see [`Notice::of`]). Nothing here reads a terminal, a `ViewModel` or a `Loop`.
use crate::{TrustChoice, TrustSaver};
use oxa_config::{Dirs, Settings};
use oxa_export::default_output_name;
use oxa_proto::{Level, OxaError, PermissionMode, Value};
use oxa_session::{SessionRef, SessionStore};
use std::path::{Path, PathBuf};

/// One line for the user: what an effect did, or why it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    pub text: String,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Notice {
        Notice {
            level: Level::Info,
            text: text.into(),
        }
    }

    pub fn warning(text: impl Into<String>) -> Notice {
        Notice {
            level: Level::Warning,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Notice {
        Notice {
            level: Level::Error,
            text: text.into(),
        }
    }

    /// The notice of a finished effect: its own on success, the error's text otherwise.
    pub fn of(result: Result<Notice, OxaError>) -> Notice {
        result.unwrap_or_else(|e| Notice::error(e.to_string()))
    }
}

/// The user setting that lists the keys of "always allow" (#1512 S2); user settings only.
const APPROVAL_ALWAYS: &str = "oxa.approval.always";

/// Save one setting to the user file (`project = false`) or the project file, and in memory.
pub fn save_setting(
    settings: &mut Settings,
    dirs: &Dirs,
    cwd: &Path,
    key: &str,
    value: Value,
    project: bool,
) -> Result<Notice, OxaError> {
    settings.set(dirs, cwd, key, value, project)?;
    Ok(Notice::info(format!("Saved {key}")))
}

/// #202: save `mode` as the user's `permissions.defaultMode`. Switching the running session
/// is the front end's part; this only makes the mode the default of the next one.
pub fn save_permission_mode(
    settings: &mut Settings,
    dirs: &Dirs,
    cwd: &Path,
    mode: PermissionMode,
) -> Result<Notice, OxaError> {
    let value = Value::String(mode.as_str().to_string());
    settings.set(dirs, cwd, "permissions.defaultMode", value, false)?;
    Ok(Notice::info(format!("Default mode saved: {}", mode.as_str())))
}

/// Save `name` as the user's `theme`. Loading and showing the theme is the front end's part.
pub fn persist_theme(
    settings: &mut Settings,
    dirs: &Dirs,
    cwd: &Path,
    name: &str,
) -> Result<Notice, OxaError> {
    settings.set(dirs, cwd, "theme", Value::String(name.to_string()), false)?;
    Ok(Notice::info(format!("Theme saved: {name}")))
}

/// #1512 S2: save `keys` to the user settings' `oxa.approval.always`, each key once. The list is
/// read from the user layer only, so a project's value is never copied into the user file.
pub fn append_always(
    settings: &mut Settings,
    dirs: &Dirs,
    _cwd: &Path,
    keys: &[String],
) -> Result<(), OxaError> {
    // Under the user file's write lock, merge into the list the FILE holds now: two sessions
    // saving at once keep each other's keys (#1772).
    settings.update_user(dirs, APPROVAL_ALWAYS, |current| {
        let mut list: Vec<String> = current
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for key in keys {
            if !list.contains(key) {
                list.push(key.clone());
            }
        }
        Ok(Value::Array(list.into_iter().map(Value::String).collect()))
    })
}

/// Rename a stored session that is **not** the open one: the name is appended to its file.
/// The open session is renamed by its own thread (`SessionCmd::SetName`), which owns the file.
pub fn rename_session(
    store: &dyn SessionStore,
    session: &SessionRef,
    name: &str,
) -> Result<Notice, OxaError> {
    let entries = store.load(session)?;
    let leaf = entries.last().map(|e| e.id.clone()).unwrap_or_default();
    oxa_session::set_name(store, session, &leaf, name)?;
    Ok(Notice::info(format!("Session renamed: {name}")))
}

/// pi `onDeleteSession`: remove a stored session and say what happened. The open session
/// (`open_id`) is never removed. An `Error` notice means the session is still there, or that
/// it went and its spill files did not.
pub fn delete_session(
    store: &dyn SessionStore,
    tmp_dir: &Path,
    open_id: &str,
    session: &SessionRef,
) -> Notice {
    if session.id == open_id {
        return Notice::error("Cannot delete the currently active session");
    }
    let removed = match oxa_session::remove_session_preserving_spills(store, tmp_dir, session) {
        Ok(removed) => removed,
        Err(e) => return Notice::error(format!("Failed to delete: {}", e.message)),
    };
    // P6: the session's spill files go with it (`<tmp_dir>/<id>`), so they do not outlive it,
    // unless a fork or clone of it still names them (#576).
    if let Err(e) = oxa_session::remove_spill_dir_unless_forked(store, tmp_dir, session) {
        return Notice::error(format!(
            "Session removed, but its spill files were not: {}",
            e.message
        ));
    }
    match removed {
        oxa_session::Removed::Trashed => Notice::info("Session moved to trash"),
        oxa_session::Removed::Deleted => Notice::info("Session deleted"),
    }
}

/// What `/export` leaves for the front end to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Export {
    /// Finished (a JSONL copy) or refused: show the notice.
    Done(Notice),
    /// HTML: send `SessionCmd::Export { path }`; the session thread writes it and reports.
    Html(PathBuf),
}

/// `/export [path]` (pi): HTML by default, JSONL when the path ends in `.jsonl` (a copy of
/// the session file). A relative path is taken from `cwd`; with no path the HTML goes to the
/// default name (`default_output_name`, as `--export` and RPC do). `source` is the session
/// file (`None` = the session is not saved). The destination is never the session file itself,
/// by path or by link.
pub fn export(source: Option<&Path>, session_id: &str, cwd: &Path, path: Option<PathBuf>) -> Export {
    use std::os::unix::fs::MetadataExt;

    let Some(source) = source else {
        return Export::Done(Notice::warning("/export: this session is not saved to a file."));
    };
    let target = path
        .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
        .unwrap_or_else(|| default_output_name(Some(source), session_id, cwd));
    let aliases_source = source == target
        || match (std::fs::metadata(source), std::fs::metadata(&target)) {
            (Ok(source), Ok(target)) => source.dev() == target.dev() && source.ino() == target.ino(),
            _ => false,
        };
    if aliases_source {
        return Export::Done(Notice::error(
            "/export: destination is the session file; choose a different path.",
        ));
    }
    if target.extension().is_some_and(|e| e == "jsonl") {
        return Export::Done(match std::fs::copy(source, &target) {
            Ok(_) => Notice::info(format!("Exported the session to {}", target.display())),
            Err(e) => {
                let err = OxaError::io(&format!("cannot write {}", target.display()), &e);
                Notice::error(err.to_string())
            }
        });
    }
    Export::Html(target)
}

/// `/bug`: write the local text bundle (the description, versions and `facts`; no transcript
/// text and no environment). Nothing leaves the machine. The front end supplies its own facts
/// (layout, session id, capabilities, block counts).
pub fn bug_report(dirs: &Dirs, description: &str, facts: &[String]) -> Result<Notice, OxaError> {
    let path = oxa_core::write_bug_report(dirs, description, facts)?;
    Ok(Notice::info(format!(
        "Bug report written to {} (review it before you share it)",
        path.display()
    )))
}

/// Save a `/trust` decision through the CLI's trust store. `Skip` saves nothing (`None`).
pub fn save_trust(save: &mut TrustSaver, choice: TrustChoice) -> Result<Option<Notice>, OxaError> {
    let what = match choice {
        TrustChoice::Skip => return Ok(None),
        TrustChoice::Trust => "This project is trusted",
        TrustChoice::TrustParent => "The parent directory is trusted",
        TrustChoice::NoTrust => "This project is not trusted",
    };
    save(choice)?;
    Ok(Some(Notice::info(format!(
        "{what}. Saved; it applies the next time oxa starts here."
    ))))
}

#[cfg(test)]
mod tests {
    //! The effects run with no terminal, no `Loop` and no session thread.
    use super::*;
    use oxa_fake::TestEnv;
    use serde_json::json;

    fn blank() -> Settings {
        Settings::from_values(json!({}), json!({}))
    }

    #[test]
    fn a_setting_is_saved_to_the_user_file_and_reported() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut settings = blank();

        let notice = save_setting(
            &mut settings,
            &dirs,
            &env.work(),
            "hideThinkingBlock",
            json!(true),
            false,
        )
        .unwrap();

        assert_eq!(notice, Notice::info("Saved hideThinkingBlock"));
        assert_eq!(settings.get("hideThinkingBlock"), Some(&json!(true)));
        let on_disk = Settings::load(&dirs, &env.work(), false);
        assert_eq!(on_disk.get("hideThinkingBlock"), Some(&json!(true)));
    }

    #[test]
    fn a_refused_setting_becomes_an_error_notice() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut settings = blank();

        // `oxa.sandbox.net` is user-only: a project file must not open the sandbox.
        let result = save_setting(
            &mut settings,
            &dirs,
            &env.work(),
            "oxa.sandbox.net",
            json!("on"),
            true,
        );

        let notice = Notice::of(result);
        assert_eq!(notice.level, Level::Error);
        assert!(notice.text.contains("user-only"), "{}", notice.text);
        assert_eq!(settings.get("oxa.sandbox.net"), blank().get("oxa.sandbox.net"));
    }

    #[test]
    fn the_theme_is_saved_under_its_key() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut settings = blank();

        persist_theme(&mut settings, &dirs, &env.work(), "dark").unwrap();

        let on_disk = Settings::load(&dirs, &env.work(), false);
        assert_eq!(on_disk.get("theme"), Some(&json!("dark")));
    }

    #[test]
    fn export_without_a_session_file_or_onto_it_is_refused() {
        let env = TestEnv::new();
        let source = env.work().join("source.jsonl");
        std::fs::write(&source, b"{}\n").unwrap();

        let unsaved = export(None, "id", &env.work(), None);
        assert!(matches!(unsaved, Export::Done(ref n) if n.level == Level::Warning));

        let onto_itself = export(Some(&source), "id", &env.work(), Some("source.jsonl".into()));
        assert!(matches!(onto_itself, Export::Done(ref n) if n.level == Level::Error));
        assert_eq!(std::fs::read(&source).unwrap(), b"{}\n");

        let copy = env.work().join("copy.jsonl");
        let copied = export(Some(&source), "id", &env.work(), Some(copy.clone()));
        assert!(matches!(copied, Export::Done(ref n) if n.level == Level::Info));
        assert_eq!(std::fs::read(&copy).unwrap(), b"{}\n");

        let html = env.work().join("out.html");
        let step = export(Some(&source), "id", &env.work(), Some(html.clone()));
        assert_eq!(step, Export::Html(html));
    }

    #[test]
    fn a_trust_decision_reaches_the_saver_and_skip_does_not() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let mut save: TrustSaver = Box::new(move |choice| {
            log.lock().unwrap().push(choice);
            Ok(())
        });

        assert_eq!(save_trust(&mut save, TrustChoice::Skip).unwrap(), None);
        let notice = save_trust(&mut save, TrustChoice::Trust).unwrap().unwrap();

        assert_eq!(
            notice.text,
            "This project is trusted. Saved; it applies the next time oxa starts here."
        );
        assert_eq!(*seen.lock().unwrap(), vec![TrustChoice::Trust]);
    }
}
