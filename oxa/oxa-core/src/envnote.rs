//! The environment note (`oxa.envNote`, on by default): what the model cannot know
//! by itself, the date, time and time zone, and the session's runtime facts.
//!
//! Cache rule (W1/W3): nothing here goes into the frozen prefix. The note is appended to the
//! END of the user message a prompt adds, and stored in that entry's wire bytes, so every
//! later request and a resume replay it unchanged: history bytes never change, only new
//! bytes are appended. The first prompt of a session (and the first after the wire lost the
//! block, e.g. to a compaction, or after midnight) carries the full [`ENV_OPEN`] block; every
//! other prompt carries the short time line ([`TIME_NOTE`]).
//!
//! Provenance: the prompt stores the note it appended as a [`Part::EnvNote`] of the same entry,
//! so the editor refill and the display drop exactly that suffix ([`strip_note`]) and nothing a
//! user typed (a literal `<env>` block, an older session, `envNote` off).
use crate::SessionState;
use oxa_proto::{Part, Role, WireMsg};
use oxa_session::LocalTime;

/// First line of the full block; a wire message containing it carries the date.
pub const ENV_OPEN: &str = "<env>";
pub const ENV_CLOSE: &str = "</env>";
/// The short per-prompt note: `{hm}`, `{zone}`.
pub const TIME_NOTE: &str = "[local time {hm} {zone}]";

/// The runtime facts of the full block that do not depend on the clock.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvFacts {
    /// `nb_NO` (from `LC_ALL` / `LC_MESSAGES` / `LANG`); empty = not set or `C`/`POSIX`.
    pub locale: String,
    /// `Linux 7.3.0-60-omarchy`; empty = unknown.
    pub kernel: String,
    pub model: String,
    pub n_ctx: u32,
    pub thinking: String,
    pub mode: String,
    pub bash_network: bool,
    pub offline: bool,
    /// No OS sandbox on this platform: bash runs with the user's rights.
    pub no_os_sandbox: bool,
}

/// The date line's `{weekday} {date}` (the key that says whether the wire already has today).
fn date_key(t: &LocalTime) -> String {
    format!("Date: {} {}", t.weekday, t.date)
}

/// The full block.
pub fn full_block(t: &LocalTime, f: &EnvFacts) -> String {
    let mut date = format!(
        "{}, local time {} {} ({})",
        date_key(t),
        t.hm,
        t.zone_label(),
        t.utc_offset()
    );
    if !f.locale.is_empty() {
        date.push_str(&format!(", locale {}", f.locale));
    }
    let mut lines = vec![ENV_OPEN.to_string(), date];
    if !f.kernel.is_empty() {
        lines.push(format!("Kernel: {}", f.kernel));
    }
    let mut model = format!("Model: {}", f.model);
    if f.n_ctx > 0 {
        model.push_str(&format!(", context {} tokens", f.n_ctx));
    }
    model.push_str(&format!(", thinking {}", f.thinking));
    lines.push(model);
    lines.push(format!(
        "Session: {} mode, bash network {}, offline {}",
        f.mode,
        if f.bash_network { "on" } else { "off" },
        if f.offline { "yes" } else { "no" }
    ));
    if f.no_os_sandbox {
        lines.push(
            "Sandbox: none. Commands you run execute with the user's own rights; there is no OS sandbox on this platform."
                .to_string(),
        );
    }
    lines.push(
        "Use this date for \"today\", \"latest\" and \"recent\" (also for web_search freshness).".to_string(),
    );
    lines.push(ENV_CLOSE.to_string());
    lines.join("\n")
}

/// The short note.
pub fn time_note(t: &LocalTime) -> String {
    TIME_NOTE
        .replace("{hm}", &t.hm)
        .replace("{zone}", &t.zone_label())
}

/// The note for a new user message after `wire`: the full block unless a user message on
/// `wire` already carries a full block with today's date, else the short note. Pure.
pub fn note_for(wire: &[WireMsg], t: &LocalTime, f: &EnvFacts) -> String {
    let key = date_key(t);
    let has_today = wire.iter().rev().filter(|m| m.role == Role::User).any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.contains(ENV_OPEN) && c.contains(&key))
    });
    if has_today {
        time_note(t)
    } else {
        full_block(t, f)
    }
}

/// `text` + the note, separated by a blank line (the typed text stays a byte prefix, so a
/// draft prefilled before the minute changed still matches up to the note).
pub fn append_note(text: &str, note: &str) -> String {
    if text.is_empty() {
        note.to_string()
    } else {
        format!("{text}\n\n{note}")
    }
}

/// The note oxa recorded for a stored user message (its [`Part::EnvNote`]), if it appended one.
pub fn recorded_note(parts: &[Part]) -> Option<&str> {
    parts.iter().rev().find_map(|p| match p {
        Part::EnvNote(note) => Some(note.as_str()),
        _ => None,
    })
}

