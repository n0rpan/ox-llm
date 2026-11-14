import QtQuick
import qs.Commons
import qs.Commons as Commons
import "Look.js" as Look

// A thin one-value bar: how much of something is used. Rounded on rounded themes.
Rectangle {
    id: root

    property real fraction: 0
    property color foreground: Commons.Color.foreground
    property color tone: Commons.Color.accent

    width: parent ? parent.width : implicitWidth
    implicitWidth: Style.space(200)
    implicitHeight: Style.space(4)
    height: implicitHeight
    radius: Style.cornerRadius > 0 ? height / 2 : 0
    color: Look.tint(root.foreground, Look.track)

    Rectangle {
        readonly property real share: Math.max(0, Math.min(1, root.fraction))
        width: share > 0 ? Math.max(Style.space(2), Math.round(parent.width * share)) : 0
        height: parent.height
        radius: parent.radius
        color: root.tone
        Behavior on width { NumberAnimation { duration: Style.duration(180); easing.type: Easing.OutCubic } }
        Behavior on color { ColorAnimation { duration: Style.duration(120) } }
    }
}
