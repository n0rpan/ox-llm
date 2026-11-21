//! The `Interface` rows of the settings page, in page order (issue #2042, R3): the docs view of
//! the descriptor table, rendered into `oxa/docs/pages/settings.md` by
//! `tools/ci/settings_table.py`. Edit a row here and rerun the tool.

use super::{row, PageRow};

/// The section heading shown on the page.
pub(super) const TITLE: &str = "Interface";

#[rustfmt::skip]
pub(super) const ROWS: &[PageRow] = &[
    row("$schema", "string", "`null`", "editor schema hint; ignored"),
    row("theme", "string", "`\"system\"`", "`system`, `dark`, `light` or a name from a theme file; also `oxa` (OXA's own dark palette) and `oxa-omarchy` (follows the current Omarchy desktop theme, read-only; `system` when there is none). With the `oxa` skin the default is `oxa-omarchy`"),
    row("ui.animation", "`arcade` / `minimal` / `off`", "`arcade`", "what moves on the activity line of the `oxa` skin while a run is in progress: `arcade` is a one-row animated scene with a short `CLEAR` when the run is done; `minimal` is the small six-cell sprite; `off` is the static text badge. Never any sound. `ui.reducedMotion`, `NO_COLOR` and `TERM=dumb` give `off`. `--animation <mode>` overrides this key for one run"),
    row("ui.animationGraphics", "`auto` / `off`", "`auto`", "`auto` draws the scene as pixel art when the terminal answers the kitty graphics query at start and is kitty or Ghostty (every frame is sent once and deleted again on exit); `off` keeps the scene in text everywhere"),
    row("ui.animationScene", "`graphics` / `stack` / `stars` / `charge`", "`null`", "the scene of `ui.animation = \"arcade\"`. Unset: the pixel-art scene where the terminal can draw it, else `stack` (stacking columns). `stack`, `stars` (a ship in a starfield) and `charge` (the ox, in block characters) are the text scenes; `graphics` asks for the pixel-art scene on any terminal that answers the graphics query"),
    row("ui.notify", "`off` / `unfocused` / `always`", "`unfocused`", "terminal notification when a run settles (including errors/aborts) or a permission dialog opens. `unfocused` requires a terminal focus-out report; terminals without focus reporting stay quiet. Uses OSC 9 and a BEL fallback from oxa itself, with generic text (no prompt or file details); any sound is controlled by the terminal. `off` disables both"),
    row("ui.reducedMotion", "bool", "`false`", "no animation in the `oxa` skin: the working sprite and the YOLO flames are replaced by the static text badge. The environment variable `OXA_REDUCED_MOTION=1` (also `true`, `yes` or `on`) does the same for one run, and `OXA_REDUCED_MOTION=0` (also `false`, `no` or `off`) turns it off for one run; `NO_COLOR` and `TERM=dumb` imply it"),
    row("ui.skin", "`auto` / `pi` / `oxa` / `ascii`", "`auto`", "the look of the interactive UI: `oxa` is OXA's own (frames, a status line with meters, a key hint bar), `pi` is the plain look, `ascii` is `oxa` with ASCII glyphs only, and `auto` is `oxa` (ASCII when the locale is not UTF-8 or `TERM` is `linux`/`dumb`). With the `oxa` skin, `NO_COLOR` turns all colour off"),
    row("themes", "list", "`[]`", "extra theme files or directories"),
    row("tuiMode", "`fullscreen` / `regular`", "`fullscreen`", "start-up layout"),
    row("quietStartup", "`bool` or `\"header\"`", "`false`", "shorter start-up listing; `\"header\"` shows only the header"),
    row("hideThinkingBlock", "bool", "`false`", "hide thinking blocks in the transcript"),
    row("outputPad", "number", "`1`", "padding of output blocks, `0` or `1`"),
    row("editorPaddingX", "number", "`0`", "horizontal editor padding, 0 to 3"),
    row("autocompleteMaxVisible", "number", "`5`", "rows of the autocomplete list"),
    row("markdown.codeBlockIndent", "string", "`\"  \"`", "prefix of each line of a code block"),
    row("markdown.mermaid", "`off` / `final` / `streaming`", "`streaming`", "accepted; diagrams are shown as code, so it has no effect"),
    row("fullscreenScrollbar", "`auto` / `always` / `hidden`", "`auto`", "scrollbar in full-screen mode; `auto` shows it while scrolling"),
    row("fullscreenWheelScrollLines", "number or `\"auto\"`", "`\"auto\"`", "lines per wheel step; `auto` speeds up fast spins"),
    row("fullscreenCopyOnSelect", "bool", "`true`", "copy a mouse selection on release"),
    row("fullscreenExitOutput", "`transcript` / `resume-hint`", "`transcript`", "what stays on screen after leaving full screen"),
    row("terminal.showImages", "bool", "`true`", "draw images in the terminal where the terminal supports it"),
    row("terminal.imageWidthCells", "number", "`60`", "width of drawn images, in cells"),
    row("terminal.clearOnShrink", "bool", "`false`", "clear empty rows when the output shrinks"),
    row("images.blockImages", "bool", "`false`", "do not send images to the model"),
    row("treeFilterMode", "`default` / `no-tools` / `user-only` / `labeled-only` / `all`", "`default`", "filter the tree opens with"),
    row("doubleEscapeAction", "`tree` / `fork` / `none`", "`tree`", "action of a double Escape on an empty editor"),
    row("showCacheMissNotices", "bool", "`false`", "show a notice for each cache miss ([compaction.md](compaction.md#cache-misses))"),
    row("enableSkillCommands", "bool", "`true`", "list skills in the slash-command autocomplete"),
    row("followUpMode", "`all` / `one-at-a-time`", "`one-at-a-time`", "how queued follow-up messages are sent"),
    row("steeringMode", "`all` / `one-at-a-time`", "`one-at-a-time`", "how queued steering messages are sent"),
    row("oxa.desk.font", "string", "`null`", "desktop app (`oxa-desk`) only, user settings only: the font family of the window. Unset uses the system font. The terminal UI does not read the `oxa.desk.*` keys"),
    row("oxa.desk.fontSize", "number", "`14`", "desktop app only, user settings only: text size in pixels. A value outside 8 to 40 is saved with a note, not refused"),
    row("oxa.desk.blinkCursor", "bool", "`false`", "desktop app only, user settings only: blink the text cursor. Off by default, so an idle window does not wake up to redraw"),
    row("oxa.desk.keepTranscripts", "number", "`30`", "desktop app only, user settings only: how many threads the desktop app keeps loaded in memory. Beyond that the one used longest ago is dropped and reloads when opened; the open thread and a thread with a pending question always stay loaded"),
];
