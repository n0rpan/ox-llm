//! B2 / B3 (wave H lane `harden`): the bash sandbox's view of the machine, from the user's
//! environment and the USER settings only (a project file cannot widen the sandbox).
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::Level;
use oxa_sandbox::{
    expand_home, is_home_or_above, Layout, LayoutInputs, SETTING_ALLOW_HOME_WRITE, SETTING_READ_PATHS,
    SETTING_WRITE_PATHS,
};
use std::path::Path;

/// The startup warning when oxa runs in `$HOME`, `/` or a parent of `$HOME` (R6 shape).
pub(crate) fn home_root_warning(cwd: &Path) -> String {
    format!(
        "Bash has no writable directory: oxa was started in {}, which is your home directory or one of its parents\nFix: start oxa in a project directory, or set \"{SETTING_ALLOW_HOME_WRITE}\": true in the user settings.json",
        cwd.display()
    )
}

/// The layout for this process; pushes the B3 startup warning when it applies.
pub(crate) fn layout(env: &Env, dirs: &Dirs, cwd: &Path, notices: &mut Vec<(Level, String)>) -> Layout {
    let user = Settings::load(dirs, cwd, false);
    let paths = |key: &str| {
        user.list(key)
            .iter()
            .map(|raw| expand_home(raw, &dirs.home))
            .collect::<Vec<_>>()
    };
    let allow_home_write = user.bool(SETTING_ALLOW_HOME_WRITE, false);
    if is_home_or_above(cwd, &dirs.home) && !allow_home_write {
        notices.push((Level::Warning, home_root_warning(cwd)));
    }
    Layout::build(&LayoutInputs {
        home: dirs.home.clone(),
        agent_dir: Some(dirs.agent_dir.clone()),
        path_var: env.get("PATH").unwrap_or("").to_string(),
        env: env.pairs().to_vec(),
        read_extra: paths(SETTING_READ_PATHS),
        write_extra: paths(SETTING_WRITE_PATHS),
        allow_home_write,
    })
}
