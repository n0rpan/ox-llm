import QtQuick
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui
import "Model.js" as Model

// The ox AI panel shell (design section 1): bar button, header, hero, state chips, tabs, the
// tab body and two action rows. Data comes only from Feeds.qml (oxa JSON); this file holds no
// process, file or network access. The tab bodies are Loaders that later slices fill in.
Ui.Panel {
    id: root

    implicitWidth: button.implicitWidth
    implicitHeight: button.implicitHeight

    moduleName: "snapdragon.local-ai"
    ipcTarget: "snapdragon.local-ai"

    // The last readable status (Model.parseStatus); before the first answer it is a fault.
    property var oxaStatus: Model.parseStatus("", null, false)
    // "models", "usage", "lead" or "storage".
    property string tab: "models"
    property bool pollingPaused: false
    readonly property var head: Model.headline(root.oxaStatus)
    readonly property var chipList: Model.chips(root.oxaStatus, { leadUrl: root.setting("leadUrl", "") })
    readonly property string binaryShown: root.setting("oxaPath", "") !== ""
        ? root.setting("oxaPath", "") : (Quickshell.env("HOME") || "") + "/.local/bin/oxa"

    readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
    readonly property color accent: Commons.Color.accent
    readonly property color urgent: Commons.Color.urgent
    readonly property string face: bar ? bar.fontFamily : Style.font.family

    Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }

    // Feeds.qml runs the commands (status every 5 s; the tab's own command while it is shown).
    Feeds {
        id: feeds
        oxaPath: root.setting("oxaPath", "")
        pollEnabled: root.setting("pollEnabled", true)
        pollingPaused: root.pollingPaused
        opened: root.opened
        tab: root.tab
        onAnswered: function (kind, text, code, timedOut) {
            if (kind === "status") root.oxaStatus = Model.parseStatus(text, code, timedOut)
        }
    }

    // Words for the hero and the bar label, keyed by Model.headline ids.
    function headlineText(h) {
        if (h.id === "usage") return strings.t("hero.usage").arg(strings.t("state." + h.state))
        if (h.id === "stalled") return strings.t("hero.stalled").arg(h.count)
        if (h.id === "asks") return strings.t("hero.asks").arg(h.count)
        if (h.id === "working") return strings.t("hero.working").arg(h.count)
        return strings.t("hero." + h.id)
    }

    function detailText(h) {
        if (root.oxaStatus.fault !== null) return strings.t("fault.fix." + root.oxaStatus.fault).arg(root.binaryShown)
        if (h.id === "asks") return strings.t("hero.detail.asks")
        if (h.id === "stalled") return h.name !== "" ? strings.t("hero.detail.stalledNamed").arg(h.name) : ""
        if (h.id === "usage") return strings.t("hero.detail.usage")
        if (h.id === "no_model") return strings.t("hero.detail.noModel")
        return root.oxaStatus.defaultModel !== "" ? strings.t("hero.detail.model").arg(root.oxaStatus.defaultModel) : ""
    }

    // Which look the hero's dot and tone take: faults and attention in urgent, live work in accent.
    readonly property bool heroNeedsAttention: root.oxaStatus.fault !== null || root.head.id === "stalled"
        || root.head.id === "asks" || root.head.id === "usage"
    readonly property color heroTone: root.heroNeedsAttention ? root.urgent : root.accent
    readonly property string heroKind: root.oxaStatus.fault !== null || root.head.id === "stalled" ? "filled"
        : root.heroNeedsAttention ? "hollow" : root.head.id === "working" ? "pulse" : "filled"

    // The launch actions (Chat, Code, Studio, lead page, Settings) are wired by the launch slice.
    // Until then a click does nothing.
    function launch(kind) {
    }

    function openTab(name) {
        root.tab = name
    }

    // The bar label: asks, else working sessions, else nothing.
    readonly property string barLabel: root.head.id === "asks" ? String(root.head.count)
        : root.head.id === "working" ? String(root.head.count) : ""

    Ui.BarIconButton {
        id: button
        anchors.fill: parent
        bar: root.bar
        text: ""
        labelText: root.barLabel
        slotSize: Style.bar.statusSlot
        tooltipText: root.headlineText(root.head)
        onPressed: root.toggle()
    }

    Ui.KeyboardPanel {
        id: panel
        anchorItem: button
        owner: root
        bar: root.bar
        open: root.opened
        focusTarget: panel.loadedContent
        contentWidth: panel.fittedContentWidth(Style.space(420))
        contentHeight: panel.fittedContentHeight(panel.loadedContentHeight)
        content: Ui.PanelKeyCatcher {
            anchors.fill: parent
            implicitHeight: column.implicitHeight
            onCloseRequested: root.close()
            onTabRequested: function (direction) { root.switchPanel(direction) }

            Column {
                id: column
                width: parent.width
                spacing: Style.space(12)

                // Everything but the tab body; the body takes what is left and scrolls.
                readonly property real chrome: header.height + hero.height + tabs.height
                    + separator.height + actions.height + column.spacing * 6
                readonly property real bodyRoom: panel.availableCardHeight > 0
                    ? Math.max(Style.space(140), panel.availableCardHeight - panel.verticalContentInset - column.chrome)
                    : -1

                Item {
                    id: header
                    width: column.width
                    height: Math.max(titleLabel.implicitHeight, pauseButton.implicitHeight)

                    Label {
                        id: titleLabel
                        anchors.verticalCenter: parent.verticalCenter
                        width: parent.width - pauseButton.width - Style.space(16)
                        text: strings.t("title")
                        color: root.foreground
                        font.family: root.face
                        font.pixelSize: Style.font.title
                        font.bold: true
                    }
                    Ui.Button {
                        id: pauseButton
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        iconText: root.pollingPaused ? "" : ""
                        iconSize: Style.font.caption
                        opacity: root.pollingPaused || hot ? 1 : 0.5
                        tooltipText: root.pollingPaused ? strings.t("footer.resume") : strings.t("footer.pause")
                        foreground: root.foreground
                        accent: root.accent
                        fontFamily: root.face
                        onClicked: root.pollingPaused = !root.pollingPaused
                    }
                }

                // Hero: the headline and its detail, then the three state chips.
                Card {
                    id: hero
                    width: column.width
                    foreground: root.foreground
                    tone: root.heroTone
                    gap: Style.space(8)

                    Row {
                        width: hero.width - hero.pad * 2
                        spacing: Style.space(8)

                        StateDot {
                            id: heroDot
                            kind: root.heroKind
                            tint: root.heroTone
                        }
                        Label {
                            id: headline
                            width: parent.width - heroDot.width - parent.spacing
                            text: root.headlineText(root.head)
                            color: root.foreground
                            font.family: root.face
                            font.pixelSize: Style.font.body
                            font.bold: true
                        }
                    }
                    Label {
                        id: detail
                        width: hero.width - hero.pad * 2
                        visible: text !== ""
                        text: root.detailText(root.head)
                        color: root.foreground
                        dim: true
                        font.family: root.face
                        font.pixelSize: Style.font.caption
                        wrapMode: Text.WordWrap
                        maximumLineCount: 3
                    }
                    StatusCard {
                        id: chips
                        width: hero.width - hero.pad * 2
                        chips: root.chipList
                        foreground: root.foreground
                        accent: root.accent
                        urgent: root.urgent
                        face: root.face
                        onTabRequested: function (name) { root.openTab(name) }
                    }
                }

                TabStrip {
                    id: tabs
                    width: column.width
                    options: [
                        { value: "models", label: strings.t("tab.models"), mark: "" },
                        { value: "usage", label: strings.t("tab.usage"), mark: root.head.id === "usage" ? "urgent" : "" },
                        { value: "lead", label: strings.t("tab.lead"), mark: root.head.id === "asks" ? "urgent" : "" },
                        { value: "storage", label: strings.t("tab.storage"), mark: "" }
                    ]
                    value: root.tab
                    foreground: root.foreground
                    accent: root.accent
                    urgent: root.urgent
                    face: root.face
                    onChanged: function (picked) { root.openTab(picked) }
                }

                Flickable {
                    id: bodyFlick
                    width: column.width
                    height: column.bodyRoom > 0 ? Math.min(body.implicitHeight, column.bodyRoom) : body.implicitHeight
                    contentWidth: width
                    contentHeight: body.implicitHeight
                    clip: contentHeight > height
                    interactive: contentHeight > height
                    boundsBehavior: Flickable.StopAtBounds

                    Connections {
                        target: root
                        function onTabChanged() { bodyFlick.contentY = 0 }
                    }

                    Column {
                        id: body
                        width: bodyFlick.width

                        // Each tab's body is its own file; a slice that adds one only sets its source.
                        Loader {
                            width: body.width
                            active: root.tab === "models"
                            visible: active
                            source: "ModelsView.qml"
                        }
                        Loader {
                            width: body.width
                            active: root.tab === "usage"
                            visible: active
                            source: "UsageView.qml"
                        }
                        Loader {
                            width: body.width
                            active: root.tab === "lead"
                            visible: active
                            source: "LeadView.qml"
                        }
                        Loader {
                            width: body.width
                            active: root.tab === "storage"
                            visible: active
                            source: "StorageView.qml"
                        }
                    }
                }

                Ui.PanelSeparator {
                    id: separator
                    width: column.width
                    foreground: root.foreground
                }

                // Primary row: the three ways in. Second row: the lead page and settings.
                Column {
                    id: actions
                    width: column.width
                    spacing: Style.space(8)

                    Row {
                        width: parent.width
                        spacing: Style.space(8)
                        readonly property real cell: (width - spacing * 2) / 3

                        Ui.Button {
                            width: parent.cell
                            iconText: ""
                            text: strings.t("action.chat")
                            bordered: true
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            onClicked: root.launch("chat")
                        }
                        Ui.Button {
                            width: parent.cell
                            iconText: ""
                            text: strings.t("action.code")
                            bordered: true
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            onClicked: root.launch("code")
                        }
                        Ui.Button {
                            width: parent.cell
                            iconText: ""
                            text: strings.t("action.studio")
                            bordered: true
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            onClicked: root.launch("studio")
                        }
                    }

                    Row {
                        width: parent.width
                        spacing: Style.space(8)
                        readonly property real cell: (width - spacing) / 2

                        Ui.Button {
                            width: parent.cell
                            text: strings.t("action.lead")
                            bordered: true
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            onClicked: root.launch("lead")
                        }
                        Ui.Button {
                            width: parent.cell
                            text: strings.t("action.settings")
                            bordered: true
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            onClicked: root.launch("settings")
                        }
                    }
                }
            }
        }
    }
}
