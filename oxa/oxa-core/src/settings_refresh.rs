//! L1: a changed settings file reaches the session at the start of its next run, without
//! `/reload`.
//!
//! On by default; `oxa.liveSettings: false` turns it off (read from the session settings).
//! The check is one `stat` per file (user, project) compared with the values seen last
//! (mtime, size, inode). While they match, nothing is read or parsed. On a change the files
//! are parsed once and the result replaces `st.settings` (the path `/reload` uses for
//! settings, without resources, hook reload or hook events). A file that no longer parses
//! keeps the previous settings and gives one warning. The prompt prefix is touched only when
//! a key that feeds it changed (`prompt_inputs`), so an unrelated save keeps the provider
//! cache. Keys fixed when an epoch opens (the rest of the prefix, codemode drivers) still
//! wait for `/reload`.
use crate::SessionState;
use oxa_config::{Dirs, Settings};
use oxa_proto::{AgentEvent, EventSink, Level, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

/// What a stat says about one file: `None` = absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
    ino: u64,
}

type Stamps = [Option<Stamp>; 2];

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some(Stamp {
        mtime: m.modified().ok(),
        len: m.len(),
        ino,
    })
}

/// The stamps of the user and the project settings file. The project file is stamped whatever
/// its trust (a stat only); trust is decided when the files are read.
pub(crate) fn stamps(dirs: &Dirs, cwd: &Path) -> Stamps {
    [
        stamp(&dirs.settings_file()),
        stamp(&dirs.project_settings_file(cwd)),
    ]
}

/// The stamps seen last. `None` until the first check of a session, which reads the files once
/// and compares them with the settings the session loaded at start (an edit made between that
/// load and the first run must not be lost).
#[derive(Debug, Default)]
pub(crate) struct Watch {
    seen: Option<Stamps>,
}

/// `/reload` has just read the files: what it stamped before reading is the new baseline.
pub(crate) fn resync(st: &SessionState, stamps: Stamps) {
    if let Ok(mut w) = st.shared.gate.settings_watch.lock() {
        w.seen = Some(stamps);
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
    now: Stamps,
    current: &Settings,
    load: &mut dyn FnMut() -> Settings,
) -> Refresh {
    let first = match watch.seen.replace(now.clone()) {
        None => true,
        Some(seen) if seen == now => return Refresh::Unchanged,
        Some(_) => false,
    };
    let fresh = load();
    if fresh.layer_failed() {
        // Broken at start: the start already warned.
        if first {
            return Refresh::Unchanged;
        }
        let warnings = fresh
            .diagnostics()
            .iter()
            .map(|d| format!("{}: {}", d.source, d.message))
            .collect();
        return Refresh::Kept(warnings);
    }
    // What the session set for itself in memory stays over the files.
    let fresh = fresh.with_overrides_of(current);
    let changed = changed_keys(current, &fresh);
    if first && changed.is_empty() {
        return Refresh::Unchanged;
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
    let now = stamps(&st.dirs, &st.cwd);
    let outcome = {
        let Ok(mut watch) = st.shared.gate.settings_watch.lock() else {
            return;
        };
        let (dirs, cwd) = (st.dirs.clone(), st.cwd.clone());
        // Trust is read from its store only when a file really changed.
        let mut load = || Settings::load(&dirs, &cwd, crate::epoch::project_trusted(st));
        check(&mut watch, now, &st.settings, &mut load)
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
                return;
            }
            let prev_enabled = st.settings.compaction().enabled;
            let budget = changed.iter().any(|k| touches_budget(k));
            for d in settings.diagnostics() {
                notice(sink, d.level, format!("{}: {}", d.source, d.message));
            }
            st.settings = settings;
            notice(
                sink,
                Level::Info,
                format!("settings changed: {}", changed.join(", ")),
            );
            if prompt {
                if let Err(e) = crate::epoch::open_epoch_at_run_start(st, "settings", sink) {
                    notice(
                        sink,
                        Level::Warning,
                        format!("settings changed but the prompt was not rebuilt: {e}"),
                    );
                }
            }
            crate::tune::refresh(st);
            if budget {
                crate::session::refresh_budget(st, Some(prev_enabled), sink);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let mut load = || {
                self.reads.set(self.reads.get() + 1);
                Settings::load(&self.dirs, &self.cwd, false)
            };
            check(watch, stamps(&self.dirs, &self.cwd), current, &mut load)
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
        // Only the first check reads (the baseline against the start settings).
        assert_eq!(f.reads.get(), 1);
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
        // The baseline read of the first check, and the one read of the change.
        assert_eq!(f.reads.get(), 2);
    }

    #[test]
    fn broken_file_keeps_old_settings_with_warning() {
        let f = Fixture::new("broken", r#"{"theme":"dark"}"#);
        let cur = Settings::load(&f.dirs, &f.cwd, false);
        let mut w = Watch::default();
        f.run(&mut w, &cur);
        f.write(r#"{"theme": "#);
        match f.run(&mut w, &cur) {
            Refresh::Kept(warnings) => assert_eq!(warnings.len(), 1),
            other => panic!("{other:?}"),
        }
        // The broken file is reported once, not at every turn.
        assert_eq!(f.run(&mut w, &cur), Refresh::Unchanged);
        // The baseline read of the first check, and the one read of the change.
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
        // The baseline read of the first check, and the one read of the change.
        assert_eq!(f.reads.get(), 2);
    }
}
