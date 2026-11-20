//! Settings that exist in the schema but have no row in the menus: lists and objects edited as
//! raw JSON, pi keys oxa accepts, and keys the menus leave out. A hidden row declares the key, its
//! kind and its default; `descriptors()` and the other lookups skip it, `schema()` reads it.

use super::{d, Descriptor, Group};
use crate::schema::Kind;

/// The hidden rows, sorted by key.
pub(super) const ENTRIES: &[Descriptor] = &[
    // C2: skills active before a compaction come back after it when the owner opts in.
    d(
        "compaction.reactivateSkills",
        Group::Advanced,
        "",
        "compaction.reactivateSkills",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    // #1512 S2: exact rule keys saved by "always allow" in a dangerous-command prompt; user
    // settings only (a project value is never read).
    d(
        "oxa.approval.always",
        Group::Advanced,
        "",
        "oxa.approval.always",
        "",
    )
    .key(Kind::StringList, "[]")
    .hidden(),
    // #1512: dangerous-command rules (+id/-id/cmd:..); user settings only.
    d(
        "oxa.approval.dangerous",
        Group::Advanced,
        "",
        "oxa.approval.dangerous",
        "",
    )
    .key(Kind::StringList, "[]")
    .hidden(),
    // #1512 S5: what an unattended run does with a dangerous command (no one to ask): refuse it,
    // file an approval request, or `auto` (refuse under the OS sandbox, request without it).
    // User settings only.
    d(
        "oxa.approval.unattended",
        Group::Advanced,
        "",
        "oxa.approval.unattended",
        "",
    )
    .key(Kind::Enum(&["auto", "refuse", "request"]), "\"auto\"")
    .hidden(),
    // F5: seconds; unset = 120; 0 = none.
    // Background jobs that may run at once (`bash {background: true}`); at least 1.
    d("oxa.bash.maxJobs", Group::Advanced, "", "oxa.bash.maxJobs", "")
        .key(Kind::Number, "16")
        .hidden()
        .user_only(),
    d(
        "oxa.bash.ciPollHint",
        Group::Advanced,
        "",
        "oxa.bash.ciPollHint",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden()
    .user_only(),
    // Longest a bash job read blocks, in seconds (default 300; at least 1).
    d("oxa.bash.waitCap", Group::Advanced, "", "oxa.bash.waitCap", "")
        .key(Kind::Number, "300")
        .hidden(),
    // R1: the first wait after a transient request failure of a pursuing goal; `0` opts out.
    d(
        "oxa.goal.failureBackoffSeconds",
        Group::Advanced,
        "",
        "oxa.goal.failureBackoffSeconds",
        "",
    )
    .key(Kind::Number, "60")
    .hidden(),
    // #2183: how long a follow-up waits behind a pursuing goal that keeps calling tools; `0` =
    // until the run stops.
    d(
        "oxa.goal.followUpMaxWaitSeconds",
        Group::Advanced,
        "",
        "oxa.goal.followUpMaxWaitSeconds",
        "",
    )
    .key(Kind::Number, "300")
    .hidden(),
    d(
        "oxa.goal.idlePause",
        Group::Advanced,
        "",
        "oxa.goal.idlePause",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    // V020-OPTIMIZE #1 / #2: unset = on.
    // The per-prompt environment note (date, time, runtime facts): unset = on.
    // K3: a headless print or JSON run of a persisted session with a pursuing goal takes
    // `oxa send` follow-ups from the session inbox for the run; false = off.
    d(
        "oxa.inbox.headless",
        Group::Advanced,
        "",
        "oxa.inbox.headless",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    // W4: a finished background bash job (not one stopped) queues one follow-up message with its
    // exit code and last lines; true (the default, owner decision); false (opt-out) = the job's
    // output waits for a read. User settings only: a project file cannot enable it.
    d(
        "oxa.jobs.wakeOnFinish",
        Group::Advanced,
        "",
        "oxa.jobs.wakeOnFinish",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    // L1: apply a changed settings file at the next run; on by default, `false` opts out.
    d("oxa.liveSettings", Group::Advanced, "", "oxa.liveSettings", "")
        .key(Kind::Bool, "true")
        .hidden(),
    // B2b: user-only largest image and per-batch image budget; 0 = no limit.
    d(
        "oxa.read.maxImageBytes",
        Group::Advanced,
        "",
        "oxa.read.maxImageBytes",
        "",
    )
    .key(Kind::Number, "20971520")
    .hidden(),
    // Model replies per run before it ends with `turn_limit`; unset or 0 = no limit (#507).
    d(
        "oxa.reportIssue.dailyNew",
        Group::Advanced,
        "",
        "oxa.reportIssue.dailyNew",
        "",
    )
    .key(Kind::Number, "null")
    .hidden(),
    // #1512 S3: the OS sandbox of an interactive (attended) run; user settings only.
    d(
        "oxa.sandbox.interactive",
        Group::Advanced,
        "",
        "oxa.sandbox.interactive",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    // #1512 S3: the OS sandbox of an unattended run (workers, subagents, headless goals).
    d(
        "oxa.sandbox.unattended",
        Group::Advanced,
        "",
        "oxa.sandbox.unattended",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    // Skills another product synced into `~/.agents/skills` (`synced/`); off = skipped.
    // O-146: unset = on except on Flash and below 32K (`SubagentHooks::declared_for`).
    // Local children can run in their own session and prefix instead of forking.
    d(
        "oxa.subagent.isolated",
        Group::Advanced,
        "",
        "oxa.subagent.isolated",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    // Children per parent session (running or queued) and tasks or steps per call; at least 1.
    d(
        "oxa.subagent.maxChildren",
        Group::Advanced,
        "",
        "oxa.subagent.maxChildren",
        "",
    )
    .key(Kind::Number, "8")
    .hidden()
    .user_only(),
    // The repeat guard for identical tool calls (oxa-core `repeat.rs`).
    // The `wait` tool is declared in every session; `false` opts out (read at session start).
    d("oxa.tools.wait", Group::Advanced, "", "oxa.tools.wait", "")
        .key(Kind::Bool, "true")
        .hidden()
        .user_only(),
    // Read from the user layer only. Unset follows the mode: yolo `worker`, auto `broker`,
    // standard `runner`.
    d(
        "oxa.work.workerGithub",
        Group::Advanced,
        "",
        "oxa.work.workerGithub",
        "",
    )
    .key(Kind::Enum(&["runner", "broker", "worker"]), "null")
    .hidden(),
    // W2: the `wait` tool's `until` conditions, answered by hook sets (`Hooks::poll_event`).
    d(
        "oxa.wait.maxSeconds",
        Group::Advanced,
        "",
        "oxa.wait.maxSeconds",
        "",
    )
    .key(Kind::Number, "86400")
    .hidden(),
    d(
        "oxa.wait.pollSeconds",
        Group::Advanced,
        "",
        "oxa.wait.pollSeconds",
        "",
    )
    .key(Kind::Number, "30")
    .hidden(),
    // O-169: `web_search` (oxa-web); declared only with a Brave key and never offline.
    // The worker runner reads this list from owner settings only.
    d(
        "oxa.work.localChecks",
        Group::Advanced,
        "",
        "oxa.work.localChecks",
        "",
    )
    .key(Kind::StringList, "[\"cargo check\"]")
    .hidden(),
    // K5a: the `worker` tool (never in a worker child) and the most live workers a session runs.
    // The `oxa.workers.*` keys are user settings only: a project file cannot start processes,
    // raise a limit or change the stop timing (`Settings::user_bool`, `user_u32`, `user_value`).
    d(
        "oxa.workers.enabled",
        Group::Advanced,
        "",
        "oxa.workers.enabled",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    d("oxa.workers.max", Group::Advanced, "", "oxa.workers.max", "")
        .key(Kind::Number, "4")
        .hidden(),
    // The owner's model choice policy for workers: default and per-kind model lists, and the
    // escalation after repeated failures. User settings only; no model id is built in.
    d(
        "oxa.workers.models",
        Group::Advanced,
        "",
        "oxa.workers.models",
        "",
    )
    .key(Kind::Object, "{}")
    .hidden(),
    // K5b: relaunch the unfinished, dead workers of a session when it is opened again. User
    // settings only: a project file can neither enable nor disable it (`Settings::user_bool`).
    d(
        "oxa.workers.resumeOnStart",
        Group::Advanced,
        "",
        "oxa.workers.resumeOnStart",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    // How long `worker send` waits for the worker to take the message before it reports
    // `not_delivered`; 0 does not wait. User settings only.
    d(
        "oxa.workers.sendAckSeconds",
        Group::Advanced,
        "",
        "oxa.workers.sendAckSeconds",
        "",
    )
    .key(Kind::Number, "10")
    .hidden(),
    // Requests kept back per App resource for required Lead reads (#1413). Unset = no gate.
    d(
        "oxa.lead.github_reserve",
        Group::Advanced,
        "",
        "oxa.lead.github_reserve",
        "",
    )
    .key(Kind::Number, "null")
    .hidden(),
    // K4: seconds a worker gets after SIGINT before its process group gets SIGTERM.
    d(
        "oxa.workers.stopGraceSeconds",
        Group::Advanced,
        "",
        "oxa.workers.stopGraceSeconds",
        "",
    )
    .key(Kind::Number, "10")
    .hidden(),
    // #1541: the shared per-repository build cache of a worker. User settings only.
    d(
        "oxa.workers.sharedCache",
        Group::Advanced,
        "",
        "oxa.workers.sharedCache",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    d(
        "oxa.workers.cacheDir",
        Group::Advanced,
        "",
        "oxa.workers.cacheDir",
        "",
    )
    .key(Kind::String, "null")
    .hidden(),
    d(
        "oxa.workers.cacheWarnGb",
        Group::Advanced,
        "",
        "oxa.workers.cacheWarnGb",
        "",
    )
    .key(Kind::Number, "null")
    .hidden(),
];
