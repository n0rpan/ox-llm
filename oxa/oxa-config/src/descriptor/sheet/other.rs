//! The `Other` rows of the settings page, in page order (issue #2042, R3): the docs view of
//! the descriptor table, rendered into `oxa/docs/pages/settings.md` by
//! `tools/ci/settings_table.py`. Edit a row here and rerun the tool.

use super::{row, PageRow};

/// The section heading shown on the page.
pub(super) const TITLE: &str = "Other";

#[rustfmt::skip]
pub(super) const ROWS: &[PageRow] = &[
    row("externalEditor", "string", "`null`", "editor for Ctrl+G; wins over `VISUAL` and `EDITOR`"),
    row("oxa.reportIssue.repo", "string", "`null` (reports go to `oxstack/ox-llm`)", "repository for `oxa report-issue submit`, as `OWNER/NAME`; a value that is not `OWNER/NAME` (for example `off`) turns reporting off"),
    row("oxa.reportIssue.dailyNew", "number", "`null`", "owner cap on new issues per worker per UTC day for semantic dedupe. Unset means no check. Over the cap the post preview warns and records it; the owner's yes still posts"),
    row("oxa.reportIssue.semanticDedupe", "bool", "`false`", "verify dogfood reports against current main and search open and recently closed issues before posting. Matching reports become comments, and the one preview question covers the post. New issues are limited to one per task; the per-worker daily count is `oxa.reportIssue.dailyNew`"),
    row("oxa.builtinSkills", "bool", "`true`", "include the skills built into oxa"),
    row("oxa.share.remoteModels", "string list", "`[]`", "user-only allowlist of metered cloud aliases eligible for explicit LAN sharing; personal subscriptions are excluded"),
    row("oxa.lead.github_reserve", "number", "`null`", "user settings only. Requests kept back per GitHub App resource (core and graphql) for required Lead reads: `oxa work board --app-quota-shadow` runs its optional reads (lead panels, queued-run pages, test reports) only while the App window stays above this reserve, and records each deferral in the sample (`quota_deferred`) instead of failing. Unset means no gate. Model and subscription limits are not touched. Needs `--app-quota-shadow`; `--app-quota-reserve N` overrides it for one run"),
    row("oxa.skills.includeSynced", "bool", "`false`", "also list the skills another product synced into `~/.agents/skills` (a `synced/` folder, or a folder whose `manifest.json` lists `skillId`s); they are written for that product's tools, so oxa skips them by default"),
    row("oxa.workers.enabled", "bool", "`true`", "user settings only (a project file cannot change it). The `worker` tool (start, list, stop background workers, each in its own git worktree); `false` removes it. Never offered inside a worker"),
    row("oxa.workers.max", "number", "`4`", "user settings only (a project file cannot change it). Most live workers one session may run; a `start` past this limit is refused"),
    row("oxa.workers.models", "object", "`{}`", "user settings only (a project file cannot change it). Which model a `worker start` without `model` gets: `{\"default\": [ids], \"kinds\": {\"<kind>\": [ids]}, \"escalateAfter\": 2, \"escalateTo\": [ids]}`, every field optional. The `kind` the caller passes picks the row (else `default`); within a list the entry this session used least is taken, so two ids alternate. After `escalateAfter` failed workers on one job (same `job` key), the next one gets the first `escalateTo` id that has not failed yet. Kind names are lowercase letters, digits and dashes (at most 32). No ids are built in: unset means the session model. A model id the live catalog does not know only warns. Example: `{\"kinds\": {\"review\": [\"provider/fast-a\", \"provider/fast-b\"]}, \"escalateTo\": [\"provider/strong\"]}`"),
    row("oxa.workers.repository", "string", "unset", "user settings only. Canonical absolute path of the explicitly trusted primary Git checkout used by `worker start` in a trusted live Lead session outside Git"),
    row("oxa.workers.resumeOnStart", "bool", "`true`", "user settings only (a project file cannot change it, either way). When a session is opened again, relaunch each of its workers that did not finish and whose process is gone, with `--continue=<id>` and no goal; one notice per worker. `false` leaves them as they are"),
    row("oxa.workers.sendAckSeconds", "number (seconds)", "`10`", "user settings only (a project file cannot change it). How long the `worker` tool's `send` waits for the worker to take the message before it answers `receipt <id>: not_delivered`; `0` does not wait and answers `queued`. `send` with `retry` waits again for the same message and with `withdraw` removes it"),
    row("oxa.workers.stopGraceSeconds", "number (seconds)", "`10`", "user settings only (a project file cannot change it). How long a stopped worker gets after SIGINT before its whole process group gets SIGTERM; SIGKILL follows 2 seconds later if the group is still alive"),
    row("oxa.workers.sharedCache", "bool", "`true`", "user settings only (a project file cannot change it). A worker derives one Cargo target directory per main repository, `<cache dir>/<hash>/cargo-target`, shared by every worktree of it: it is the only extra writable path of the worker sandbox and `CARGO_TARGET_DIR` points at it, unless you set `CARGO_TARGET_DIR` yourself. `false` turns it off"),
    row("oxa.workers.cacheDir", "path", "unset (`<XDG cache home>/oxa/workers`)", "user settings only. The root of the worker build caches; `~/` is expanded"),
    row("oxa.workers.cacheWarnGb", "number (GB)", "unset", "user settings only. When set and a repository's worker build cache is larger, the Workers view warns and names the directory to delete. Never a limit"),
];
