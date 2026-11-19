.pragma library

// The ox AI panel's model, part 1: the answer of `oxa status --json` (section 2.2 of
// oxa/docs/pages/panel.md), the one-line hero headline and the three state chips.
// Pure functions: no process, file, network or clock. The text the user reads is chosen by the
// caller from i18n/en.json, keyed by the ids returned here. Unknown enum values become "unknown"
// and the raw word stays in `raw`, so a newer oxa never breaks the panel.

var CAPABILITIES = ["models", "usage", "storage", "panel_install", "local_add", "studio", "settings_flag"];
var LOCAL_STATES = ["off", "not_wired", "ready", "running", "error"];
var CLOUD_STATES = ["off", "ready", "no_credentials", "error"];
var LEAD_STATES = ["off", "idle", "pursuing", "awaiting_owner", "paused", "stalled"];
var USAGE_STATES = ["none", "ok", "low", "over", "unknown"];
var ENGINE_BINARY = ["found", "missing"];
var KEY_FILES = ["present", "missing", "unsafe_mode"];
var CHATGPT_LOGIN = ["present", "missing"];
var FIXES = ["local_add", "enable_local", "install_engine", "add_key", "fix_key_mode", "enable_cloud", "fix_config"];
var WORKER_STATES = ["starting", "working", "idle", "finished", "dead", "stopped"];
var MAX_ROWS = 50;
var MAX_RUNNING = 20;

function oneOf(list, value, fallback) {
    return list.indexOf(value) >= 0 ? value : fallback;
}

// A finite number in 0..max, else -1 (unknown).
function num(n, max) {
    return typeof n === "number" && isFinite(n) && n >= 0 && n <= max ? n : -1;
}

// A whole number in 0..max, else 0.
function count(n, max) {
    return typeof n === "number" && isFinite(n) && Math.floor(n) === n && n >= 0 && n <= max ? n : 0;
}

// One bounded line of plain text (control characters removed), else "".
function plain(value, max) {
    return typeof value === "string" ? value.replace(/[\u0000-\u001f\u007f]+/g, " ").trim().slice(0, max) : "";
}

function isObject(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
}

function asObject(value) {
    return isObject(value) ? value : {};
}

function asList(value) {
    return Array.isArray(value) ? value : [];
}

function asFix(value) {
    return oneOf(FIXES, value, null);
}

// The status shape with every field present and safe. `fault` is null for a status that was
// read, else one of the fault ids of section 2.6: missing, timeout, old, failed, unreadable, newer.
function emptyStatus(fault, code) {
    return {
        fault: fault,
        code: plain(code, 64),
        oxaVersion: "",
        panelBuild: "",
        capabilities: [],
        generatedMs: -1,
        defaultModel: "",
        local: { state: "unknown", raw: "", enabled: false, engineBinary: "unknown", wired: 0,
                 onDiskUnwired: 0, running: [], fix: null },
        cloud: { state: "unknown", raw: "", enabled: false, routes: 0, keyFile: "unknown",
                 chatgptLogin: "unknown", fix: null },
        lead: { state: "unknown", raw: "", configured: false, goalStatus: "", openAsks: 0, model: "",
                lastActivityMs: -1, serve: { enabled: false, listening: false, url: "", settings: false } },
        sessions: { live: 0, working: 0, stalled: 0, lastActivityMs: -1 },
        workers: { working: 0, idle: 0, starting: 0, finished: 0, dead: 0, stopped: 0, stalled: 0, rows: [] },
        usageState: "unknown"
    };
}

function parseObject(body) {
    try {
        return asObject(JSON.parse(body));
    } catch (e) {
        return null;
    }
}

function workerRow(value) {
    var row = asObject(value);
    return {
        name: plain(row.name, 64),
        state: oneOf(WORKER_STATES, row.state, "unknown"),
        mode: plain(row.mode, 32),
        model: plain(row.model, 120),
        branch: plain(row.branch, 120),
        startedMs: num(row.started_ms, 1e15),
        lastProgressMs: num(row.last_progress_ms, 1e15),
        stalled: row.stalled === true,
        outcome: plain(row.outcome, 32)
    };
}

