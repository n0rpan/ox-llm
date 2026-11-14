import QtQuick
import qs.Commons
import qs.Commons as Commons
import "Look.js" as Look

// A quiet surface that groups what belongs together: a faint fill and a hairline outline in the
// theme's own colour. `tone` tints it (accent for what is live, urgent for a warning); `strong`
// is the chosen or highlighted look.
Rectangle {
    id: root

    default property alias content: body.data
    property color foreground: Commons.Color.foreground
    property color tone: root.foreground
    property bool strong: false
    property real pad: Style.space(12)
    property real gap: Style.space(10)

    color: Look.tint(root.tone, root.strong ? Look.fillStrong : Look.fill)
    border.width: Math.max(1, Style.space(1))
    border.color: Look.tint(root.tone, root.strong ? Look.lineStrong : Look.line)
    radius: Style.cornerRadius
    implicitHeight: body.implicitHeight + root.pad * 2
    height: implicitHeight

    Behavior on border.color { ColorAnimation { duration: Style.duration(120) } }

    Column {
        id: body
        x: root.pad
        y: root.pad
        width: root.width - root.pad * 2
        spacing: root.gap
    }
}
