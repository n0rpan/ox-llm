//! Settings that exist in the schema but have no row in the menus: lists and objects edited as
//! raw JSON, pi keys oxa accepts, and keys the menus leave out. A hidden row declares the key, its
//! kind and its default; `descriptors()` and the other lookups skip it, `schema()` reads it.

use super::{d, Descriptor, Group};
use crate::schema::Kind;

/// The hidden rows, sorted by key.
pub(super) const ENTRIES: &[Descriptor] = &[
    d("defaultTools", Group::Advanced, "", "defaultTools", "")
        .key(
            Kind::StringList,
            "[\"read\",\"bash\",\"edit\",\"write\",\"grep\",\"find\",\"ls\"]",
        )
        .hidden(),
    d("done", Group::Advanced, "", "done", "")
        .key(Kind::StringList, "[]")
        .hidden(),
    d("extensions", Group::Advanced, "", "extensions", "")
        .key(Kind::StringList, "[]")
        .hidden(),
    d("hooks", Group::Advanced, "", "hooks", "")
        .key(Kind::Object, "{}")
        .hidden(),
    // F5: seconds; unset = 120; 0 = none.
    d(
        "oxa.bash.defaultTimeout",
        Group::Advanced,
        "",
        "oxa.bash.defaultTimeout",
        "",
    )
    .key(Kind::Number, "null")
    .hidden(),
    // Background jobs that may run at once (`bash {background: true}`); at least 1.
    d("oxa.bash.maxJobs", Group::Advanced, "", "oxa.bash.maxJobs", "")
        .key(Kind::Number, "16")
        .hidden(),
    d("oxa.builtinSkills", Group::Advanced, "", "oxa.builtinSkills", "")
        .key(Kind::Bool, "true")
        .hidden(),
    d("oxa.cloud", Group::Advanced, "", "oxa.cloud", "")
        .key(Kind::Object, "{}")
        .hidden(),
    // V020-OPTIMIZE #1 / #2: unset = on.
    d("oxa.diet.output", Group::Advanced, "", "oxa.diet.output", "")
        .key(Kind::Bool, "null")
        .hidden(),
    d("oxa.diet.tools", Group::Advanced, "", "oxa.diet.tools", "")
        .key(Kind::Bool, "null")
        .hidden(),
    // The per-prompt environment note (date, time, runtime facts): unset = on.
    d("oxa.envNote", Group::Advanced, "", "oxa.envNote", "")
        .key(Kind::Bool, "null")
        .hidden(),
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
    // L1: apply a changed settings file at the next run; on by default, `false` opts out.
    d("oxa.liveSettings", Group::Advanced, "", "oxa.liveSettings", "")
        .key(Kind::Bool, "true")
        .hidden(),
    // Model replies per run before it ends with `turn_limit`; unset or 0 = no limit (#507).
    d("oxa.maxTurns", Group::Advanced, "", "oxa.maxTurns", "")
        .key(Kind::Number, "null")
        .hidden(),
    d(
        "oxa.reportIssue.dailyNew",
        Group::Advanced,
        "",
        "oxa.reportIssue.dailyNew",
        "",
    )
    .key(Kind::Number, "null")
    .hidden(),
    d(
        "oxa.reportIssue.repo",
        Group::Advanced,
        "",
        "oxa.reportIssue.repo",
        "",
    )
    .key(Kind::String, "null")
    .hidden(),
    d(
        "oxa.reportIssue.semanticDedupe",
        Group::Advanced,
        "",
        "oxa.reportIssue.semanticDedupe",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    // Skills another product synced into `~/.agents/skills` (`synced/`); off = skipped.
    d(
        "oxa.skills.includeSynced",
        Group::Advanced,
        "",
        "oxa.skills.includeSynced",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    // O-146: unset = on except on Flash and below 32K (`SubagentHooks::declared_for`).
    d(
        "oxa.subagent.enabled",
        Group::Advanced,
        "",
        "oxa.subagent.enabled",
        "",
    )
    .key(Kind::Bool, "null")
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
    .hidden(),
    d(
        "oxa.subagent.remoteConcurrency",
        Group::Advanced,
        "",
        "oxa.subagent.remoteConcurrency",
        "",
    )
    .key(Kind::Number, "4")
    .hidden(),
    d(
        "oxa.subagent.resultCap",
        Group::Advanced,
        "",
        "oxa.subagent.resultCap",
        "",
    )
    .key(Kind::Number, "6144")
    .hidden(),
    d(
        "oxa.subagent.smallContext",
        Group::Advanced,
        "",
        "oxa.subagent.smallContext",
        "",
    )
    .key(Kind::Bool, "false")
    .hidden(),
    d(
        "oxa.subagent.softTokens",
        Group::Advanced,
        "",
        "oxa.subagent.softTokens",
        "",
    )
    .key(Kind::Number, "24000")
    .hidden(),
    d("oxa.toolOutputCap", Group::Advanced, "", "oxa.toolOutputCap", "")
        .key(Kind::Number, "16384")
        .hidden(),
    d("oxa.toolRepair", Group::Advanced, "", "oxa.toolRepair", "")
        .key(Kind::Bool, "true")
        .hidden(),
    // The repeat guard for identical tool calls (oxa-core `repeat.rs`).
    d(
        "oxa.toolRepeatGuard",
        Group::Advanced,
        "",
        "oxa.toolRepeatGuard",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    // W2: the `wait` tool's `until` conditions, answered by hook sets (`Hooks::poll_event`).
    d(
        "oxa.wait.maxSeconds",
        Group::Advanced,
        "",
        "oxa.wait.maxSeconds",
        "",
    )
    .key(Kind::Number, "3600")
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
    d(
        "oxa.webSearch.enabled",
        Group::Advanced,
        "",
        "oxa.webSearch.enabled",
        "",
    )
    .key(Kind::Bool, "true")
    .hidden(),
    d(
        "oxa.webSearch.maxTokens",
        Group::Advanced,
        "",
        "oxa.webSearch.maxTokens",
        "",
    )
    .key(Kind::Number, "2048")
    .hidden(),
    d(
        "oxa.webSearch.provider",
        Group::Advanced,
        "",
        "oxa.webSearch.provider",
        "",
    )
    .key(Kind::Enum(&["brave", "none"]), "\"brave\"")
    .hidden(),
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
    d("prompts", Group::Advanced, "", "prompts", "")
        .key(Kind::StringList, "[]")
        .hidden(),
    d(
        "shellCommandPrefix",
        Group::Advanced,
        "",
        "shellCommandPrefix",
        "",
    )
    .key(Kind::String, "null")
    .hidden(),
    d("shellPath", Group::Advanced, "", "shellPath", "")
        .key(Kind::String, "null")
        .hidden(),
    d("skills", Group::Advanced, "", "skills", "")
        .key(Kind::StringList, "[]")
        .hidden(),
];
