import QtQuick
import qs.Commons
import qs.Commons as Commons
import "Look.js" as Look

// One stacked bar: the loaded model, other apps, then free RAM. While the selected model
// is not loaded, a ghost segment shows the room it would take (needGib > 0), tinted
// urgent when it does not fit. A legend under the bar names each part that is drawn.
Item {
    id: root

    property real totalGib: -1
    property real modelGib: -1
    property real otherGib: -1
    property real needGib: -1
    property bool needFits: true
    property color foreground: Commons.Color.foreground
    property color accent: Commons.Color.accent
    property color urgent: Commons.Color.urgent
    property string face: Style.font.family
    // The catalog for the legend; without it the bar is drawn alone.
    property var strings: null
    // Short name of the model the ghost segment stands for.
    property string needName: ""

    readonly property bool known: totalGib > 0

    function share(gib) {
        return root.totalGib > 0 && gib > 0 ? Math.min(1, gib / root.totalGib) : 0
    }
    function tint(c, a) { return Qt.rgba(c.r, c.g, c.b, a) }

    readonly property real modelShare: share(modelGib)
    readonly property real otherShare: Math.min(1 - modelShare, share(otherGib))
    readonly property real needShare: Math.min(1 - modelShare - otherShare, share(needGib))
    readonly property real gap: Style.space(2)

    width: parent ? parent.width : implicitWidth
    implicitWidth: Style.space(200)
    readonly property bool hasLegend: root.strings !== null
    readonly property var legend: {
        var out = []
        if (!root.hasLegend) return out
        if (root.modelShare > 0) out.push({ label: root.strings.t("ram.model"), color: root.accent, hollow: false })
        if (root.otherShare > 0) out.push({ label: root.strings.t("ram.other"), color: Look.tint(root.foreground, Look.quiet), hollow: false })
        if (root.needGib > 0 && root.needName !== "")
            out.push({ label: root.strings.t(root.needFits ? "ram.need" : "ram.needNoFit").arg(root.needName),
                       color: root.needFits ? Look.tint(root.accent, 0.35) : Look.tint(root.urgent, 0.55), hollow: false })
        out.push({ label: root.strings.t("ram.free"), color: Look.tint(root.foreground, 0.3), hollow: true })
        return out
    }

    implicitHeight: known ? track.height + (hasLegend ? legendRow.height + Style.space(6) : 0) : 0
    height: implicitHeight
    visible: known

    Flow {
        id: legendRow
        visible: root.hasLegend
        y: track.height + Style.space(6)
        width: parent.width
        spacing: Style.space(12)

        Repeater {
            model: root.legend
            delegate: Row {
                required property var modelData
                spacing: Style.space(5)

                Rectangle {
                    anchors.verticalCenter: parent.verticalCenter
                    width: Style.space(8)
                    height: Style.space(4)
                    radius: Style.cornerRadius > 0 ? height / 2 : 0
                    color: modelData.hollow ? "transparent" : modelData.color
                    border.width: modelData.hollow ? 1 : 0
                    border.color: modelData.color
                }
                Label {
                    text: modelData.label
                    color: root.foreground
                    dim: true
                    font.family: root.face
                    font.pixelSize: Style.font.caption
                }
            }
        }
    }

    Rectangle {
        id: track
        width: parent.width
        height: Style.space(6)
        radius: Style.cornerRadius > 0 ? height / 2 : 0
        color: root.tint(root.foreground, 0.10)
        clip: true

        Rectangle {
            id: modelSegment
            height: track.height
            width: Math.round(root.modelShare * track.width)
            color: root.accent
            Behavior on width { NumberAnimation { duration: Style.duration(180); easing.type: Easing.OutCubic } }
        }

        Rectangle {
            id: otherSegment
            x: modelSegment.width > 0 ? modelSegment.width + root.gap : 0
            height: track.height
            width: Math.round(root.otherShare * track.width)
            color: Look.tint(root.foreground, Look.quiet)
            Behavior on width { NumberAnimation { duration: Style.duration(180); easing.type: Easing.OutCubic } }
        }

        Rectangle {
            visible: root.needGib > 0
            x: otherSegment.x + otherSegment.width + (otherSegment.width > 0 ? root.gap : 0)
            height: track.height
            width: Math.max(0, Math.min(track.width - x, Math.round(root.share(root.needGib) * track.width)))
            color: root.needFits ? root.tint(root.accent, 0.35) : root.tint(root.urgent, 0.55)
            Behavior on width { NumberAnimation { duration: Style.duration(180); easing.type: Easing.OutCubic } }
        }
    }
}
