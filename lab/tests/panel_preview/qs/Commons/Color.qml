pragma Singleton
import QtQuick
import Quickshell
// Preview stand-in for the theme's palette. A scenario's "theme" object may replace any of the
// colours (and the corner radius, in Style), so the same panel can be drawn in another theme.
QtObject {
  readonly property var theme: Feed.scenario && Feed.scenario.theme ? Feed.scenario.theme : ({})
  function pick(key, fallback) { return typeof theme[key] === "string" ? theme[key] : fallback }
  property color foreground: pick("foreground", "#a9b1d6")
  property color background: pick("background", "#1a1b26")
  property color accent: pick("accent", "#7aa2f7")
  property color urgent: pick("urgent", "#f7768e")
  property color muted: pick("muted", "#414868")
}
