//! The environment note (`oxa.envNote`, on under profile `local`): what the model cannot know
//! by itself, the date, time and time zone, and the session's runtime facts.
//!
//! Cache rule (W1/W3): nothing here goes into the frozen prefix. The note is appended to the
//! END of the user message a prompt adds, and stored in that entry's wire bytes, so every
//! later request and a resume replay it unchanged: history bytes never change, only new
//! bytes are appended. The first prompt of a session (and the first after the wire lost the
//! block, e.g. to a compaction, or after midnight) carries the full [`ENV_OPEN`] block; every
//! other prompt carries the short time line ([`TIME_NOTE`]).
use crate::SessionState;
use oxa_proto::{Profile, Role, WireMsg};
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

/// The typed text of a user message without a trailing note (for the editor refill).
pub fn strip_note(content: &str) -> &str {
    if let Some(i) = content.rfind(&format!("\n\n{ENV_OPEN}\n")) {
        if content.ends_with(ENV_CLOSE) {
            return &content[..i];
        }
    }
    if let Some(i) = content.rfind("\n\n[local time ") {
        if content.ends_with(']') && !content[i + 2..].contains('\n') {
            return &content[..i];
        }
    }
    content
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

/// Whether the note is on for this session (`oxa.envNote`; unset = on under `local`).
pub fn enabled(st: &SessionState) -> bool {
    st.settings
        .bool("oxa.envNote", st.stats.profile == Profile::Local)
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
    }
}

/// The user message `msg` with the note appended when the note is on (else `msg` unchanged).
pub fn with_note(st: &SessionState, msg: WireMsg) -> WireMsg {
    if !enabled(st) {
        return msg;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let Some(t) = oxa_session::local_time(now) else {
        return msg;
    };
    let note = note_for(st.transcript.wire(), &t, &facts(st));
    WireMsg::user(append_note(msg.content.as_deref().unwrap_or(""), &note))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn strip_returns_the_typed_text() {
        let full = full_block(&t("2026-11-14", "13:42"), &f());
        assert_eq!(strip_note(&append_note("hi\nthere", &full)), "hi\nthere");
        assert_eq!(strip_note(&append_note("hi", "[local time 13:42 CET]")), "hi");
        assert_eq!(strip_note("plain [local time] text"), "plain [local time] text");
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