/// The typed text of a stored user message (editor refill, display): its wire `content`
/// without the note recorded in `parts` (exactly the [`append_note`] suffix). Without a
/// recorded note, or when the content does not end with it, `content` is returned unchanged.
pub fn strip_note<'a>(content: &'a str, parts: &[Part]) -> &'a str {
    let Some(note) = recorded_note(parts) else {
        return content;
    };
    if content == note {
        return "";
    }
    content
        .strip_suffix(note)
        .and_then(|typed| typed.strip_suffix("\n\n"))
        .unwrap_or(content)
}

/// `LC_ALL` > `LC_MESSAGES` > `LANG`, without the encoding (`nb_NO.UTF-8` -> `nb_NO`).
pub fn locale_from(get: impl Fn(&str) -> Option<String>) -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = get(key).filter(|v| !v.trim().is_empty()) {
            let base = v.split(['.', '@']).next().unwrap_or("").trim().to_string();
            return if base == "C" || base == "POSIX" {
                String::new()
            } else {
                base
            };
        }
    }
    String::new()
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|r| format!("Linux {}", r.trim()))
        .unwrap_or_default()
}

/// `OXA_ENV_NOTE=0` (also `false`, `no`, `off`) / `=1` (`true`, `yes`, `on`) overrides the
/// setting for this process (the PARITY harness turns the note off: it checks pi's bytes).
pub const ENV_SWITCH: &str = "OXA_ENV_NOTE";

/// Whether the note is on for this session: [`ENV_SWITCH`], else `oxa.envNote` (unset = on).
pub fn enabled(st: &SessionState) -> bool {
    let forced = std::env::var(ENV_SWITCH)
        .ok()
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => Some(false),
            "1" | "true" | "yes" | "on" => Some(true),
            _ => None,
        });
    forced.unwrap_or_else(|| st.settings.bool("oxa.envNote", true))
}

/// The facts of `st` now.
pub fn facts(st: &SessionState) -> EnvFacts {
    EnvFacts {
        locale: locale_from(|k| std::env::var(k).ok()),
        kernel: kernel(),
        model: st.model.clone(),
        n_ctx: st.info.n_ctx,
        thinking: st.thinking.as_str().to_string(),
        mode: st.mode.as_str().to_string(),
        bash_network: st.sandbox.network(),
        offline: oxa_proto::offline(),
        no_os_sandbox: !oxa_proto::OS_SANDBOX,
    }
}

thread_local! {
    static TEST_TIME: std::cell::RefCell<Option<LocalTime>> = const { std::cell::RefCell::new(None) };
}

/// Pin the environment-note clock on this thread for a test, restoring it even on panic.
/// Other threads and normal sessions keep using the real clock.
#[doc(hidden)]
pub fn with_time_for_tests<R>(time: LocalTime, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<LocalTime>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_TIME.with(|t| *t.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TEST_TIME.with(|t| t.replace(Some(time))));
    run()
}

/// The note a user message added now gets; `None` when the note is off (or the clock is
/// unknown).
fn note_now(st: &SessionState) -> Option<String> {
    if !enabled(st) {
        return None;
    }
    let t = TEST_TIME.with(|t| t.borrow().clone()).or_else(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        oxa_session::local_time(now)
    })?;
    Some(note_for(st.transcript.wire(), &t, &facts(st)))
}

/// The user message `msg` with the note appended when the note is on, and the note appended
/// (to store as [`Part::EnvNote`]); `(msg, None)` unchanged when it is off.
pub fn add_note(st: &SessionState, msg: WireMsg) -> (WireMsg, Option<String>) {
    match note_now(st) {
        Some(note) => (
            WireMsg::user(append_note(msg.content.as_deref().unwrap_or(""), &note)),
            Some(note),
        ),
        None => (msg, None),
    }
}

