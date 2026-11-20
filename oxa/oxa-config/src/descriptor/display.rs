//! Display group rows (issue #1489, design section 1.2): look, motion and alerts, full screen,
//! images and input. Keys and kinds are checked against the schema by the tests below.

use super::{d, Apply, Control, Descriptor, Group, Item, Unit, WebEdit};

const G: Group = Group::Display;
const LOOK: &str = "Look";
const MOTION: &str = "Motion and alerts";
const FULL: &str = "Full screen";
const IMAGES: &str = "Images";
const INPUT: &str = "Input";

/// The 30 Display rows, in display order. Hand-wrapped (not rustfmt'd) so each row stays two lines.
#[rustfmt::skip]
pub(super) const ENTRIES: &[Descriptor] = &[
    d("theme", G, LOOK, "Theme", "Colour theme of the terminal UI.")
        .control(Control::Theme).apply(Apply::Now).web(WebEdit::Edit),
    d("ui.skin", G, LOOK, "Skin", "Look of the interactive UI: oxa, plain (pi) or ASCII only.")
        .choice(&[("auto", ""), ("pi", "plain, as pi"), ("oxa", "oxa's look"), ("ascii", "ASCII only")])
        .apply(Apply::Now).web(WebEdit::Edit),
    d("tuiMode", G, LOOK, "Start-up layout", "Start in full screen or in the regular scrolling layout.")
        .choice(&[("fullscreen", "full screen"), ("regular", "regular scrolling layout")])
        .apply(Apply::Restart).web(WebEdit::Edit),
    d("quietStartup", G, LOOK, "Start-up listing", "Full listing, a short one, or the header only.")
        .choice(&[("false", "full listing"), ("true", "short listing"), ("header", "header only")])
        .apply(Apply::Now).web(WebEdit::Edit),
    d("hideThinkingBlock", G, LOOK, "Hide thinking", "Hide thinking blocks in the transcript.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),
    d("showCacheMissNotices", G, LOOK, "Cache miss notices", "Show a notice for each prompt-cache miss.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),
    d("outputPad", G, LOOK, "Output padding", "Padding of output blocks.")
        .control(Control::Number).hard(0, 1, 1, "0 or 1 only").apply(Apply::Now).web(WebEdit::Edit),
    d("editorPaddingX", G, LOOK, "Editor side padding", "Empty columns left and right of the editor.")
        .number(Unit::Cells).hard(0, 3, 1, "0 to 3 columns").apply(Apply::Now).web(WebEdit::Edit),
    d("markdown.codeBlockIndent", G, LOOK, "Code block indent", "Text put before each line of a code block.")
        .text().apply(Apply::Now).web(WebEdit::Edit),
    d("themes", G, LOOK, "Extra theme folders", "Theme files or folders to load.")
        .control(Control::List(Item::Path)).apply(Apply::Reload).web(WebEdit::Edit),

    d("ui.animation", G, MOTION, "Activity animation", "What moves on the activity line while a run is in progress.")
        .choice(&[("arcade", "animated scene"), ("minimal", ""), ("off", "no animation")])
        .apply(Apply::Now).web(WebEdit::Edit),
    d("ui.animationScene", G, MOTION, "Animation scene", "Scene of the arcade animation. Unset picks by terminal.")
        .choice(&[("graphics", ""), ("stack", ""), ("stars", ""), ("charge", "")])
        .apply(Apply::Now).web(WebEdit::Edit),
    d("ui.animationGraphics", G, MOTION, "Pixel-art animation", "Draw the scene as pixel art where the terminal can.")
        .choice(&[("auto", ""), ("off", "")]).apply(Apply::Restart).web(WebEdit::Edit),
    d("ui.reducedMotion", G, MOTION, "Reduced motion", "No animation; a static badge instead.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),
    d("ui.notify", G, MOTION, "Notifications", "Terminal notification when a run settles or a permission question opens.")
        .choice(&[("off", ""), ("unfocused", "when the window is not focused"), ("always", "")])
        .apply(Apply::Now).web(WebEdit::Edit),

    d("fullscreenScrollbar", G, FULL, "Scrollbar", "When the scrollbar shows in full screen.")
        .choice(&[("auto", ""), ("always", ""), ("hidden", "")]).apply(Apply::Now).web(WebEdit::Edit),
    d("fullscreenWheelScrollLines", G, FULL, "Wheel scroll speed", "Lines per wheel step; auto speeds up fast spins.")
        .control(Control::NumberOr("auto")).apply(Apply::Now).web(WebEdit::Edit),
    d("fullscreenCopyOnSelect", G, FULL, "Copy on select", "Copy a mouse selection when the button is released.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),
    d("fullscreenExitOutput", G, FULL, "Left on screen at exit", "The transcript, or only a hint how to resume.")
        .choice(&[("transcript", ""), ("resume-hint", "")]).apply(Apply::Now).web(WebEdit::Edit),
    d("terminal.clearOnShrink", G, FULL, "Clear on shrink", "Clear empty rows when the output gets shorter.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),

    d("terminal.showImages", G, IMAGES, "Show images", "Draw images in the terminal where it can.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),
    d("terminal.imageWidthCells", G, IMAGES, "Image width", "Width of drawn images.")
        .number(Unit::Cells).advise(10, 200, 1, "outside 10 to 200 cells, images may not fit the screen")
        .apply(Apply::Now).web(WebEdit::Edit),
    d("images.blockImages", G, IMAGES, "Never send images", "Do not send images to the model.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),

    d("autocompleteMaxVisible", G, INPUT, "Autocomplete rows", "Rows of the autocomplete list.")
        .number(Unit::Rows).advise(3, 20, 1, "outside 3 to 20 rows, the list may not fit the screen")
        .apply(Apply::Now).web(WebEdit::Edit),
    d("enableSkillCommands", G, INPUT, "Skills in the slash menu", "List skills in the slash-command autocomplete.")
        .toggle().apply(Apply::Now).web(WebEdit::Edit),
    d("doubleEscapeAction", G, INPUT, "Double Escape", "What a double Escape on an empty editor does.")
        .choice(&[("tree", ""), ("fork", ""), ("none", "")]).apply(Apply::Now).web(WebEdit::Edit),
    d("treeFilterMode", G, INPUT, "Tree opens with", "Filter the session tree starts with.")
        .choice(&[("default", ""), ("no-tools", ""), ("user-only", ""), ("labeled-only", ""), ("all", "")])
        .apply(Apply::Now).web(WebEdit::Edit),
    d("followUpMode", G, INPUT, "Queued follow-ups", "Send queued follow-up messages all at once or one at a time.")
        .choice(&[("all", ""), ("one-at-a-time", "")]).apply(Apply::Reload).web(WebEdit::Edit),
    d("steeringMode", G, INPUT, "Queued steering", "Send queued steering messages all at once or one at a time.")
        .choice(&[("all", ""), ("one-at-a-time", "")]).apply(Apply::Reload).web(WebEdit::Edit),
    // Class C on the web: the program named here runs on Ctrl+G, so the review step applies.
    d("externalEditor", G, INPUT, "External editor", "Editor for Ctrl+G; wins over VISUAL and EDITOR.")
        .text().widens("Ctrl+G runs this program to edit the prompt.")
        .apply(Apply::Now).web(WebEdit::Confirm),
];

#[cfg(test)]
mod tests {
    use super::super::in_group;
    use super::*;
    use crate::{schema, Kind};

    fn kind_of(key: &str) -> Kind {
        schema()
            .iter()
            .find(|k| k.key == key)
            .unwrap_or_else(|| panic!("{key} is not a schema key"))
            .kind
    }

    #[test]
    fn display_keys_exist_in_schema() {
        let rows: Vec<&Descriptor> = in_group(Group::Display).collect();
        for row in &rows {
            if let Kind::Enum(options) = kind_of(row.key) {
                for (opt, _) in row.choices {
                    assert!(options.contains(opt), "{}: {opt} is not a schema option", row.key);
                }
            }
            assert!(row.label.chars().count() <= 32, "label too long: {}", row.label);
            assert!(row.help.chars().count() <= 100, "help too long: {}", row.key);
        }
    }

    #[test]
    fn display_controls_match_kinds() {
        for row in in_group(Group::Display) {
            let kind = kind_of(row.key);
            let fits = match (row.control, kind) {
                (Control::Toggle, Kind::Bool) => true,
                (Control::Choice, Kind::Enum(_)) => true,
                // quietStartup is a bool or "header" in the schema (Kind::Any), shown as a choice.
                (Control::Choice, Kind::Any) => row.key == "quietStartup",
                // fullscreenWheelScrollLines is a number or "auto" (Kind::Any).
                (Control::NumberOr(_), Kind::Any) => row.key == "fullscreenWheelScrollLines",
                (Control::Number | Control::NumberOr(_), Kind::Number) => true,
                (Control::Text | Control::Theme, Kind::String) => true,
                (Control::List(_), Kind::StringList) => true,
                _ => false,
            };
            assert!(
                fits,
                "{}: control {:?} does not fit kind {:?}",
                row.key, row.control, kind
            );
        }
    }
}
