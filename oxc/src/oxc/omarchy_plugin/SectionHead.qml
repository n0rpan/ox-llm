import QtQuick
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui

// A section title in the kit's own header style, with an optional quiet figure at its end.
Item {
    id: root

    property string text: ""
    property string trailing: ""
    property color foreground: Commons.Color.foreground
    property string face: Style.font.family

    implicitHeight: Math.max(head.implicitHeight, tail.visible ? tail.implicitHeight : 0)
    height: implicitHeight

    Ui.PanelSectionHeader {
        id: head
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        foreground: root.foreground
        fontFamily: root.face
        text: root.text
    }
    Label {
        id: tail
        visible: root.trailing !== ""
        anchors.right: parent.right
        anchors.baseline: head.baseline
        width: Math.min(implicitWidth, root.width - head.width - Style.space(12))
        horizontalAlignment: Text.AlignRight
        text: root.trailing
        color: root.foreground
        dim: true
        font.family: root.face
        font.pixelSize: Style.font.bodySmall
    }
}
