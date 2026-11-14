import QtQuick
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui
import "Model.js" as Model
import "Look.js" as Look

// The model chooser, the Flash download line (only while something is happening) and the apps.
Column {
    id: root

    property var vm: Model.parse("", 0)
    property var strings: null
    property color foreground: Commons.Color.foreground
    property color accent: Commons.Color.accent
    property color urgent: Commons.Color.urgent
    property string face: Style.font.family
    property bool busy: false
    property var preparing: []

    signal selectRequested(string key)
    signal prepareRequested(string name)
    signal prepareAllRequested()
    signal forgetRequested(string name)
    signal flashCacheRequested(string preference)
    signal modeRequested(string mode)

    component T: Label { color: root.foreground; font.family: root.face }

    function fmt(value, decimals) { return Commons.I18n.formatNumber(value, decimals) }
    // 48000 -> "48K"; small numbers stay as they are.
    function tokens(n) { return n >= 1000 ? root.strings.t("unit.k").arg(root.fmt(Math.round(n / 1000), 0)) : root.fmt(n, 0) }

    // Context modes (Fast / Deep / Max) of the 35B MoE model in play; empty when the controller
    // does not report them.
    readonly property var context: root.vm.context || ({ mode: "", modes: [], canChange: false })
    // The mode command sets the selected model's mode, so it is offered only on that model.
    readonly property bool modeEnabled: root.context.canChange === true && !root.busy
        && root.vm.contextKey !== "" && root.vm.contextKey === root.vm.selected
    // The controller's short reasons for a mode it does not offer, in plain words.
    readonly property var modeReasons: ({
        "needs RSS proof": "rss", "RSS proof is for another engine": "stale", "RSS above 27 GiB": "memory",
        "Ornith only": "ornith", "Rust engine only": "engine"
    })
    // What a mode costs, in plain words; for one that is not offered, why.
    function modeTip(mode) {
        var lines = [root.strings.t("mode." + mode.id + ".cost")]
        if (mode.compactAt > 0) lines.push(root.strings.t("mode.keep").arg(root.tokens(mode.compactAt)))
        if (mode.cap > 0) lines.push(root.strings.t("mode.window").arg(root.tokens(mode.cap)))
        if (mode.available !== true)
            lines.push(root.strings.t("mode.unavailable").arg(
                mode.reason === "" ? root.strings.t("mode.reason.none")
                : root.modeReasons[mode.reason] ? root.strings.t("mode.reason." + root.modeReasons[mode.reason])
                : mode.reason))
        return lines.join("\n")
    }

    // One chooser card. Clicking it asks to select the model; the parent decides nothing. The
    // mark on its leading edge says which model is chosen (it loads on the next request); the
    // word at its end says what is in RAM now. Its own settings sit inside the card.
    component ModelRow: Item {
        id: entry
        property var info: ({})
        readonly property string key: entry.info.key || ""
        readonly property bool selected: entry.info.selected === true
        readonly property bool available: entry.info.available === true
        readonly property bool clickable: entry.available && !entry.selected && !root.busy && root.canSelect
        readonly property real workingSet: entry.info.workingSetGib || -1
        readonly property real shownGib: (entry.info.needGib || -1) >= 0 ? entry.info.needGib : entry.workingSet
        readonly property bool noFit: entry.available && entry.info.fits === "no" && (entry.info.needGib || -1) >= 0
        readonly property string phase: entry.info.state || "cold"
        readonly property var cache: entry.info.cache || null
        readonly property bool showCache: entry.key === "flash" && entry.cache !== null && entry.cache.control === true
            && (entry.selected || entry.phase !== "cold")
        readonly property bool showMode: entry.key !== "" && entry.key === root.vm.contextKey && root.context.modes.length > 0
        readonly property string tagText: entry.info.isDefault === true ? root.strings.t("model.badge.default")
            : entry.info.group === "heavy" ? root.strings.t("model.badge.heavy") : ""
        // The word at the end of line two; empty for an idle model.
        readonly property string stateText: !entry.available ? root.strings.t("model.unavailable")
            : entry.phase !== "cold" ? root.strings.t("model.state." + entry.phase) : ""
        // Why it cannot load now, on a line of its own.
        readonly property string noFitText: !entry.noFit || entry.phase !== "cold" ? ""
            : root.vm.ram.availableGib >= 0
                ? root.strings.t("model.nofitLine").arg(root.fmt(entry.info.needGib, 1)).arg(root.fmt(root.vm.ram.availableGib, 1))
                : root.strings.t("model.nofit")
        readonly property string tip: {
            var lines = [root.strings.t("model." + entry.key + (entry.key === "flash" ? ".full" : ""))]
            if (entry.noFit)
                lines.push(root.strings.t("model.nofitDetail").arg(root.fmt(entry.info.needGib, 1)).arg(root.fmt(root.vm.ram.availableGib, 1)))
            if (entry.workingSet > 0) lines.push(root.strings.t("model.workingSet").arg(root.fmt(entry.workingSet, 1)))
            if ((entry.info.batteryStart || 0) > 0) lines.push(root.strings.t("model.battery").arg(root.fmt(entry.info.batteryStart)))
            if (entry.available && !entry.selected) lines.push(root.strings.t("model.selectHint"))
            return lines.join("\n")
        }
        readonly property real padX: Style.space(12)
        readonly property real padY: Style.space(10)
        // Lines under the name start where the name starts, past the mark.
        readonly property real indent: mark.width + Style.space(10)
        readonly property real labelColumn: Math.max(Style.space(56), entry.showCache ? cacheTitle.implicitWidth : 0, entry.showMode ? modeTitle.implicitWidth : 0)

        width: root.width
        height: lines.height + entry.padY * 2

        Rectangle {
            anchors.fill: parent
            radius: Style.cornerRadius
            opacity: entry.available ? 1 : 0.6
            color: entry.selected ? Look.tint(root.accent, Look.fillStrong)
                : hover.containsMouse && entry.clickable ? Look.tint(root.foreground, Look.fillHover)
                : Look.tint(root.foreground, Look.fill)
            border.width: Math.max(1, Style.space(1))
            border.color: entry.selected ? Look.tint(root.accent, Look.lineStrong) : Look.tint(root.foreground, Look.line)
            Behavior on color { ColorAnimation { duration: Style.duration(120) } }
            Behavior on border.color { ColorAnimation { duration: Style.duration(120) } }
        }

        MouseArea {
            id: hover
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: entry.clickable ? Qt.PointingHandCursor : Qt.ArrowCursor
            onClicked: {
                if (entry.clickable) root.selectRequested(entry.key)
            }
        }

        Ui.PanelToolTip {
            visible: hover.containsMouse
            text: entry.tip
            width: Math.min(implicitWidth, Style.space(340))
            fontFamily: root.face
        }

        Column {
            id: lines
            x: entry.padX
            y: entry.padY
            width: entry.width - entry.padX * 2
            spacing: Style.space(3)

            Item {
                width: parent.width
                height: nameLabel.implicitHeight
                opacity: entry.available ? 1 : 0.5

                // The choice mark: a ring, filled when this is the chosen model.
                Rectangle {
                    id: mark
                    anchors.verticalCenter: parent.verticalCenter
                    width: Style.space(12)
                    height: width
                    radius: Style.cornerRadius > 0 ? width / 2 : 0
                    color: "transparent"
                    border.width: Math.max(1, Style.space(1))
                    border.color: entry.selected ? root.accent : Look.tint(root.foreground, 0.45)

                    Rectangle {
                        visible: entry.selected
                        anchors.centerIn: parent
                        width: Style.space(6)
                        height: width
                        radius: Style.cornerRadius > 0 ? width / 2 : 0
                        color: root.accent
                    }
                }
                T {
                    id: nameLabel
                    x: entry.indent
                    width: Math.min(implicitWidth, parent.width - x - (tagPill.visible ? tagPill.width + Style.space(8) : 0)
                        - needLabel.width - Style.space(10))
                    text: root.strings.t("model." + entry.key + ".short")
                    font.pixelSize: Style.font.subtitle
                    font.bold: true
                }
                Pill {
                    id: tagPill
                    anchors.left: nameLabel.right
                    anchors.leftMargin: Style.space(8)
                    anchors.verticalCenter: nameLabel.verticalCenter
                    visible: entry.tagText !== ""
                    text: entry.tagText
                    quiet: true
                    foreground: root.foreground
                    face: root.face
                }
                T {
                    id: needLabel
                    anchors.right: parent.right
                    anchors.baseline: nameLabel.baseline
                    visible: entry.available && entry.shownGib > 0
                    width: visible ? implicitWidth : 0
                    text: visible ? root.strings.t("unit.gib").arg(root.fmt(entry.shownGib, 1)) : ""
                    dim: !entry.noFit
                    color: entry.noFit ? root.urgent : root.foreground
                    font.pixelSize: Style.font.bodySmall
                }
            }

            Item {
                width: parent.width
                height: hintLabel.implicitHeight
                opacity: entry.available ? 1 : 0.5

                T {
                    id: hintLabel
                    x: entry.indent
                    width: parent.width - x - (stateGroup.visible ? stateGroup.width + Style.space(10) : 0)
                    text: root.strings.t("model." + entry.key + ".hint")
                    dim: true
                    font.pixelSize: Style.font.bodySmall
                }
                Row {
                    id: stateGroup
                    visible: entry.stateText !== ""
                    anchors.right: parent.right
                    anchors.verticalCenter: hintLabel.verticalCenter
                    spacing: Style.space(6)

                    StateDot {
                        visible: entry.phase !== "cold"
                        anchors.verticalCenter: parent.verticalCenter
                        kind: entry.phase === "loading" ? "pulse" : "filled"
                        tint: root.accent
                    }
                    T {
                        text: entry.stateText
                        dim: !entry.available
                        color: entry.phase !== "cold" ? root.accent : root.foreground
                        font.pixelSize: Style.font.bodySmall
                    }
                }
            }

            T {
                visible: entry.noFitText !== ""
                x: entry.indent
                width: parent.width - x
                text: entry.noFitText
                color: root.urgent
                wrapMode: Text.WordWrap
                maximumLineCount: 2
                font.pixelSize: Style.font.bodySmall
            }

            // The card's own settings, under a rule: Flash's expert cache and the context mode.
            Item {
                visible: entry.showCache || entry.showMode
                width: parent.width
                height: Style.space(9)

                Rectangle {
                    anchors.bottom: parent.bottom
                    anchors.bottomMargin: Style.space(2)
                    width: parent.width
                    height: Math.max(1, Style.space(1))
                    color: Look.tint(entry.selected ? root.accent : root.foreground, Look.line)
                }
            }

            // Flash's expert-cache preference: shown only while Flash is the model in play.
            Item {
                visible: entry.showCache
                width: parent.width
                height: Math.max(cacheTitle.implicitHeight, cacheGroup.implicitHeight)

                T {
                    id: cacheTitle
                    anchors.verticalCenter: parent.verticalCenter
                    text: root.strings.t("flash.cache.title")
                    dim: true
                    font.pixelSize: Style.font.bodySmall

                    MouseArea { id: cacheHover; anchors.fill: parent; hoverEnabled: true }
                    Ui.PanelToolTip {
                        visible: cacheHover.containsMouse
                        text: root.strings.t("flash.cache.next")
                        width: Math.min(implicitWidth, Style.space(340))
                        fontFamily: root.face
                    }
                }
                Ui.ButtonGroup {
                    id: cacheGroup
                    x: entry.labelColumn + Style.space(8)
                    anchors.verticalCenter: parent.verticalCenter
                    enabled: entry.available && entry.showCache && entry.cache.canChange === true && !root.busy
                    opacity: enabled ? 1 : 0.5
                    options: [
                        { value: "auto", label: root.strings.t("flash.cache.auto") },
                        { value: "20", label: root.fmt(20) },
                        { value: "18", label: root.fmt(18) },
                        { value: "16", label: root.fmt(16) },
                        { value: "14", label: root.fmt(14) }
                    ]
                    value: entry.showCache ? entry.cache.pref : ""
                    foreground: root.foreground
                    accent: root.accent
                    fontFamily: root.face
                    fontSize: Style.font.bodySmall
                    focusable: false
                    onChanged: function(picked) { root.flashCacheRequested(picked) }
                }
            }
            T {
                readonly property bool known: entry.showCache && (entry.cache.tierGib || -1) > 0 && (entry.cache.reason || "") !== ""
                visible: known || (entry.showCache && entry.cache.pref === "20")
                width: parent.width
                wrapMode: Text.WordWrap
                text: known
                    ? root.strings.t("flash.cache.actual").arg(root.fmt(entry.cache.tierGib))
                        .arg(root.strings.t("flash.cache.reason." + entry.cache.reason))
                    : root.strings.t("flash.cache.swap")
                dim: true
                font.pixelSize: Style.font.caption
            }

            // Context mode: how much conversation is kept before older turns are summarised.
            Item {
                visible: entry.showMode
                width: parent.width
                height: Math.max(modeTitle.implicitHeight, modeGroup.implicitHeight)

                T {
                    id: modeTitle
                    anchors.verticalCenter: parent.verticalCenter
                    text: root.strings.t("mode.title")
                    dim: true
                    font.pixelSize: Style.font.bodySmall

                    MouseArea { id: modeHover; anchors.fill: parent; hoverEnabled: true }
                    Ui.PanelToolTip {
                        visible: modeHover.containsMouse
                        text: root.strings.t(root.modeEnabled ? "mode.tip" : "mode.tipBusy")
                        width: Math.min(implicitWidth, Style.space(340))
                        fontFamily: root.face
                    }
                }
                // The kit's ButtonGroup cannot dim one option, and Max must show as not offered.
                Row {
                    id: modeGroup
                    x: entry.labelColumn + Style.space(8)
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Style.spacing.md
                    opacity: root.modeEnabled ? 1 : 0.5

                    Repeater {
                        model: entry.showMode ? root.context.modes : []
                        delegate: Ui.Button {
                            required property var modelData
                            readonly property bool offered: modelData.available === true
                            text: root.strings.t("mode." + modelData.id)
                            tooltipText: root.modeTip(modelData)
                            selected: modelData.id === root.context.mode
                            bordered: true
                            opacity: offered ? 1 : 0.45
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            fontSize: Style.font.bodySmall
                            onClicked: {
                                if (offered && root.modeEnabled && !selected) root.modeRequested(modelData.id)
                            }
                        }
                    }
                }
            }
        }
    }

    readonly property var modelRows: root.vm.models || []
    readonly property var harnessRows: root.vm.harnesses || []
    readonly property bool canSelect: root.vm.phase !== "loading" && root.vm.phase !== "answering" && root.vm.phase !== "stopping"
    readonly property bool showHarnesses: root.vm.hasHarnessData === true && root.harnessRows.length > 0
    readonly property bool anyWarm: root.harnessRows.some(function(h) { return h.state === "warmup" })
    readonly property bool anyPreparing: root.preparing.length > 0
    readonly property string selectedName: root.vm.selected !== "" ? root.strings.t("model." + root.vm.selected + ".short") : ""

    // Flash's download is worth a line here only while it is moving, stuck, or under test.
    readonly property string flashStateKey: {
        var s = root.vm.flash.state
        if (s === "downloading") return "flash.downloading"
        if (s === "verifying") return root.vm.flash.waiting ? "flash.waitingVerify" : "flash.verifying"
        if (s === "weights-ready-testing") return "flash.readyTesting"
        if (s === "ready") return "flash.ready"
        if (s === "paused") return "flash.paused"
        return ""
    }

    spacing: Style.space(14)

    Column {
        width: root.width
        spacing: Style.space(6)

        Repeater {
            model: root.modelRows
            delegate: ModelRow {
                required property var modelData
                info: modelData
            }
        }
    }

    EmptyState {
        visible: root.modelRows.length === 0
        width: root.width
        glyph: "\uf4ad"
        title: root.strings.t(root.vm.phase === "error" || root.vm.phase === "offline" ? "empty.models.down" : "empty.models")
        hint: root.strings.t(root.vm.phase === "error" ? "now.retryTip" : root.vm.phase === "offline" ? "empty.models.downHint" : "empty.models.hint")
        foreground: root.foreground
        face: root.face
    }

    Card {
        visible: root.flashStateKey !== "" || root.vm.flash.trialActive === true
        width: root.width
        foreground: root.foreground
        gap: Style.space(6)

        Item {
            visible: root.flashStateKey !== ""
            width: parent.width
            height: flashLabel.implicitHeight

            T {
                id: flashLabel
                width: parent.width - flashPercent.width - Style.space(8)
                text: root.flashStateKey !== "" ? root.strings.t(root.flashStateKey) : ""
                font.pixelSize: Style.font.bodySmall
            }
            T {
                id: flashPercent
                anchors.right: parent.right
                text: root.strings.t("unit.percent").arg(root.fmt(root.vm.flash.percent, 1))
                dim: true
                font.pixelSize: Style.font.bodySmall
            }
        }
        Meter {
            visible: root.flashStateKey !== ""
            width: parent.width
            fraction: root.vm.flash.percent / 100
            foreground: root.foreground
            tone: root.accent
        }
        T {
            visible: root.vm.flash.trialActive === true
            width: parent.width
            text: visible ? root.strings.t("flash.trial").arg(root.fmt(Math.floor(root.vm.flash.trialElapsed / 60))) : ""
            dim: true
            font.pixelSize: Style.font.bodySmall
        }
    }

    Column {
        visible: root.showHarnesses
        width: root.width
        spacing: Style.space(8)

        Item {
            width: parent.width
            height: Math.max(harnessHeader.implicitHeight, prepareAll.visible ? prepareAll.implicitHeight : 0)

            Ui.PanelSectionHeader {
                id: harnessHeader
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                foreground: root.foreground
                fontFamily: root.face
                text: root.strings.t("harness.title")
            }

            Ui.Button {
                id: prepareAll
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                visible: root.anyWarm && !root.anyPreparing && root.harnessRows.length > 1
                text: root.strings.t("harness.prepareAll")
                enabled: !root.busy
                foreground: root.foreground
                accent: root.accent
                fontFamily: root.face
                fontSize: Style.font.bodySmall
                verticalPadding: Style.spacing.xs
                onClicked: root.prepareAllRequested()
            }
        }

        Repeater {
            model: root.harnessRows
            delegate: HarnessChip {
                required property var modelData
                width: root.width
                name: modelData.name
                markState: root.preparing.indexOf(modelData.name) >= 0 ? "preparing" : modelData.state
                estimateSeconds: modelData.estimateSeconds
                tools: modelData.tools
                cachedTokens: modelData.cachedTokens
                evaluatedTokens: modelData.evaluatedTokens
                costText: root.selectedName !== "" ? root.strings.t("harness.prepareCost").arg(root.selectedName) : ""
                strings: root.strings
                foreground: root.foreground
                accent: root.accent
                face: root.face
                busy: root.busy
                onPrepareRequested: root.prepareRequested(modelData.name)
                onForgetRequested: root.forgetRequested(modelData.name)
            }
        }
    }
}
