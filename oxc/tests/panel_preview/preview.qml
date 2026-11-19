// Offscreen preview of the panel's view components with fixture data. Not part of the plugin.
// Run: tests/panel_preview/render.sh <status.json> <out.png>
import QtQuick
import qs.Commons
import qs.Commons as Commons
import "../../../oxa/omarchy-panel" as Plugin
import "../../../oxa/omarchy-panel/Model.js" as Model

Rectangle {
  id: root
  readonly property var args: Qt.application.arguments
  readonly property string statusFile: args[args.length - 2]
  readonly property string outFile: args[args.length - 1]
  readonly property string fixtures: Qt.resolvedUrl("../fixtures/panel/")
  function read(url) { var x = new XMLHttpRequest(); x.open("GET", url, false); x.send(); return x.responseText }
  property var vm: Model.parse(read("file://" + statusFile), 0)
  // Fixture times are replaced by recent ones so the "When" column reads as it would live.
  function recent(text) {
    var value = JSON.parse(text), gaps = [20, 130, 860, 2400, 7300]
    for (var i = 0; i < value.rows.length; i++) if (value.rows[i]) value.rows[i].t = Date.now() / 1000 - gaps[i % gaps.length]
    return JSON.stringify(value)
  }
  property var ledger: Model.parseLedger(recent(read(fixtures + "ledger_cache.json")))
  readonly property var answer: Model.lastAnswer(root.ledger, root.vm.loaded)
  property var storage: Model.parseStorage(read(fixtures + "storage.json"))
  property var cache: Model.parseCache(read(fixtures + "cache.json"))
  width: 1320; height: Math.max(a.height, b.height, c.height) + 40
  color: "#0e0e14"
  Commons.Translations { id: localAiStrings; catalog: JSON.parse(root.read(Qt.resolvedUrl("../../../oxa/omarchy-panel/i18n/en.json"))) }
  component Card: Rectangle {
    default property alias content: inner.data
    width: 420; height: inner.height + 28; color: Commons.Color.background; y: 20
    Column { id: inner; x: 14; y: 14; width: parent.width - 28; spacing: Style.space(10) }
  }
  Timer { interval: 600; running: true; onTriggered: root.grabToImage(function(r) { r.saveToFile(root.outFile); Qt.quit() }) }
}
