import QtQuick
import qs.Commons
import qs.Commons as Commons

// One figure with its caption under it. `bad` paints the figure in the theme's urgent colour.
Column {
    id: root

    property string value: ""
    property string label: ""
    property bool bad: false
    property color foreground: Commons.Color.foreground
    property color urgent: Commons.Color.urgent
    property string face: Style.font.family

    spacing: Style.space(2)

    Label {
        width: root.width
        text: root.value
        color: root.bad ? root.urgent : root.foreground
        font.family: root.face
        font.pixelSize: Style.font.title
        font.bold: true
    }
    Label {
        width: root.width
        text: root.label
        color: root.foreground
        dim: true
        font.family: root.face
        font.pixelSize: Style.font.caption
    }
}