// The fields of a status answer that passed the type and version checks.
function normalize(answer) {
    var oxa = asObject(answer.oxa);
    var local = asObject(answer.local);
    var cloud = asObject(answer.cloud);
    var lead = asObject(answer.lead);
    var serve = asObject(lead.serve);
    var sessions = asObject(answer.sessions);
    var workers = asObject(answer.workers);
    var status = emptyStatus(null, "");

    status.oxaVersion = plain(oxa.version, 32);
    status.panelBuild = plain(oxa.panel_build, 32);
    status.capabilities = asList(answer.capabilities).filter(function (name) {
        return CAPABILITIES.indexOf(name) >= 0;
    });
    status.generatedMs = num(answer.generated_ms, 1e15);
    status.defaultModel = plain(answer.default_model, 200);

    status.local = {
        state: oneOf(LOCAL_STATES, local.state, "unknown"),
        raw: plain(local.state, 32),
        enabled: local.enabled === true,
        engineBinary: oneOf(ENGINE_BINARY, local.engine_binary, "unknown"),
        wired: count(local.wired, 1e6),
        onDiskUnwired: count(local.on_disk_unwired, 1e6),
        running: asList(local.running).slice(0, MAX_RUNNING).map(function (value) {
            var row = asObject(value);
            return { model: plain(row.model, 200), sessions: count(row.sessions, 1e6) };
        }).filter(function (row) {
            return row.model !== "";
        }),
        fix: asFix(local.fix)
    };

    status.cloud = {
        state: oneOf(CLOUD_STATES, cloud.state, "unknown"),
        raw: plain(cloud.state, 32),
        enabled: cloud.enabled === true,
        routes: count(cloud.routes, 1e6),
        keyFile: oneOf(KEY_FILES, cloud.key_file, "unknown"),
        chatgptLogin: oneOf(CHATGPT_LOGIN, cloud.chatgpt_login, "unknown"),
        fix: asFix(cloud.fix)
    };

    status.lead = {
        state: oneOf(LEAD_STATES, lead.state, "unknown"),
        raw: plain(lead.state, 32),
        configured: lead.configured === true,
        goalStatus: plain(lead.goal_status, 32),
        openAsks: count(lead.open_asks, 1e6),
        model: plain(lead.model, 200),
        lastActivityMs: num(lead.last_activity_ms, 1e15),
        serve: {
            enabled: serve.enabled === true,
            listening: serve.listening === true,
            url: plain(serve.url, 200),
            settings: serve.settings === true
        }
    };

    status.sessions = {
        live: count(sessions.live, 1e6),
        working: count(sessions.working, 1e6),
        stalled: count(sessions.stalled, 1e6),
        lastActivityMs: num(sessions.last_activity_ms, 1e15)
    };

    status.workers = {
        working: count(workers.working, 1e6),
        idle: count(workers.idle, 1e6),
        starting: count(workers.starting, 1e6),
        finished: count(workers.finished, 1e6),
        dead: count(workers.dead, 1e6),
        stopped: count(workers.stopped, 1e6),
        stalled: count(workers.stalled, 1e6),
        rows: asList(workers.rows).slice(0, MAX_ROWS).map(workerRow).filter(function (row) {
            return row.name !== "";
        })
    };

    status.usageState = oneOf(USAGE_STATES, answer.usage_state, "unknown");
    return status;
}

// Reads the answer of `oxa status --json`.
//   text      what the command printed on stdout
//   exitCode  its exit code, or null when the process never started or is not known
//   timedOut  true when the panel stopped the command after its timeout (section 2.6)
// Returns the status shape of emptyStatus: `fault` is null for a readable answer.
function parseStatus(text, exitCode, timedOut) {
    var code = typeof exitCode === "number" && isFinite(exitCode) ? exitCode : null;
    var body = typeof text === "string" ? text : "";

    if (timedOut === true) {
        return emptyStatus("timeout", "");
    }
    // Did not start: no exit code and no output, a shell's "not found" (127) or "not executable" (126).
    if ((code === null && body.trim() === "") || code === 126 || code === 127 || (code !== null && code < 0)) {
        return emptyStatus("missing", "");
    }

    var answer = parseObject(body);
    if (answer === null) {
        // An oxa without `status --json` prints its usage text and exits non-zero.
        return emptyStatus(code !== null && code !== 0 ? "old" : "unreadable", "");
    }
    if (typeof answer.version === "number" && answer.version > 1) {
        return emptyStatus("newer", "");
    }
    if (answer.type === "oxa_error") {
        return emptyStatus("failed", answer.code);
    }
    if (answer.type !== "oxa_suite_status" || answer.version !== 1 || (code !== null && code !== 0)) {
        return emptyStatus("unreadable", "");
    }
    return normalize(answer);
}

// The hero's headline: the first rule that matches (section 1.1). Returns
//   id     missing, timeout, old, failed, unreadable, newer (faults) or
//          asks, stalled, usage, working, ready, no_model
//   count  asks, stalled sessions, or working sessions
//   name   for stalled: the first stalled worker's name ("" when only sessions are stalled)
//   state  for usage: the worst pool state from oxa ("low" or "over")
function headline(status) {
    if (status.fault !== null) {
        return { id: status.fault, count: 0, name: "", state: "" };
    }
    if (status.lead.openAsks > 0) {
        return { id: "asks", count: status.lead.openAsks, name: "", state: "" };
    }
    var stalled = Math.max(status.sessions.stalled, status.workers.stalled);
    if (stalled > 0) {
        var first = status.workers.rows.filter(function (row) {
            return row.stalled;
        })[0];
        return { id: "stalled", count: stalled, name: first ? first.name : "", state: "" };
    }
    if (status.usageState === "low" || status.usageState === "over") {
        return { id: "usage", count: 0, name: "", state: status.usageState };
    }
    if (status.sessions.working > 0) {
        return { id: "working", count: status.sessions.working, name: "", state: "" };
    }
    return { id: status.defaultModel === "" ? "no_model" : "ready", count: 0, name: "", state: "" };
}

// The three state chips, always in this order: Local, Cloud, Lead. Each names the tab that
// explains it. `options.leadUrl` (the widget setting) turns a lead that is off on this machine
// into "remote": the lead runs elsewhere and the chip offers its address.
//   state  the oxa word (local: off, not_wired, ready, running, error; cloud: off, ready,
//          no_credentials, error; lead: off, idle, pursuing, awaiting_owner, paused, stalled,
//          or remote) or "unknown"
//   raw    the word oxa sent, for an unknown value
function chips(status, options) {
    var leadUrl = options && typeof options.leadUrl === "string" ? plain(options.leadUrl, 200) : "";
    var running = status.local.running;
    var leadRemote = status.lead.state === "off" && leadUrl !== "";
    return [
        { id: "local", tab: "models", state: status.local.state, raw: status.local.raw,
          model: running.length > 0 ? running[0].model : "", count: running.length, fix: status.local.fix },
        { id: "cloud", tab: "models", state: status.cloud.state, raw: status.cloud.raw,
          model: "", count: status.cloud.routes, fix: status.cloud.fix },
        { id: "lead", tab: "lead", state: leadRemote ? "remote" : status.lead.state, raw: status.lead.raw,
          model: status.lead.model, count: status.lead.openAsks, fix: null, url: leadRemote ? leadUrl : "" }
    ];
}
