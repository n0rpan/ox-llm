//! Desktop app rows (desk slice Z12): the owner settings of `oxa-desk`. They sit in the Display
//! group under their own heading, so the terminal menu, the web page and the desktop settings pane
//! all show them from this one table. The terminal UI reads none of them.

use super::{d, Apply, Control, Descriptor, Group, Unit, WebEdit};
use crate::schema::Kind;

const G: Group = Group::Display;
const DESK: &str = "Desktop app";

/// The desktop app rows, in display order. Every one is user-only: a project file does not
/// restyle the owner's window. Ranges are advice, never a refusal.
#[rustfmt::skip]
pub(super) const ENTRIES: &[Descriptor] = &[
    d("oxa.desk.font", G, DESK, "Desktop font", "Font family of the desktop app. Unset uses the system font.").key(Kind::String, "null")
        .text().user_only().apply(Apply::Now).web(WebEdit::Edit),
    d("oxa.desk.fontSize", G, DESK, "Desktop font size", "Text size of the desktop app, in pixels.").key(Kind::Number, "14")
        .control(Control::Number).advise(8, 40, 1, "outside 8 to 40 pixels, text may not fit the window")
        .user_only().apply(Apply::Now).web(WebEdit::Edit),
    d("oxa.desk.blinkCursor", G, DESK, "Blinking cursor", "Blink the text cursor in the desktop app. Off keeps an idle window still.").key(Kind::Bool, "false")
        .toggle().user_only().apply(Apply::Now).web(WebEdit::Edit),
    d("oxa.desk.keepTranscripts", G, DESK, "Transcripts kept loaded", "Threads the desktop app keeps in memory; older ones reload when opened.").key(Kind::Number, "30")
        .number(Unit::Count).advise_from(1, 1, "the open thread and a thread with a pending question always stay loaded")
        .user_only().apply(Apply::Now).web(WebEdit::Edit),
];

#[cfg(test)]
mod tests {
    use super::super::{control_fits, descriptor};
    use super::*;
    use crate::descriptor_value::check;

    /// Each row declares its own key, is an owner (user-only) setting, and its schema default
    /// passes the row's own check, so the defaults are values the settings editors accept.
    #[test]
    fn desk_rows_are_owner_settings_with_valid_defaults() {
        for row in ENTRIES {
            let info = row
                .schema
                .unwrap_or_else(|| panic!("{} declares no key", row.key));
            assert!(row.key.starts_with("oxa.desk."), "{}", row.key);
            assert!(row.user_only && !row.hidden && !row.secret, "{}", row.key);
            assert!(
                control_fits(row.control, info.kind),
                "{}: control and kind differ",
                row.key
            );
            assert_eq!(descriptor(row.key), Some(row));
            let default: serde_json::Value = serde_json::from_str(info.default).expect("default is JSON");
            if !default.is_null() {
                assert_eq!(
                    check(row, &default),
                    Ok(None),
                    "{}: default outside its own range",
                    row.key
                );
            }
        }
    }

    /// The owner's file sets a desk key; a project file cannot (the value falls back).
    #[test]
    fn a_project_file_cannot_set_a_desk_key() {
        use serde_json::json;
        let user = crate::Settings::from_values(json!({"oxa": {"desk": {"keepTranscripts": 80}}}), json!({}));
        assert_eq!(user.u32("oxa.desk.keepTranscripts", 30), 80);
        assert!(user.diagnostics().is_empty(), "{:?}", user.diagnostics());
        let project =
            crate::Settings::from_values(json!({}), json!({"oxa": {"desk": {"blinkCursor": true}}}));
        assert!(!project.bool("oxa.desk.blinkCursor", false));
    }
}
