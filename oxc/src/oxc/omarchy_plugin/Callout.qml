import QtQuick
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui
import "Look.js" as Look

// A message that must be read: a tinted block with a rule in the same colour on its leading
// edge. Urgent by default (a failure, a warning). An optional button sits at its end.
Rectangle {
    id: root

    property string text: ""
    property string actionText: ""
    property color foreground: Commons.Color.foreground
    property color accent: Commons.Color.accent
    property color tone: Commons.Color.urgent
    property string face: Style.font.family
    property int maximumLineCount: 4

    signal actionRequested()

    readonly property real pad: Style.space(10)

    implicitHeight: Math.max(message.implicitHeight, action.visible ? action.implicitHeight : 0) + root.pad * 2
    height: implicitHeight
    radius: Style.cornerRadius
    color: Look.tint(root.tone, 0.10)

    Rectangle {
        anchors.left: parent.left
        y: root.radius
        width: Style.space(2)
        height: parent.height - root.radius * 2
        color: root.tone
    }

    Label {
        id: message
        anchors.left: parent.left
        anchors.leftMargin: root.pad + Style.space(2)
        anchors.right: action.visible ? action.left : parent.right
        anchors.rightMargin: action.visible ? Style.space(8) : root.pad
        anchors.verticalCenter: parent.verticalCenter
        text: root.text
        color: root.foreground
        wrapMode: Text.WordWrap
        maximumLineCount: root.maximumLineCount
        font.family: root.face
        font.pixelSize: Style.font.bodySmall
    }
    Ui.Button {
        id: action
        visible: root.actionText !== ""
        anchors.right: parent.right
        anchors.rightMargin: root.pad
        anchors.verticalCenter: parent.verticalCenter
        text: root.actionText
        bordered: true
        foreground: root.foreground
        accent: root.accent
        fontFamily: root.face
        fontSize: Style.font.bodySmall
        verticalPadding: Style.spacing.xs
        onClicked: root.actionRequested()
    }
}
