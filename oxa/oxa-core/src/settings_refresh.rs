//! L1: a changed settings file reaches the session at the start of its next run, without
//! `/reload`.
//!
//! On by default; `oxa.liveSettings: false` turns it off (read from the session settings).
//! The check is one `stat` per file loaded at startup compared with the values seen last
//! (mtime, size, inode). While they match, nothing is read or parsed. On a change the files
//! are parsed once and the result replaces `st.settings` (the path `/reload` uses for
//! settings, without resources, hook reload or hook events). A file that no longer parses
//! keeps the previous settings and gives one warning. The prompt prefix is touched only when
//! a key that feeds it changed (`prompt_inputs`), so an unrelated save keeps the provider
//! cache. Keys fixed when an epoch opens (the rest of the prefix, codemode drivers) still
//! wait for `/reload`.
use crate::SessionState;
use oxa_config::{Settings, SettingsFiles};
use oxa_proto::{AgentEvent, EventSink, Level, Value};
use std::collections::BTreeMap;
/// The last observation, including a change that was reported but could not be applied.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    seen: Option<SettingsFiles>,
}

/// `/reload` just read the files, so discard the observation from before that load.
pub(crate) fn resync(st: &SessionState) {
    if let Ok(mut w) = st.shared.gate.settings_watch.lock() {
        w.seen = None;
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Refresh {
    /// Same stamps: nothing was read.
    Unchanged,
    /// A file changed but does not parse: the old settings stay. Carries the warnings.
    Kept(Vec<String>),
    /// The new settings (diagnostics are those of the load).
    Applied {
        settings: Settings,
        changed: Vec<String>,
        prompt: bool,
    },
}

/// The pure core: compare stamps, and only on a difference call `load` once.
pub(crate) fn check(
    watch: &mut Watch,
    current: &Settings,
    load: &mut dyn FnMut(SettingsFiles, bool) -> Settings,
) -> Refresh {
    let Some(original) = current.files() else {
        return Refresh::Unchanged;
    };
    let now = original.observe();
    let previous = watch.seen.as_ref().unwrap_or(original);
    if *previous == now {
        return Refresh::Unchanged;
    }
    let project_changed = now.project_changed_since(original);
    watch.seen = Some(now.clone());
    let fresh = load(now, project_changed);
    if fresh.layer_failed() {
        let warnings = fresh
            .diagnostics()
            .iter()
            .map(|d| format!("{}: {}", d.source, d.message))
            .collect();
        return Refresh::Kept(warnings);
    }
    let mut changed = changed_keys(current, &fresh);
    // List patches can exclude an opt-in tool without changing the merged default list.
    if current.default_tools_edits() != fresh.default_tools_edits()
        && !changed.iter().any(|key| key == "defaultTools")
    {
        changed.push("defaultTools".to_string());
        changed.sort();
    }
    let prompt = prompt_inputs(current) != prompt_inputs(&fresh);
    Refresh::Applied {
        settings: fresh,
        changed,
        prompt,
    }
}

/// Everything that feeds the prefix from the settings (see `epoch::gather`, `prefix_opts`).
fn prompt_inputs(s: &Settings) -> (bool, Option<Vec<String>>, Option<Vec<String>>, Option<String>) {
    (
        s.diet_output(),
        s.default_tools(),
        s.default_tools_edits(),
        s.string("codemode.mode"),
    )
}

fn leaves(prefix: &str, v: &Value, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Object(m) if !m.is_empty() => {
            for (k, v) in m {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                leaves(&key, v, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), v.clone());
        }
    }
}

/// The dotted keys whose merged value differs, sorted. Values are never put in a notice.
fn changed_keys(old: &Settings, new: &Settings) -> Vec<String> {
    let (mut a, mut b) = (BTreeMap::new(), BTreeMap::new());
    leaves("", old.merged(), &mut a);
    leaves("", new.merged(), &mut b);
    let mut keys: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
    keys.sort();
    keys.dedup();
    keys.retain(|k| a.get(k) != b.get(k));
    keys
}

/// `oxa.liveSettings`: on by default (owner decision); `false` opts out.
pub(crate) fn enabled(s: &Settings) -> bool {
    s.bool("oxa.liveSettings", true)
}

/// Whether a changed key moves the budget policy (`refresh_budget`): the `compaction.*` keys
/// and `oxa.compaction.*` (the soft limit `oxa.compaction.softTokens` is read there).
fn touches_budget(key: &str) -> bool {
    key.starts_with("compaction.") || key.starts_with("oxa.compaction.")
}

/// Called at the start of a run (a prompt, or a queued run). Never fails and never blocks the
/// run. Does nothing unless `oxa.liveSettings` is on.
pub(crate) fn refresh_at_run_start(st: &mut SessionState, sink: &mut dyn EventSink) {
    if !enabled(&st.settings) {
        return;
    }
    let outcome = {
        let Ok(mut watch) = st.shared.gate.settings_watch.lock() else {
            return;
        };
        let mut load = |files, project_changed| {
            // An unchanged project layer keeps the effective launch trust, including
            // explicit one-run approval. Changed bytes still need content approval.
            let read_project = project_changed
                && matches!(
                    oxa_perm::TrustStore::load(&st.dirs.trust_file()).approval(&st.cwd),
                    oxa_perm::Approval::Matches
                );
            st.settings.refresh_files(files, read_project)
        };
        check(&mut watch, &st.settings, &mut load)
    };
    let notice =
        |sink: &mut dyn EventSink, level, text: String| sink.emit(&AgentEvent::Notice { level, text });
    match outcome {
        Refresh::Unchanged => {}
        Refresh::Kept(warnings) => {
            let why = warnings.join("; ");
            notice(
                sink,
                Level::Warning,
                format!("settings file changed but could not be read; keeping the previous settings ({why})"),
            );
        }
        Refresh::Applied {
            settings,
            changed,
            prompt,
        } => {
            if changed.is_empty() {
                st.settings = settings;
                return;
            }
            let prev_enabled = st.settings.compaction().enabled;
            let budget = changed.iter().any(|k| touches_budget(k));
            let retry = settings.retry().enabled;
            if retry != st.settings.retry().enabled {
                st.auto_retry = retry;
            }
            let steer = settings.steering_mode();
            let follow = settings.follow_up_mode();
            let steer = (steer != st.settings.steering_mode()).then_some(steer);
            let follow = (follow != st.settings.follow_up_mode()).then_some(follow);
            if steer.is_some() || follow.is_some() {
                st.shared.update_modes(steer, follow);
            }
            let mut message = format!("settings changed: {}", changed.join(", "));
            let mut level = Level::Info;
            for diagnostic in settings.diagnostics() {
                use std::fmt::Write;
                level = Level::Warning;
                let _ = write!(message, "\n{}: {}", diagnostic.source, diagnostic.message);
            }
            st.settings = settings;
            st.shared.publish_live_settings(&st.settings);
            notice(sink, level, message);
            if prompt {
                if let Err(e) = crate::epoch::open_epoch_at_run_start(st, "settings", sink) {
                    notice(
                        sink,
                        Level::Warning,
                        format!("settings changed but the prompt was not rebuilt: {e}"),
                    );
                }
            }
            if budget {
                crate::session::apply_budget_settings(st, Some(prev_enabled));
            } else {
                crate::tune::refresh(st);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_config::Dirs;
    use std::cell::Cell;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oxa-l1-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One user file in a temp dir; `reads` counts `Settings::load` calls (the seam).
    struct Fixture {
        dirs: Dirs,
        cwd: std::path::PathBuf,
        reads: Cell<u32>,
    }

    impl Fixture {
        fn new(name: &str, first: &str) -> Fixture {
            let root = dir(name);
            let dirs = Dirs::under(&root);
            std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
            std::fs::write(dirs.settings_file(), first).unwrap();
            Fixture {
                dirs,
                cwd: root.join("work"),
                reads: Cell::new(0),
            }
        }
        fn write(&self, text: &str) {
            std::fs::write(self.dirs.settings_file(), text).unwrap();
        }
        fn run(&self, watch: &mut Watch, current: &Settings) -> Refresh {
            let mut load = |files, project_changed| {
                self.reads.set(self.reads.get() + 1);
                current.refresh_files(files, project_changed)
            };
            check(watch, current, &mut load)
        }
    }

    #[test]
    fn unchanged_file_is_not_read() {
        let f = Fixture::new("same", r#"{"theme":"dark"}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false);
        let mut w = Watch::default();
        for _ in 0..5 {
            assert_eq!(f.run(&mut w, &cur), Refresh::Unchanged);
        }
        assert_eq!(f.reads.get(), 0);
    }

    #[test]
    fn edit_before_the_first_check_is_applied() {
        let f = Fixture::new("early", r#"{"theme":"dark"}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false);
        f.write(r#"{"theme":"light-theme"}"#);
        let mut w = Watch::default();
        match f.run(&mut w, &cur) {
            Refresh::Applied { changed, .. } => assert_eq!(changed, vec!["theme".to_string()]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refresh_keeps_in_memory_overrides() {
        let f = Fixture::new("override", r#"{"done":["make test"],"theme":"dark"}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false).with_override("done", Value::Array(Vec::new()));
        let mut w = Watch::default();
        f.run(&mut w, &cur);
        f.write(r#"{"done":["make test"],"theme":"light-theme"}"#);
        let Refresh::Applied {
            settings, changed, ..
        } = f.run(&mut w, &cur)
        else {
            panic!("expected a change");
        };
        assert_eq!(changed, vec!["theme".to_string()]);
        assert_eq!(settings.get("done"), Some(&Value::Array(Vec::new())));
    }

    #[test]
    fn user_edit_preserves_the_project_layer_admitted_at_launch() {
        let f = Fixture::new("launch-trust", r#"{"theme":"dark"}"#);
        let project = f.dirs.project_settings_file(&f.cwd);
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, r#"{"done":["make check"]}"#).unwrap();
        let current = Settings::load(&f.dirs, &f.cwd, true);
        f.write(r#"{"theme":"light-theme"}"#);
        let mut watch = Watch::default();
        let Refresh::Applied { settings, .. } = f.run(&mut watch, &current) else {
            panic!("expected user edit");
        };
        assert_eq!(settings.done_commands(), ["make check"]);

        std::fs::write(project, r#"{"done":["make another-check"]}"#).unwrap();
        let mut keep_project = |files, project_changed: bool| {
            assert!(project_changed);
            settings.refresh_files(files, false)
        };
        let Refresh::Applied {
            settings, changed, ..
        } = check(&mut watch, &settings, &mut keep_project)
        else {
            panic!("expected a changed stamp");
        };
        assert!(changed.is_empty());
        assert_eq!(settings.done_commands(), ["make check"]);
    }

    #[test]
    fn repaired_user_file_retries_the_unapplied_project_edit() {
        let fixture = Fixture::new("repaired-user", r#"{"theme":"dark"}"#);
        let project = fixture.dirs.project_settings_file(&fixture.cwd);
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, r#"{"theme":"light"}"#).unwrap();
        let current = Settings::load(&fixture.dirs, &fixture.cwd, true);
        let mut watch = Watch::default();

        fixture.write("{");
        std::fs::write(project, r#"{"theme":"light-theme"}"#).unwrap();
        assert!(matches!(fixture.run(&mut watch, &current), Refresh::Kept(_)));
        fixture.write(r#"{"theme":"dark","quietStartup":true}"#);
        let Refresh::Applied { settings, .. } = fixture.run(&mut watch, &current) else {
            panic!("expected both valid layers to apply");
        };
        assert_eq!(settings.string("theme").as_deref(), Some("light-theme"));
    }

    #[test]
    fn tool_exclusion_applies_even_if_the_merged_tool_list_matches() {
        let f = Fixture::new("tool-exclusion", "{}");
        let current = Settings::load(&f.dirs, &f.cwd, false);
        f.write(r#"{"defaultTools":["-subagent"]}"#);
        let Refresh::Applied {
            settings,
            changed,
            prompt,
        } = f.run(&mut Watch::default(), &current)
        else {
            panic!("expected a tool selection change");
        };
        assert_eq!(
            settings.merged()["defaultTools"],
            current.merged()["defaultTools"]
        );
        assert_eq!(changed, ["defaultTools"]);
        assert!(prompt);
    }

    #[test]
    fn live_settings_are_on_by_default_and_can_opt_out() {
        assert!(enabled(&Settings::from_values(Value::Null, Value::Null)));
        let off = Settings::from_values(serde_json::json!({"oxa": {"liveSettings": false}}), Value::Null);
        assert!(!enabled(&off));
    }

    #[test]
    fn soft_tokens_change_moves_the_budget() {
        assert!(touches_budget("oxa.compaction.softTokens"));
        assert!(touches_budget("compaction.reserveTokens"));
        assert!(!touches_budget("theme"));
    }

    #[test]
    fn changed_display_key_applies_next_turn_without_prompt_change() {
        let f = Fixture::new("display", r#"{"theme":"dark"}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false);
        let mut w = Watch::default();
        f.run(&mut w, &cur);
        f.write(r#"{"theme":"light-theme"}"#);
        match f.run(&mut w, &cur) {
            Refresh::Applied { changed, prompt, .. } => {
                assert_eq!(changed, vec!["theme".to_string()]);
                assert!(!prompt);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(f.reads.get(), 1);
    }

    #[test]
    fn broken_file_keeps_old_settings_with_warning() {
        let f = Fixture::new("broken", r#"{"theme":"dark"}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false);
        let mut w = Watch::default();
        f.write(r#"{"theme": "#);
        match f.run(&mut w, &cur) {
            Refresh::Kept(warnings) => assert_eq!(warnings.len(), 1),
            other => panic!("{other:?}"),
        }
        // The broken file is reported once, not at every turn.
        assert_eq!(f.run(&mut w, &cur), Refresh::Unchanged);
        f.write(r#"["not a settings object"]"#);
        assert!(matches!(f.run(&mut w, &cur), Refresh::Kept(_)));
        assert_eq!(f.reads.get(), 2);
    }

    #[test]
    fn prompt_key_change_rebuilds_prompt_once() {
        let f = Fixture::new("prompt", r#"{"oxa":{"diet":{"output":false}}}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false);
        let mut w = Watch::default();
        f.run(&mut w, &cur);
        f.write(r#"{"oxa":{"diet":{"output":true}}}"#);
        let Refresh::Applied { settings, prompt, .. } = f.run(&mut w, &cur) else {
            panic!("expected a change");
        };
        assert!(prompt);
        // The next turn sees the same files: no second rebuild, no second read.
        assert_eq!(f.run(&mut w, &settings), Refresh::Unchanged);
        assert_eq!(f.reads.get(), 1);
    }
}
