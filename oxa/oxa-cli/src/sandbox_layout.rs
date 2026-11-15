//! B2 / B3 (wave H lane `harden`): the bash sandbox's view of the machine, from the user's
//! environment and the USER settings only (a project file cannot widen the sandbox).
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::Level;
use oxa_sandbox::{
    expand_home, is_home_itself, is_home_or_above, Layout, LayoutInputs, SETTING_ALLOW_HOME_WRITE,
    SETTING_ALLOW_PROJECT_CONFIG_WRITE, SETTING_READ_PATHS, SETTING_WRITE_PATHS,
};
use std::path::Path;

/// What stays protected in a home workspace (O-199), in the user's words.
const HOME_PROTECTED_SUMMARY: &str = "the home folder itself, every dotfile and dot-folder (.bashrc, .profile, .config, .local/bin, .ssh, .oxa, ...) and ~/bin are read-only; keys and tokens are hidden; bash cannot create new top-level folders in ~, so create one with the file tools or outside oxa";

/// The startup warning when oxa runs in `$HOME`, `/` or a parent of `$HOME` without a home
/// workspace (R6 shape).
pub(crate) fn home_root_warning(cwd: &Path, home_itself: bool) -> String {
    if home_itself {
        format!(
            "Bash has no writable directory: oxa was started in {}, which is your home directory\nFix: start oxa in a project directory, or set \"{SETTING_ALLOW_HOME_WRITE}\": true in the user settings.json",
            cwd.display()
        )
    } else {
        format!(
            "Bash has no writable directory: oxa was started in {}, which is a parent of your home directory or /\nFix: start oxa in a project directory",
            cwd.display()
        )
    }
}

/// The startup notice of a home workspace (O-199).
pub(crate) fn home_workspace_notice() -> String {
    format!("workspace = your home directory: bash can write in its folders and files (protected: {HOME_PROTECTED_SUMMARY})")
}

/// The extra line of the interactive start question in `$HOME` (`home_cwd`).
pub(crate) fn home_confirm_effect() -> String {
    format!("If you go on, your home directory is the workspace: bash can write in its folders and files (protected: {HOME_PROTECTED_SUMMARY}).")
}

/// The layout for this process; pushes the B3 startup warning or the O-199 home workspace
/// notice when they apply. `confirmed` = the interactive start in `$HOME` itself was confirmed
/// (`home_cwd::check`); the user setting [`SETTING_ALLOW_HOME_WRITE`] does the same in every
/// mode. Neither ever makes `/` or a parent of `$HOME` writable.
pub(crate) fn layout(
    env: &Env,
    dirs: &Dirs,
    cwd: &Path,
    confirmed: bool,
    notices: &mut Vec<(Level, String)>,
) -> Layout {
    let user = Settings::load(dirs, cwd, false);
    let paths = |key: &str| {
        user.list(key)
            .iter()
            .map(|raw| expand_home(raw, &dirs.home))
            .collect::<Vec<_>>()
    };
    let resolved = cwd.canonicalize().ok();
    let home_root = is_home_or_above(cwd, &dirs.home)
        || resolved
            .as_deref()
            .is_some_and(|r| is_home_or_above(r, &dirs.home));
    let home_itself = is_home_itself(cwd, &dirs.home);
    let allow_home_write = home_itself && (confirmed || user.bool(SETTING_ALLOW_HOME_WRITE, false));
    if home_root && allow_home_write {
        notices.push((Level::Info, home_workspace_notice()));
    } else if home_root {
        notices.push((Level::Warning, home_root_warning(cwd, home_itself)));
    }
    let mut layout = Layout::build(&LayoutInputs {
        home: dirs.home.clone(),
        agent_dir: Some(dirs.agent_dir.clone()),
        path_var: env.get("PATH").unwrap_or("").to_string(),
        env: env.pairs().to_vec(),
        read_extra: paths(SETTING_READ_PATHS),
        write_extra: paths(SETTING_WRITE_PATHS),
        allow_home_write,
    });
    layout.allow_project_config_write = user.bool(SETTING_ALLOW_PROJECT_CONFIG_WRITE, false);
    if layout.allow_project_config_write {
        notices.push((
            Level::Warning,
            "oxa.sandbox.allowProjectConfigWrite is on: plain git merge/pull/checkout/rebase may write the project's .oxa and .agents/skills".to_string(),
        ));
    }
    layout.mask_runtime_ipc = true;
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_config::dirs_from_env;
    use oxa_fake::TestEnv;

    fn setup(env: &TestEnv) -> (Env, Dirs) {
        let owned = env.envs("http://127.0.0.1:9");
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let e = Env::from_pairs(&pairs);
        let dirs = dirs_from_env(&e, None).expect("dirs");
        (e, dirs)
    }

    #[test]
    fn a_confirmed_start_in_home_is_a_workspace_with_one_notice() {
        let env = TestEnv::new();
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.home(), true, &mut notices);
        assert!(l.allow_home_write);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].0, Level::Info);
        assert!(
            notices[0].1.starts_with("workspace = your home directory"),
            "{notices:?}"
        );
        // Not confirmed, no setting: read-only and the warning.
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.home(), false, &mut notices);
        assert!(!l.allow_home_write);
        assert!(
            notices[0].1.starts_with("Bash has no writable directory"),
            "{notices:?}"
        );
    }

    #[test]
    fn a_parent_of_home_is_never_a_workspace() {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"sandbox":{"allowHomeWrite":true}}}"#,
        );
        let (e, dirs) = setup(&env);
        for cwd in [env.root().to_path_buf(), std::path::PathBuf::from("/")] {
            let mut notices = Vec::new();
            let l = layout(&e, &dirs, &cwd, true, &mut notices);
            assert!(!l.allow_home_write, "{cwd:?}");
            assert!(notices
                .iter()
                .any(|(_, t)| t.contains("a parent of your home directory")));
        }
        // A project below home: never a home workspace, no notice.
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.work(), true, &mut notices);
        assert!(!l.allow_home_write);
        assert!(notices.is_empty(), "{notices:?}");
    }
}
