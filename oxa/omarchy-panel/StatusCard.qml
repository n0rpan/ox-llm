import QtQuick
import qs.Commons
import qs.Commons as Commons
import "Look.js" as Look

// The three state chips under the hero: Local, Cloud, Lead, always in that order (design 1.1).
// Each is a mark plus words, never colour alone. A click opens the tab that explains the chip.
// `chips` is the list Model.chips returns: { id, tab, state, raw, model, count, fix, url }.
Item {
    id: root

    property var chips: []
    property color foreground: Commons.Color.foreground
    property color accent: Commons.Color.accent
    property color urgent: Commons.Color.urgent
    property string face: Style.font.family

    signal tabRequested(string tab)

    readonly property real gap: Style.space(6)

    implicitHeight: row.implicitHeight
    height: implicitHeight

    // The dot's shape and tint for a state: filled = active or ready, hollow = present but not in
    // use, hollow urgent = needs attention, filled urgent = failed (the StateDot rule of section 3).
    function markKind(state) {
        if (state === "error" || state === "stalled") return "filled"
        if (state === "ready" || state === "running" || state === "idle" || state === "pursuing") return "filled"
        return "hollow"
    }

    function markTint(state) {
        if (state === "error" || state === "stalled" || state === "no_credentials" || state === "awaiting_owner")
            return root.urgent
        if (state === "ready" || state === "running" || state === "idle" || state === "pursuing" || state === "remote")
            return root.accent
        return root.foreground
    }

    Row {
        id: row
        width: root.width
        spacing: root.gap

        Repeater {
            model: root.chips

            delegate: Rectangle {
                id: chip

                required property var modelData
                readonly property string chipState: modelData.state
                readonly property color tint: root.markTint(chip.chipState)

                width: (root.width - root.gap * 2) / 3
                height: chipRow.implicitHeight + Style.space(8) * 2
                radius: Style.cornerRadius
                color: Look.tint(chip.tint, chipArea.containsMouse ? Look.fillStrong : Look.fill)
                border.width: Math.max(1, Style.space(1))
                border.color: Look.tint(chip.tint, Look.line)

                Row {
                    id: chipRow
                    x: Style.space(8)
                    y: Style.space(8)
                    width: chip.width - Style.space(16)
                    spacing: Style.space(6)

                    StateDot {
                        id: dot
                        kind: root.markKind(chip.chipState)
                        tint: chip.tint
                    }
                    Label {
                        width: chipRow.width - dot.width - chipRow.spacing
                        text: strings.t("chip." + chip.modelData.id) + " · "
                            + strings.t("state." + chip.chipState).arg(chip.modelData.model)
                        color: root.foreground
                        font.family: root.face
                        font.pixelSize: Style.font.caption
                    }
                }

                MouseArea {
                    id: chipArea
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.tabRequested(chip.modelData.tab)
                }
            }
        }
    }

    Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }
}
