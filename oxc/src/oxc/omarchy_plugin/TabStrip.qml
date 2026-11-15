import QtQuick
import qs.Commons
import qs.Commons as Commons
import "Look.js" as Look

// The panel's tabs: equal cells over a hairline, the current one marked by an accent rule that
// slides to it. Tabs look unlike the choice chips on purpose: a tab changes what is shown, a
// chip changes a setting. An option is { value, label, mark } where mark ("" / "accent" /
// "urgent") puts a small dot beside the label for a tab that has something to say.
Item {
    id: root

    property var options: []
    property string value: ""
    property color foreground: Commons.Color.foreground
    property color accent: Commons.Color.accent
    property color urgent: Commons.Color.urgent
    property string face: Style.font.family

    signal changed(string value)

    readonly property int count: root.options.length
    readonly property real cell: root.count > 0 ? root.width / root.count : 0
    readonly property int currentIndex: {
        for (var i = 0; i < root.options.length; i++)
            if (root.options[i].value === root.value) return i
        return -1
    }
    // Moves one tab along; the ends do not wrap.
    function step(delta) {
        var next = Math.max(0, Math.min(root.count - 1, root.currentIndex + delta))
        if (next !== root.currentIndex && next >= 0) root.changed(root.options[next].value)
    }

    implicitHeight: Style.space(30)
    height: implicitHeight

    Rectangle {
        anchors.bottom: parent.bottom
        width: parent.width
        height: Math.max(1, Style.space(1))
        color: Look.tint(root.foreground, Look.line)
    }

    Row {
        anchors.fill: parent

        Repeater {
            model: root.options
            delegate: Item {
                id: tab
                required property var modelData
                readonly property bool current: modelData.value === root.value
                readonly property string mark: modelData.mark ? String(modelData.mark) : ""

                width: root.cell
                height: root.height

                Rectangle {
                    anchors.fill: parent
                    anchors.bottomMargin: Style.space(3)
                    radius: Style.cornerRadius
                    color: hover.containsMouse && !tab.current ? Look.tint(root.foreground, Look.fillHover) : "transparent"
                    Behavior on color { ColorAnimation { duration: Style.duration(120) } }
                }
                Row {
                    anchors.centerIn: parent
                    anchors.verticalCenterOffset: -Style.space(1)
                    spacing: Style.space(6)

                    Label {
                        text: tab.modelData.label
                        color: root.foreground
                        dim: !tab.current && !hover.containsMouse
                        font.family: root.face
                        font.bold: tab.current
                    }
                    StateDot {
                        visible: tab.mark !== ""
                        anchors.verticalCenter: parent.verticalCenter
                        width: Style.space(6)
                        tint: tab.mark === "urgent" ? root.urgent : root.accent
                    }
                }
                MouseArea {
                    id: hover
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: tab.current ? Qt.ArrowCursor : Qt.PointingHandCursor
                    onClicked: if (!tab.current) root.changed(tab.modelData.value)
                }
            }
        }
    }

    Rectangle {
        visible: root.currentIndex >= 0
        x: Commons.I18n.rightToLeft ? root.width - (root.currentIndex + 1) * root.cell : root.currentIndex * root.cell
        anchors.bottom: parent.bottom
        width: root.cell
        height: Style.space(2)
        color: root.accent
        Behavior on x { NumberAnimation { duration: Style.duration(160); easing.type: Easing.OutCubic } }
    }
}