/// [`add_note`] without the provenance (the draft prefill: same bytes, never stored).
pub fn with_note(st: &SessionState, msg: WireMsg) -> WireMsg {
    add_note(st, msg).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_is_thread_local_and_restored_after_panic() {
        assert!(TEST_TIME.with(|v| v.borrow().is_none()));
        let outer = t("2026-01-02", "03:04");
        with_time_for_tests(outer.clone(), || {
            std::thread::spawn(|| assert!(TEST_TIME.with(|v| v.borrow().is_none())))
                .join()
                .unwrap();
            let result = std::panic::catch_unwind(|| {
                with_time_for_tests(t("2026-01-03", "03:05"), || {
                    assert_eq!(
                        TEST_TIME.with(|v| v.borrow().as_ref().unwrap().hm.clone()),
                        "03:05"
                    );
                    panic!("test clock unwinding");
                });
            });
            assert!(result.is_err());
            assert_eq!(TEST_TIME.with(|v| v.borrow().clone()), Some(outer));
        });
        assert!(TEST_TIME.with(|v| v.borrow().is_none()));
    }

    fn t(date: &str, hm: &str) -> LocalTime {
        LocalTime {
            date: date.into(),
            weekday: "Saturday",
            hm: hm.into(),
            zone: "CET".into(),
            offset_min: 60,
        }
    }

    fn f() -> EnvFacts {
        EnvFacts {
            locale: "nb_NO".into(),
            kernel: "Linux 7.3.0-60-omarchy".into(),
            model: "ornith-35b-a3b-local".into(),
            n_ctx: 131072,
            thinking: "medium".into(),
            mode: "workspace".into(),
            bash_network: false,
            offline: false,
            no_os_sandbox: false,
        }
    }

    #[test]
    fn the_full_block_reads_as_intended() {
        assert_eq!(
            full_block(&t("2026-11-14", "13:42"), &f()),
            "<env>\nDate: Saturday 2026-11-14, local time 13:42 CET (UTC+01:00), locale nb_NO\n\
             Kernel: Linux 7.3.0-60-omarchy\n\
             Model: ornith-35b-a3b-local, context 131072 tokens, thinking medium\n\
             Session: workspace mode, bash network off, offline no\n\
             Use this date for \"today\", \"latest\" and \"recent\" (also for web_search freshness).\n</env>"
        );
        assert_eq!(time_note(&t("2026-11-14", "09:05")), "[local time 09:05 CET]");
    }

    #[test]
    fn env_note_names_a_missing_sandbox_and_is_unchanged_with_one() {
        let with = full_block(&t("2026-11-14", "13:42"), &f());
        assert!(!with.contains("Sandbox:"));
        let none = full_block(
            &t("2026-11-14", "13:42"),
            &EnvFacts {
                no_os_sandbox: true,
                ..f()
            },
        );
        assert!(none.contains("Sandbox: none. Commands you run execute with the user's own rights"));
        assert_eq!(none.replace(
            "Sandbox: none. Commands you run execute with the user's own rights; there is no OS sandbox on this platform.\n", ""), with);
    }

    #[test]
    fn full_block_first_then_short_notes_then_full_again_on_a_new_day() {
        let mut wire: Vec<WireMsg> = Vec::new();
        let n1 = note_for(&wire, &t("2026-11-14", "13:42"), &f());
        assert!(n1.starts_with(ENV_OPEN));
        wire.push(WireMsg::user(append_note("latest news in norway?", &n1)));
        let n2 = note_for(&wire, &t("2026-11-14", "13:50"), &f());
        assert_eq!(n2, "[local time 13:50 CET]");
        wire.push(WireMsg::user(append_note("and sports?", &n2)));
        let n3 = note_for(&wire, &t("2026-11-15", "00:10"), &f());
        assert!(n3.starts_with(ENV_OPEN) && n3.contains("2026-11-15"));
        // A wire without the block (after a compaction) gets it again.
        let compacted = vec![WireMsg::user("summary"), wire[1].clone()];
        assert!(note_for(&compacted, &t("2026-11-14", "14:00"), &f()).starts_with(ENV_OPEN));
    }

    #[test]
    fn strip_removes_only_the_recorded_note() {
        let full = full_block(&t("2026-11-14", "13:42"), &f());
        let noted = |n: &str| vec![Part::EnvNote(n.to_string())];
        assert_eq!(
            strip_note(&append_note("hi\nthere", &full), &noted(&full)),
            "hi\nthere"
        );
        let short = "[local time 13:42 CET]";
        assert_eq!(strip_note(&append_note("hi", short), &noted(short)), "hi");
        assert_eq!(strip_note(&append_note("", short), &noted(short)), "");
        // A typed text that itself ends like a note keeps it: only the recorded suffix goes.
        let literal = "Explain this format:\n\n<env>\nexample\n</env>";
        assert_eq!(strip_note(&append_note(literal, short), &noted(short)), literal);
        // No recorded note (note off, an older session): the text as stored.
        assert_eq!(strip_note(literal, &[]), literal);
        assert_eq!(
            strip_note("hi\n\n[local time 13:42 CET]", &[]),
            "hi\n\n[local time 13:42 CET]"
        );
        // A recorded note that is not the suffix changes nothing.
        assert_eq!(strip_note("hi", &noted(short)), "hi");
    }

    #[test]
    fn locale_order_and_cleanup() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(locale_from(env(&[("LANG", "nb_NO.UTF-8")])), "nb_NO");
        assert_eq!(
            locale_from(env(&[("LANG", "en_US.UTF-8"), ("LC_ALL", "de_DE@euro")])),
            "de_DE"
        );
        assert_eq!(locale_from(env(&[("LANG", "C.UTF-8")])), "");
        assert_eq!(locale_from(env(&[])), "");
    }
}
