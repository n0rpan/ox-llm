import QtQuick
import qs.Commons
import qs.Commons as Commons

// What a tab shows when it has nothing to list: a quiet glyph, one line, and a hint under it.
Column {
    id: root

    property string glyph: ""
    property string title: ""
    property string hint: ""
    property color foreground: Commons.Color.foreground
    property string face: Style.font.family

    topPadding: Style.space(18)
    bottomPadding: Style.space(18)
    spacing: Style.space(6)

    Label {
        visible: root.glyph !== ""
        width: root.width
        horizontalAlignment: Text.AlignHCenter
        text: root.glyph
        color: root.foreground
        opacity: 0.35
        font.family: root.face
        font.pixelSize: Style.font.display
    }
    Label {
        width: root.width
        horizontalAlignment: Text.AlignHCenter
        wrapMode: Text.WordWrap
        text: root.title
        color: root.foreground
        font.family: root.face
    }
    Label {
        visible: root.hint !== ""
        width: root.width
        horizontalAlignment: Text.AlignHCenter
        wrapMode: Text.WordWrap
        text: root.hint
        color: root.foreground
        dim: true
        font.family: root.face
        font.pixelSize: Style.font.bodySmall
    }
}
