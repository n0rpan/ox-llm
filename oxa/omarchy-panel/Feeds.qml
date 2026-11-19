import QtQuick
import Quickshell
import Quickshell.Io

// The panel's data feeds (design section 2.5): `<oxa> <command> --json` as an argument list, with
// cadence, timeouts and the capability guard. No file, no HTTP request, no key path. Answers go to
// Model.js through `answered`.
QtObject {
    id: feeds

    // Widget settings and state, set by the panel.
    property string oxaPath: ""
    property bool pollEnabled: true
    property bool pollingPaused: false
    property bool opened: false
    // The tab on screen: "models", "usage", "storage", or anything else (no tab feed runs).
    property string tab: ""

    readonly property bool active: feeds.pollEnabled && !feeds.pollingPaused
    readonly property string binary: feeds.oxaPath !== "" ? feeds.oxaPath
        : (Quickshell.env("HOME") || "") + "/.local/bin/oxa"

    // Words of the last status answer of this run. Empty until a status answer arrives, and empty
    // again after a failed one, so a guarded command never runs on an oxa of unknown capability.
    property var capabilities: []
    // usage_state of the last status answer ("" when unknown).
    property string usageState: ""
    // Last successful answer per command: { text, atMs }. A failed run never replaces it.
    property var good: ({})
    // Runs in flight or just finished, per command.
    property var runs: ({})

    // One answer per run. code is -1 when the command never gave an exit code.
    signal answered(string kind, string text, int code, bool timedOut)

    // Status is always allowed; the others only when the last status answer listed them.
    function allowed(kind) {
        if (kind === "status") return true
        if (["models", "usage", "storage"].indexOf(kind) < 0) return false
        return feeds.capabilities.indexOf(kind) >= 0
    }

    function procFor(kind) {
        if (kind === "status") return statusProc
        if (kind === "models") return modelsProc
        if (kind === "usage") return usageProc
        return storageProc
    }

    function killFor(kind) {
        if (kind === "status") return statusKill
        if (kind === "models") return modelsKill
        if (kind === "usage") return usageKill
        return storageKill
    }

    function graceFor(kind) {
        if (kind === "status") return statusGrace
        if (kind === "models") return modelsGrace
        if (kind === "usage") return usageGrace
        return storageGrace
    }

    // One of a kind at a time: a poll that finds its command still running is skipped.
    function run(kind) {
        if (!feeds.allowed(kind)) return false
        var p = feeds.procFor(kind)
        if (p.running) return false
        feeds.runs[kind] = { text: "", code: null, streamDone: false, timedOut: false, done: false }
        p.command = [feeds.binary, kind, "--json"]
        p.running = true
        feeds.killFor(kind).restart()
        return true
    }

    // On open: status, models and usage, plus the tab's own command.
    function refreshOnOpen() {
        feeds.run("status")
        feeds.run("models")
        feeds.run("usage")
        if (feeds.tab === "storage") feeds.run("storage")
    }

    // After an action: status and models at once.
    function afterAction() {
        feeds.run("status")
        feeds.run("models")
    }

    function collect(kind, text) {
        var r = feeds.runs[kind]
        if (!r || r.done) return
        r.text = text
        r.streamDone = true
        if (r.code !== null) feeds.finish(kind)
    }

    function exited(kind, code) {
        var r = feeds.runs[kind]
        if (!r || r.done) return
        r.code = code
        if (r.streamDone) feeds.finish(kind)
    }

    // A command that never started gives no output and may give no exit code. A short grace
    // lets the normal pair arrive; then whatever has come is the answer.
    function stopped(kind) {
        var r = feeds.runs[kind]
        if (r && !r.done) feeds.graceFor(kind).restart()
    }

    // The timeout: the process is stopped and the run is reported as timed out.
    function expire(kind) {
        var r = feeds.runs[kind]
        if (!r || r.done) return
        r.timedOut = true
        feeds.procFor(kind).running = false
    }

    function finish(kind) {
        var r = feeds.runs[kind]
        if (!r || r.done) return
        r.done = true
        feeds.killFor(kind).stop()
        feeds.graceFor(kind).stop()
        var code = r.code === null ? -1 : r.code
        var ok = code === 0 && !r.timedOut
        if (ok) {
            var g = feeds.good
            g[kind] = { text: r.text, atMs: Date.now() }
            feeds.good = g
        }
        if (kind === "status") feeds.applyStatus(ok, r.text)
        feeds.answered(kind, r.text, code, r.timedOut)
    }

    // Anything but a version 1 suite status object leaves the guard closed.
    function applyStatus(ok, text) {
        var caps = []
        var state = ""
        if (ok) {
            try {
                var o = JSON.parse(text)
                if (o && o.type === "oxa_suite_status" && o.version === 1 && Array.isArray(o.capabilities)) {
                    caps = o.capabilities.filter(function (c) { return typeof c === "string" })
                    state = typeof o.usage_state === "string" ? o.usage_state : ""
                }
            } catch (e) {
                caps = []
            }
        }
        var before = feeds.capabilities.join(",")
        var stateChanged = state !== feeds.usageState
        feeds.capabilities = caps
        feeds.usageState = state
        if (feeds.opened && caps.join(",") !== before) feeds.refreshOnOpen()
        else if (feeds.opened && stateChanged) feeds.run("usage")
    }

    onOpenedChanged: if (feeds.opened) feeds.refreshOnOpen()
    onTabChanged: if (feeds.opened && feeds.tab !== "status") feeds.run(feeds.tab)

    Process {
        id: statusProc
        stdout: StdioCollector { waitForEnd: true; onStreamFinished: feeds.collect("status", text) }
        onExited: function (code) { feeds.exited("status", code) }
        onRunningChanged: if (!statusProc.running) feeds.stopped("status")
    }

    Process {
        id: modelsProc
        stdout: StdioCollector { waitForEnd: true; onStreamFinished: feeds.collect("models", text) }
        onExited: function (code) { feeds.exited("models", code) }
        onRunningChanged: if (!modelsProc.running) feeds.stopped("models")
    }

    Process {
        id: usageProc
        stdout: StdioCollector { waitForEnd: true; onStreamFinished: feeds.collect("usage", text) }
        onExited: function (code) { feeds.exited("usage", code) }
        onRunningChanged: if (!usageProc.running) feeds.stopped("usage")
    }

    Process {
        id: storageProc
        stdout: StdioCollector { waitForEnd: true; onStreamFinished: feeds.collect("storage", text) }
        onExited: function (code) { feeds.exited("storage", code) }
        onRunningChanged: if (!storageProc.running) feeds.stopped("storage")
    }

    Timer { id: statusKill; interval: 3000; onTriggered: feeds.expire("status") }
    Timer { id: modelsKill; interval: 5000; onTriggered: feeds.expire("models") }
    Timer { id: usageKill; interval: 5000; onTriggered: feeds.expire("usage") }
    Timer { id: storageKill; interval: 10000; onTriggered: feeds.expire("storage") }

    Timer { id: statusGrace; interval: 500; onTriggered: feeds.finish("status") }
    Timer { id: modelsGrace; interval: 500; onTriggered: feeds.finish("models") }
    Timer { id: usageGrace; interval: 500; onTriggered: feeds.finish("usage") }
    Timer { id: storageGrace; interval: 500; onTriggered: feeds.finish("storage") }

    // Cadence: status while polling is on; the tab feeds only while their tab is shown and open.
    Timer { id: statusTimer; interval: 5000; running: feeds.active; repeat: true; triggeredOnStart: true; onTriggered: feeds.run("status") }

    Timer { id: modelsTimer; interval: 60000; running: feeds.active && feeds.opened && feeds.tab === "models"; repeat: true; triggeredOnStart: true; onTriggered: feeds.run("models") }

    Timer { id: usageTimer; interval: 30000; running: feeds.active && feeds.opened && feeds.tab === "usage"; repeat: true; triggeredOnStart: true; onTriggered: feeds.run("usage") }

    Timer { id: storageTimer; interval: 60000; running: feeds.active && feeds.opened && feeds.tab === "storage"; repeat: true; triggeredOnStart: true; onTriggered: feeds.run("storage") }
}
