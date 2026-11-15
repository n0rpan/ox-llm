import QtQuick
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui
import "Look.js" as Look

// A small label on a tinted ground: a tag ("default"), or one fact of the status line
// ("Context · Fast"). With `clickable` it is a shortcut: it lights up under the pointer and
// emits clicked(). `tone` is the theme colour it is tinted with; `marked` adds a state dot.
Rectangle {
    id: root

    property string text: ""
    property string tip: ""
    property color foreground: Commons.Color.foreground
    property color tone: root.foreground
    property string face: Style.font.family
    property bool clickable: false
    property bool marked: false
    property bool quiet: false

    signal clicked()

    readonly property bool hot: root.clickable && hover.containsMouse
    readonly property bool toned: root.tone !== root.foreground

    implicitWidth: row.implicitWidth + Style.space(8) * 2
    implicitHeight: label.implicitHeight + Style.space(3) * 2
    width: implicitWidth
    height: implicitHeight
    radius: Style.cornerRadius > 0 ? Math.min(height / 2, Style.cornerRadius) : 0
    color: Look.tint(root.tone, root.hot ? 0.20 : root.toned ? 0.14 : 0.08)

    Behavior on color { ColorAnimation { duration: Style.duration(120) } }

    Row {
        id: row
        anchors.centerIn: parent
        spacing: Style.space(6)

        StateDot {
            visible: root.marked
            anchors.verticalCenter: parent.verticalCenter
            width: Style.space(6)
            tint: root.tone
        }
        Label {
            id: label
            text: root.text
            color: root.toned ? root.tone : root.foreground
            dim: root.quiet && !root.hot
            font.family: root.face
            font.pixelSize: Style.font.caption
        }
    }

    MouseArea {
        id: hover
        anchors.fill: parent
        hoverEnabled: true
        enabled: root.clickable || root.tip !== ""
        cursorShape: root.clickable ? Qt.PointingHandCursor : Qt.ArrowCursor
        onClicked: if (root.clickable) root.clicked()
    }
    Ui.PanelToolTip {
        visible: hover.enabled && hover.containsMouse && root.tip !== ""
        text: root.tip
        width: Math.min(implicitWidth, Style.space(340))
        fontFamily: root.face
    }
}
