import QtQuick
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui
import "Model.js" as Model
import "Look.js" as Look

// The conversation cache (use, budget, clear), disk use per model with the files behind it,
// and the cache-clearing sheet.
Column {
    id: root

    width: parent ? parent.width : 0
    spacing: Style.space(10)

    property var vm
    property var strings
    property color foreground
    property color accent
    property color urgent
    property string face
    property bool busy: false
    property var storage

    signal deleteRequested(string id)
    signal clearRequested(bool prompts, bool conversations, bool harnesses)
    // Conversation cache: the budget in whole GB (0 = off), and clearing every saved conversation.
    signal budgetRequested(int gb)
    signal cacheClearRequested()

    property var cache
    property bool cacheConfirming: false
    readonly property var cacheInfo: cache ? cache : Model.parseCache("")
    readonly property var budgetChoices: [{ gb: 16 }, { gb: 32 }, { gb: 64 }, { gb: 128 }, { gb: 0 }]
    readonly property real cacheFill: cacheInfo.budgetBytes > 0 ? Math.min(1, cacheInfo.usedBytes / cacheInfo.budgetBytes) : 0
    readonly property bool cacheIdle: !!vm && vm.active === 0 && !busy
    readonly property int keptDays: Math.max(1, Math.round(cacheInfo.ttlSeconds / 86400))

    property string confirmId: ""
    // The one model block whose files are listed; "" = all folded.
    property string expandedKey: ""
    property bool sheetOpen: false
    property bool clearPrompts: true
    property bool clearConversations: false
    property bool clearHarnesses: false

    component T: Label { color: root.foreground; font.family: root.face }

    readonly property bool available: !!(storage && storage.available)
    readonly property var entries: storage && storage.entries ? storage.entries : []
    readonly property real totalBytes: Number(storage ? storage.totalBytes : 0) || 0
    readonly property real freeableBytes: Number(storage ? storage.freeableBytes : 0) || 0
    readonly property bool hasHarness: !!(vm && vm.hasHarnessData)
    readonly property bool canConfirm: (clearPrompts || clearConversations || (clearHarnesses && hasHarness)) && !busy && !!vm && vm.active === 0
    readonly property var groups: {
        var order = ["ornith", "moe", "flash", "other"];
        var out = [];
        for (var i = 0; i < order.length; i++) {
            var items = [], bytes = 0;
            for (var j = 0; j < entries.length; j++) {
                if (groupKey(entries[j].model) === order[i]) { items.push(entries[j]); bytes += entries[j].bytes; }
            }
            if (items.length > 0) out.push({ key: order[i], items: items, bytes: bytes });
        }
        return out;
    }
    readonly property var flash: vm && vm.flash ? vm.flash : ({ state: "absent" })
    readonly property string flashStateKey: {
        var s = flash.state
        if (s === "downloading") return "flash.downloading"
        if (s === "verifying") return flash.waiting ? "flash.waitingVerify" : "flash.verifying"
        if (s === "weights-ready-testing") return "flash.readyTesting"
        if (s === "paused") return "flash.paused"
        return ""
    }
    readonly property var sheetChoices: [
        { key: "prompts", shown: true },
        { key: "conversations", shown: true },
        { key: "harnesses", shown: hasHarness }
    ]

    function word(key) { return strings ? strings.t(key) : "" }
    function fmt(value, decimals) { return Commons.I18n.formatNumber(value, decimals) }
    function gbText(bytes) {
        var gb = (Number(bytes) || 0) / 1e9;
        return word("unit.gb").arg(fmt(gb, gb >= 10 ? 0 : 1));
    }
    function budgetSelected(gb) { return cacheInfo.budgetBytes === Math.round(gb * 1e9) }
    function cacheAgeText(seconds) {
        if (seconds < 3600) return word("cache.age.minutes").arg(fmt(Math.max(1, Math.floor(seconds / 60)), 0));
        if (seconds < 172800) return word("cache.age.hours").arg(fmt(Math.floor(seconds / 3600), 0));
        return word("cache.age.days").arg(fmt(Math.floor(seconds / 86400), 0));
    }
    function groupKey(modelKey) { return ["ornith", "moe", "flash"].indexOf(modelKey) >= 0 ? modelKey : "other" }
    function groupTitle(key) { return key === "other" ? word("storage.other") : word("model." + key + ".short") }
    function sizeText(bytes) {
        var n = Number(bytes) || 0;
        var gib = n / 1073741824;
        if (gib >= 0.1) return word("unit.gib").arg(fmt(gib, 1));
        return word("unit.mib").arg(fmt(n / 1048576, 0));
    }
    function choice(key) { return key === "prompts" ? clearPrompts : key === "conversations" ? clearConversations : clearHarnesses }
    function toggleChoice(key) {
        if (key === "prompts") clearPrompts = !clearPrompts;
        else if (key === "conversations") clearConversations = !clearConversations;
        else clearHarnesses = !clearHarnesses;
    }
    function closeSheet() {
        clearPrompts = true;
        clearConversations = false;
        clearHarnesses = false;
        sheetOpen = false;
    }

    // Conversation cache first: it is the part of storage that is set here. Saved conversations
    // are kept on disk so a resumed chat skips its prefill. Budget and clear are the only
    // controls; both run the CLI and take effect at once.
    Column {
        id: cacheSection

        width: root.width
        spacing: Style.space(8)

        SectionHead {
            width: parent.width
            text: root.word("cache.title")
            trailing: !root.cacheInfo.available ? ""
                : root.cacheInfo.sessions > 0
                    ? root.strings.tn("cache.sessions", root.cacheInfo.sessions).arg(root.fmt(root.cacheInfo.sessions, 0))
                : ""
            foreground: root.foreground
            face: root.face
        }
        T {
            visible: !root.cacheInfo.available
            width: parent.width
            wrapMode: Text.WordWrap
            text: root.word("cache.unavailable")
            dim: true
            font.pixelSize: Style.font.bodySmall
        }
        Card {
            visible: root.cacheInfo.available
            width: parent.width
            foreground: root.foreground
            gap: Style.space(8)

            Item {
                width: parent.width
                height: cacheUsed.implicitHeight

                T {
                    id: cacheUsed
                    width: parent.width - (cacheShare.shown ? cacheShare.implicitWidth + Style.space(8) : 0)
                    text: root.cacheInfo.budgetBytes === 0
                        ? root.word("cache.usedOff").arg(root.gbText(root.cacheInfo.usedBytes))
                        : root.word("cache.used").arg(root.gbText(root.cacheInfo.usedBytes)).arg(root.gbText(root.cacheInfo.budgetBytes))
                    font.pixelSize: Style.font.subtitle
                    font.bold: true
                }
                T {
                    id: cacheShare
                    readonly property bool shown: root.cacheInfo.budgetBytes > 0
                    visible: shown
                    anchors.right: parent.right
                    anchors.baseline: cacheUsed.baseline
                    text: root.word("unit.percent").arg(root.fmt(Math.round(root.cacheFill * 100), 0))
                    dim: true
                    font.pixelSize: Style.font.bodySmall
                }
            }
            Meter {
                visible: root.cacheInfo.budgetBytes > 0
                width: parent.width
                fraction: root.cacheFill
                foreground: root.foreground
                tone: root.accent
            }
            T {
                width: parent.width
                wrapMode: Text.WordWrap
                text: root.cacheInfo.sessions > 0
                    ? (root.cacheInfo.oldestAgeS >= 0 ? root.word("cache.oldestLine").arg(root.cacheAgeText(root.cacheInfo.oldestAgeS)) + " " : "")
                        + root.strings.tn("cache.keptDays", root.keptDays).arg(root.fmt(root.keptDays, 0))
                    : root.word("cache.none")
                dim: true
                font.pixelSize: Style.font.caption
            }

            Item {
                width: parent.width
                height: Math.max(budgetTitle.implicitHeight, budgetRow.implicitHeight)

                T {
                    id: budgetTitle
                    anchors.verticalCenter: parent.verticalCenter
                    width: Style.space(56)
                    text: root.word("cache.budget")
                    dim: true
                    font.pixelSize: Style.font.bodySmall

                    MouseArea { id: budgetHover; anchors.fill: parent; hoverEnabled: true }
                    Ui.PanelToolTip {
                        visible: budgetHover.containsMouse
                        text: root.word("cache.defaultNote").arg(root.gbText(root.cacheInfo.defaultBytes))
                        width: Math.min(implicitWidth, Style.space(340))
                        fontFamily: root.face
                    }
                }
                Row {
                    id: budgetRow
                    x: budgetTitle.width + Style.space(8)
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Style.spacing.md

                    Repeater {
                        model: root.budgetChoices

                        delegate: Ui.Button {
                            required property var modelData
                            readonly property bool chosen: root.budgetSelected(modelData.gb)

                            text: modelData.gb === 0 ? root.word("cache.budget.off") : root.word("unit.gb").arg(root.fmt(modelData.gb, 0))
                            tooltipText: modelData.gb === 0 ? root.word("cache.budget.offTip")
                                : root.word("cache.budget.tip").arg(root.word("unit.gb").arg(root.fmt(modelData.gb, 0)))
                            selected: chosen
                            bordered: true
                            foreground: root.foreground
                            accent: root.accent
                            fontFamily: root.face
                            fontSize: Style.font.bodySmall
                            enabled: !root.busy
                            onClicked: root.budgetRequested(modelData.gb)
                        }
                    }
                }
            }
            T {
                visible: root.cacheInfo.invalid
                width: parent.width
                wrapMode: Text.WordWrap
                text: root.word("cache.invalid")
                color: root.urgent
                font.pixelSize: Style.font.caption
            }

            Ui.Button {
                visible: !root.cacheConfirming
                text: root.word("cache.clear")
                bordered: true
                foreground: root.foreground
                accent: root.accent
                fontFamily: root.face
                fontSize: Style.font.bodySmall
                enabled: root.cacheIdle
                opacity: enabled ? 1 : 0.5
                onClicked: root.cacheConfirming = true
            }
            Callout {
                visible: root.cacheConfirming
                width: parent.width
                text: root.vm && root.vm.active > 0 ? root.word("clear.blocked") : root.word("cache.clearConfirm")
                foreground: root.foreground
                accent: root.accent
                tone: root.urgent
                face: root.face
                maximumLineCount: 5
            }
            Row {
                visible: root.cacheConfirming
                spacing: Style.space(6)

                Ui.Button {
                    id: cacheDo
                    text: root.word("cache.clearAll")
                    bordered: true
                    foreground: root.urgent
                    accent: root.accent
                    fontFamily: root.face
                    fontSize: Style.font.bodySmall
                    enabled: root.cacheIdle
                    opacity: enabled ? 1 : 0.5
                    onClicked: {
                        root.cacheClearRequested();
                        root.cacheConfirming = false;
                    }
                }
                Ui.Button {
                    id: cacheCancel
                    text: root.word("clear.cancel")
                    foreground: root.foreground
                    accent: root.accent
                    fontFamily: root.face
                    fontSize: Style.font.bodySmall
                    onClicked: root.cacheConfirming = false
                }
            }
        }
    }

    // What is on disk: the total, then one line and bar per model; a click lists its files.
    SectionHead {
        visible: root.available
        width: root.width
        text: root.word("storage.title")
        trailing: root.available ? root.word("storage.totalLine").arg(root.sizeText(root.totalBytes)) : ""
        foreground: root.foreground
        face: root.face
    }
    EmptyState {
        visible: !root.available
        width: root.width
        glyph: "\uf0a0"
        title: root.word("storage.unavailable")
        foreground: root.foreground
        face: root.face
    }
    Callout {
        visible: root.available && root.freeableBytes > 0
        width: root.width
        text: root.word("storage.freeableLine").arg(root.sizeText(root.freeableBytes))
        foreground: root.foreground
        accent: root.accent
        tone: root.foreground
        face: root.face
    }

    Repeater {
        model: root.available ? root.groups : []

        delegate: Column {
            id: groupBox

            required property var modelData
            readonly property var group: modelData

            readonly property bool open: root.expandedKey === group.key
            readonly property int unusedCount: group.items.filter(function(e) { return e.state === "unreferenced" }).length

            width: root.width
            spacing: Style.space(6)

            Item {
                width: parent.width
                height: groupHead.height

                Column {
                    id: groupHead
                    width: parent.width
                    spacing: Style.space(6)

                    Item {
                        width: parent.width
                        height: groupName.implicitHeight

                        T {
                            id: groupMark
                            anchors.baseline: groupName.baseline
                            width: Style.space(14)
                            text: groupBox.open ? "\uf107" : Commons.I18n.rightToLeft ? "\uf104" : "\uf105"
                            dim: !groupHover.containsMouse
                            font.pixelSize: Style.font.bodySmall
                        }
                        T {
                            id: groupName
                            x: groupMark.width
                            width: Math.min(implicitWidth, parent.width - x - groupNote.width - groupSize.width - Style.space(16))
                            text: root.groupTitle(groupBox.group.key)
                            font.pixelSize: Style.font.subtitle
                            font.bold: true
                        }
                        T {
                            id: groupNote
                            anchors.left: groupName.right
                            anchors.leftMargin: Style.space(8)
                            anchors.baseline: groupName.baseline
                            text: groupBox.unusedCount > 0
                                ? root.strings.tn("storage.unused", groupBox.unusedCount).arg(root.fmt(groupBox.unusedCount, 0))
                                : root.strings.tn("storage.files", groupBox.group.items.length).arg(root.fmt(groupBox.group.items.length, 0))
                            dim: true
                            font.pixelSize: Style.font.caption
                        }
                        T {
                            id: groupSize
                            anchors.right: parent.right
                            anchors.baseline: groupName.baseline
                            text: root.sizeText(groupBox.group.bytes)
                        }
                    }

                    Meter {
                        x: groupMark.width
                        width: parent.width - x
                        fraction: root.totalBytes > 0 ? groupBox.group.bytes / root.totalBytes : 0
                        foreground: root.foreground
                        tone: groupBox.open || groupHover.containsMouse ? root.accent : Look.tint(root.foreground, Look.quiet)
                    }
                }

                MouseArea {
                    id: groupHover
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: {
                        root.confirmId = ""
                        root.expandedKey = groupBox.open ? "" : groupBox.group.key
                    }
                }
            }

            Repeater {
                model: groupBox.open ? groupBox.group.items : []

                delegate: Column {
                    id: entryBox

                    required property var modelData
                    readonly property var item: modelData
                    readonly property bool confirming: root.confirmId !== "" && root.confirmId === item.id
                    readonly property bool unused: item.state === "unreferenced"

                    x: Style.space(14)
                    width: groupBox.width - x
                    spacing: Style.space(4)

                    Item {
                        width: parent.width
                        height: Math.max(kindLabel.implicitHeight, deleteButton.visible ? deleteButton.implicitHeight : 0)

                        T {
                            id: kindLabel
                            anchors.verticalCenter: parent.verticalCenter
                            width: Math.min(implicitWidth, parent.width - stateLabel.width - sizeLabel.width - (deleteButton.visible ? deleteButton.width + Style.space(8) : 0) - Style.space(16))
                            text: root.word("storage.kind." + entryBox.item.kind)
                            dim: true
                            font.pixelSize: Style.font.bodySmall
                        }
                        T {
                            id: stateLabel
                            anchors.left: kindLabel.right
                            anchors.leftMargin: Style.space(8)
                            anchors.baseline: kindLabel.baseline
                            visible: entryBox.item.state !== "referenced"
                            width: visible ? implicitWidth : 0
                            text: root.word("storage.state." + entryBox.item.state)
                            color: entryBox.item.state === "in-use" ? root.accent : root.foreground
                            dim: entryBox.item.state !== "in-use"
                            font.pixelSize: Style.font.caption
                        }
                        T {
                            id: sizeLabel
                            anchors.right: deleteButton.visible ? deleteButton.left : parent.right
                            anchors.rightMargin: deleteButton.visible ? Style.space(8) : 0
                            anchors.baseline: kindLabel.baseline
                            text: root.sizeText(entryBox.item.bytes)
                            dim: true
                            font.pixelSize: Style.font.bodySmall
                        }
                        Ui.Button {
                            id: deleteButton
                            visible: !!entryBox.item.deletable && !entryBox.confirming
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
                            text: root.word("storage.delete")
                            fontSize: Style.font.caption
                            verticalPadding: Style.spacing.xs
                            bordered: true
                            foreground: root.foreground
                            fontFamily: root.face
                            enabled: !root.busy
                            onClicked: root.confirmId = entryBox.item.id
                        }
                    }

                    Item {
                        visible: entryBox.confirming
                        width: parent.width
                        height: Math.max(confirmLabel.implicitHeight, confirmButton.implicitHeight)

                        T {
                            id: confirmLabel
                            anchors.verticalCenter: parent.verticalCenter
                            width: parent.width - confirmButton.width - cancelButton.width - Style.space(16)
                            text: root.word("storage.deleteConfirm")
                            font.pixelSize: Style.font.bodySmall
                        }
                        Ui.Button {
                            id: confirmButton
                            anchors.right: cancelButton.left
                            anchors.rightMargin: Style.space(6)
                            anchors.verticalCenter: parent.verticalCenter
                            text: root.word("storage.delete")
                            fontSize: Style.font.caption
                            verticalPadding: Style.spacing.xs
                            bordered: true
                            foreground: root.urgent
                            fontFamily: root.face
                            enabled: !root.busy
                            onClicked: {
                                root.deleteRequested(entryBox.item.id);
                                root.confirmId = "";
                            }
                        }
                        Ui.Button {
                            id: cancelButton
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
                            text: root.word("clear.cancel")
                            fontSize: Style.font.caption
                            verticalPadding: Style.spacing.xs
                            bordered: true
                            foreground: root.foreground
                            fontFamily: root.face
                            onClicked: root.confirmId = ""
                        }
                    }
                }
            }

            // Flash's download and verification detail, kept out of the Models tab.
            T {
                visible: groupBox.open && groupBox.group.key === "flash" && root.flashStateKey !== ""
                x: Style.space(14)
                width: parent.width - x
                wrapMode: Text.WordWrap
                text: visible
                    ? root.word("flash.progress")
                        .arg(root.word(root.flashStateKey))
                        .arg(root.fmt(Model.gib(root.flash.doneBytes), 1))
                        .arg(root.fmt(Model.gib(root.flash.totalBytes), 1))
                        .arg(root.fmt(root.flash.verified))
                        .arg(root.fmt(root.flash.shards))
                    : ""
                dim: true
                font.pixelSize: Style.font.caption
            }
        }
    }

    Ui.Button {
        visible: !root.sheetOpen
        text: root.word("storage.clear")
        bordered: true
        foreground: root.foreground
        accent: root.accent
        fontFamily: root.face
        fontSize: Style.font.bodySmall
        onClicked: root.sheetOpen = true
    }

    Rectangle {
        id: sheet

        readonly property real pad: Style.space(12)

        visible: root.sheetOpen
        width: root.width
        height: sheetColumn.height + 2 * sheet.pad
        color: Look.tint(root.foreground, Look.fill)
        border.width: Math.max(1, Style.space(1))
        border.color: Look.tint(root.foreground, Look.line)
        radius: Style.cornerRadius

        Column {
            id: sheetColumn

            x: sheet.pad
            y: sheet.pad
            width: sheet.width - 2 * sheet.pad
            spacing: Style.space(12)

            Ui.PanelSectionHeader {
                foreground: root.foreground
                fontFamily: root.face
                text: root.word("clear.title")
            }

            Repeater {
                model: root.sheetChoices

                delegate: Row {
                    id: choiceRow

                    required property var modelData

                    visible: modelData.shown
                    width: sheetColumn.width
                    spacing: Style.space(10)

                    Ui.ToggleSwitch {
                        id: choiceSwitch
                        checked: root.choice(choiceRow.modelData.key)
                        busy: root.busy
                        foreground: root.foreground
                        accent: root.accent
                        onToggled: root.toggleChoice(choiceRow.modelData.key)
                    }
                    Column {
                        width: parent.width - choiceSwitch.width - parent.spacing
                        spacing: Style.space(2)

                        T {
                            width: parent.width
                            text: root.word("clear." + choiceRow.modelData.key)
                            font.pixelSize: Style.font.bodySmall
                        }
                        T {
                            width: parent.width
                            wrapMode: Text.WordWrap
                            text: root.word("clear." + choiceRow.modelData.key + "Note")
                            dim: true
                            font.pixelSize: Style.font.caption
                        }
                    }
                }
            }

            Item {
                width: parent.width
                height: sheetConfirm.implicitHeight

                T {
                    visible: !!root.vm && root.vm.active > 0
                    anchors.verticalCenter: parent.verticalCenter
                    width: parent.width - sheetCancel.width - sheetConfirm.width - Style.space(16)
                    text: root.word("clear.blocked")
                    dim: true
                    font.pixelSize: Style.font.caption
                }
                Ui.Button {
                    id: sheetCancel
                    anchors.right: sheetConfirm.left
                    anchors.rightMargin: Style.space(6)
                    text: root.word("clear.cancel")
                    foreground: root.foreground
                    fontFamily: root.face
                    onClicked: root.closeSheet()
                }
                Ui.Button {
                    id: sheetConfirm
                    anchors.right: parent.right
                    text: root.word("clear.confirm")
                    bordered: true
                    foreground: root.foreground
                    fontFamily: root.face
                    enabled: root.canConfirm
                    opacity: enabled ? 1 : 0.5
                    onClicked: {
                        root.clearRequested(root.clearPrompts, root.clearConversations, root.clearHarnesses && root.hasHarness);
                        root.sheetOpen = false;
                    }
                }
            }
        }
    }
}
