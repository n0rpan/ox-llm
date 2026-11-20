//! W3: the GitHub probes behind `wait` with an `until` object (`Hooks::poll_event`).
//!
//! * `{"kind":"ci","sha":<40 hex>,"workflow":<name>}`: done when the newest run of that workflow
//!   for exactly that head commit has completed; the evidence names its conclusion and, for a
//!   run that did not succeed, the failed jobs. No run yet, or a run that has not completed, is
//!   pending. A run for another commit never counts.
//! * `{"kind":"pr_merged","pr":<n>}`: done when the pull request is merged; closed without a
//!   merge is an error (it can never merge by waiting).
//!
//! SECURITY. This runs `gh` with the owner's login, outside the sandbox, on arguments a model
//! chose. So: `gh` is started with an argument vector (never a shell), in fixed shapes; `sha`
//! is 40 hex characters, `workflow` is `[A-Za-z0-9._ -]{1,100}` starting with an alphanumeric,
//! `pr` is a positive integer; the repository is read from the `origin` remote of the session
//! cwd (github.com only) and never from the call; every call has a deadline and an output cap;
//! stderr is discarded and no token or environment value is printed; `gh` is found with
//! `netguard::trusted_executable_for` like every other host tool, so a project shim is never run.
//! The read probes `pr_view`, `pr_checks`, `run_log_failed` and `issue_view` live in `gh_probe.rs`
//! and follow the same rules.
//! A runner-owned worker session (`OXA_RUNNER_OWNS_GITHUB=1`) runs no `gh` at all, `GH_HOST` is
//! pinned to github.com, and every call is bounded by the wait's own remaining time.
use oxa_proto::{CommandSpec, HookPoint, HookReply, Hooks, OxaError, Tool, Ui};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One `gh` or `git` call must finish within this.
const CALL_DEADLINE: Duration = Duration::from_secs(20);
/// Most bytes of one `gh` answer that are read; more is an error.
pub(crate) const OUTPUT_CAP: usize = 256 * 1024;
/// Most failed job names put in the evidence.
const MAX_FAILED_JOBS: usize = 20;

/// The hook set answering `ci` and `pr_merged`.
pub(crate) struct ProbeHooks {
    cwd: PathBuf,
    home: PathBuf,
    path: Option<OsString>,
    repo: Mutex<Option<String>>,
    /// A runner-owned `oxa work` worker has no GitHub access (AGENTS.md); the runner owns
    /// every GitHub call, including these probes.
    runner_owns_github: bool,
}

impl ProbeHooks {
    pub(crate) fn new(cwd: &Path, home: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            path: std::env::var_os("PATH"),
            repo: Mutex::new(None),
            runner_owns_github: std::env::var_os(oxa_work::RUNNER_GITHUB_ENV)
                .is_some_and(|value| value == "1"),
        }
    }

    #[cfg(test)]
    fn for_test(cwd: &Path, home: &Path, path: &Path, repo: &str) -> Self {
        Self::for_test_role(cwd, home, path, repo, false)
    }

    #[cfg(test)]
    fn for_test_runner_owned(cwd: &Path, home: &Path, path: &Path, repo: &str) -> Self {
        Self::for_test_role(cwd, home, path, repo, true)
    }

    #[cfg(test)]
    pub(crate) fn for_test_paths(cwd: &Path, home: &Path, path: &Path, repo: &str) -> Self {
        Self::for_test_role(cwd, home, path, repo, false)
    }

    #[cfg(test)]
    pub(crate) fn for_test_runner_owned_paths(cwd: &Path, home: &Path, path: &Path, repo: &str) -> Self {
        Self::for_test_role(cwd, home, path, repo, true)
    }

    #[cfg(test)]
    fn for_test_role(cwd: &Path, home: &Path, path: &Path, repo: &str, runner_owns_github: bool) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            path: Some(path.as_os_str().to_os_string()),
            repo: Mutex::new(Some(repo.to_string())),
            runner_owns_github,
        }
    }

    fn exe(&self, program: &str) -> Option<PathBuf> {
        crate::netguard::trusted_executable_for(
            program,
            self.path.as_deref(),
            Some(&self.cwd),
            Some(&self.home),
        )
    }

    fn run(&self, program: &str, args: &[String], budget: Duration) -> Result<String, String> {
        self.run_accepting(program, args, budget, OUTPUT_CAP, &[0])
    }

    /// The home directory as text, for the output scrubber.
    pub(crate) fn home_string(&self) -> String {
        self.home.to_string_lossy().into_owned()
    }

    /// Like `run`, with its own read cap, and the listed exit codes count as an answer.
    pub(crate) fn run_accepting(
        &self,
        program: &str,
        args: &[String],
        budget: Duration,
        cap: usize,
        ok_codes: &[i32],
    ) -> Result<String, String> {
        let exe = self.exe(program).ok_or_else(|| {
            format!("{program} was not found in a trusted PATH directory; wait until ci / pr_merged needs the GitHub CLI (gh)")
        })?;
        let mut command = Command::new(exe);
        command
            .args(args)
            .current_dir(&self.cwd)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1");
        if program == "gh" {
            // The repository comes from a github.com origin; never let an inherited GH_HOST
            // send the call to an enterprise instance with a same-named repository.
            command.env("GH_HOST", "github.com");
        }
        match oxa_work::command_runner(command, Some(cap), budget) {
            None => Err(format!(
                "{program} did not finish within {} seconds, could not be started, or printed more than {cap} bytes",
                budget.as_secs()
            )),
            Some((code, out)) if ok_codes.contains(&code) => Ok(out),
            Some((code, _)) => Err(format!(
                "{program} exited with status {code} (is `gh auth status` logged in, and does the repository exist?)"
            )),
        }
    }

    /// `owner/name` from the `origin` remote of the session cwd.
    pub(crate) fn repo(&self, budget: Duration) -> Result<String, String> {
        let mut cached = self.repo.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = cached.as_ref() {
            return Ok(r.clone());
        }
        let url = self.run(
            "git",
            &["remote".into(), "get-url".into(), "origin".into()],
            budget,
        )?;
        let repo = parse_github_remote(url.trim())
            .ok_or("the origin remote of this directory is not a github.com repository")?;
        *cached = Some(repo.clone());
        Ok(repo)
    }

    pub(crate) fn gh(&self, args: &[&str], budget: Duration) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let out = self.run("gh", &args, budget)?;
        serde_json::from_str(&out).map_err(|_| "gh printed output that is not JSON".to_string())
    }

    fn remaining(deadline: Instant) -> Result<Duration, String> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "the wait deadline expired during the GitHub probe".to_string())
    }

    fn ci(&self, args: &Value, deadline: Instant) -> Result<Option<String>, String> {
        let sha = valid_sha(args["sha"].as_str())?;
        let workflow = valid_workflow(args["workflow"].as_str())?;
        let repo = self.repo(Self::remaining(deadline)?)?;
        let runs = self.gh(
            &[
                "run",
                "list",
                "-R",
                &repo,
                "--workflow",
                &workflow,
                "--commit",
                &sha,
                "--json",
                "databaseId,status,conclusion,headSha",
                "-L",
                "5",
            ],
            Self::remaining(deadline)?,
        )?;
        // Only a run of exactly this head counts; the newest one decides.
        let run = runs
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                r["headSha"]
                    .as_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case(&sha))
            })
            .filter_map(|r| Some((r["databaseId"].as_u64()?, r)))
            .max_by_key(|(id, _)| *id);
        let Some((id, run)) = run else {
            return Ok(None);
        };
        if run["status"].as_str() != Some("completed") {
            return Ok(None);
        }
        let conclusion = run["conclusion"].as_str().unwrap_or("unknown");
        let mut text = format!("{workflow} run {id} at {sha}: {conclusion}");
        if conclusion != "success" {
            let id = id.to_string();
            let jobs = self.gh(
                &["run", "view", &id, "-R", &repo, "--json", "jobs"],
                Self::remaining(deadline)?,
            )?;
            let failed: Vec<String> = jobs["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|j| {
                    !matches!(
                        j["conclusion"].as_str(),
                        None | Some("success" | "skipped" | "neutral" | "")
                    )
                })
                .filter_map(|j| j["name"].as_str())
                .take(MAX_FAILED_JOBS)
                .map(|n| n.chars().take(100).collect())
                .collect();
            if failed.is_empty() {
                text.push_str("; failed jobs: none reported");
            } else {
                text.push_str(&format!("; failed jobs: {}", failed.join(", ")));
            }
        }
        Ok(Some(text))
    }

    fn pr_merged(&self, args: &Value, deadline: Instant) -> Result<Option<String>, String> {
        let pr = args["pr"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("pr must be a positive integer")?;
        let repo = self.repo(Self::remaining(deadline)?)?;
        let pr_arg = pr.to_string();
        let view = self.gh(
            &[
                "pr",
                "view",
                &pr_arg,
                "-R",
                &repo,
                "--json",
                "state,mergedAt,headRefOid",
            ],
            Self::remaining(deadline)?,
        )?;
        let merged = view["state"].as_str() == Some("MERGED")
            || view["mergedAt"].as_str().is_some_and(|m| !m.is_empty());
        if merged {
            let at = view["mergedAt"].as_str().unwrap_or("unknown time");
            let head = view["headRefOid"].as_str().unwrap_or("unknown");
            return Ok(Some(format!("PR #{pr} merged at {at}; head {head}")));
        }
        if view["state"].as_str() == Some("CLOSED") {
            return Err(format!("PR #{pr} was closed without being merged"));
        }
        Ok(None)
    }
}

/// A 40-character hex commit id, lowercased.
fn valid_sha(sha: Option<&str>) -> Result<String, String> {
    match sha {
        Some(s) if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(s.to_ascii_lowercase()),
        _ => Err("sha must be a full 40-character hex commit id".into()),
    }
}

/// `[A-Za-z0-9._ -]{1,100}`, starting with an alphanumeric so it can never read as a flag.
fn valid_workflow(name: Option<&str>) -> Result<String, String> {
    match name {
        Some(n)
            if (1..=100).contains(&n.len())
                && n.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b' ' | b'-')) =>
        {
            Ok(n.to_string())
        }
        _ => Err("workflow must be 1 to 100 characters of letters, digits, '.', '_', ' ' or '-', starting with a letter or digit".into()),
    }
}

/// `owner/name` of a github.com remote URL (ssh, scp-style or https), else `None`.
fn parse_github_remote(url: &str) -> Option<String> {
    let rest = [
        "git@github.com:",
        "https://github.com/",
        "ssh://git@github.com/",
        "git://github.com/",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let ok = |s: &str| {
        (1..=100).contains(&s.len())
            && !s.starts_with('-')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

impl Hooks for ProbeHooks {
    fn name(&self) -> &str {
        "builtin:wait-probes"
    }
    fn fire(&self, _: HookPoint, _: &Value, _: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn wants(&self, _: HookPoint) -> bool {
        false
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, args: &str, ui: &mut dyn Ui) -> Result<(), OxaError> {
        oxa_proto::NoHooks.run_command(name, args, ui)
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn poll_event(
        &self,
        kind: &str,
        args: &Value,
        budget: Duration,
    ) -> Option<Result<Option<String>, String>> {
        if kind != "ci" && kind != "pr_merged" && !crate::gh_probe::is_read_kind(kind) {
            return None;
        }
        if self.runner_owns_github {
            return Some(Err(
                "GitHub access is withheld in a runner-owned worker session: the runner makes \
                 every GitHub call, so use the issue, review and CI facts it supplies"
                    .to_string(),
            ));
        }
        if oxa_proto::offline() {
            return Some(Err(oxa_proto::offline_error(
                "wait until ci / pr_merged (GitHub)",
            )
            .to_string()));
        }
        // One `gh` call must finish inside the wait's own remaining time.
        let budget = budget.min(CALL_DEADLINE);
        if budget.is_zero() {
            return Some(Ok(None));
        }
        let deadline = Instant::now() + budget;
        Some(match kind {
            "ci" => self.ci(args, deadline),
            "pr_merged" => self.pr_merged(args, deadline),
            _ => self.read_probe(kind, args, budget).map(Some),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    /// A fake `gh` on a private PATH directory: answers from `<name>.json` files next to it and
    /// records each argument vector in `calls`.
    struct Fake {
        bin: PathBuf,
        hooks: ProbeHooks,
    }

    impl Fake {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("oxa-wp-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let mk = |p: &Path| {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(p)
                    .unwrap()
            };
            let (bin, work, home) = (root.join("bin"), root.join("work"), root.join("home"));
            for d in [&bin, &work, &home] {
                mk(d);
            }
            let script = "#!/bin/sh\ndir=\"$(dirname \"$0\")\"\necho \"$@\" >> \"$dir/calls\"\necho \"host=$GH_HOST\" >> \"$dir/hosts\"\n[ ! -f \"$dir/delay\" ] || sleep \"$(cat \"$dir/delay\")\"\ncase \"$1 $2\" in\n  \"run list\") cat \"$dir/run_list.json\" ;;\n  \"run view\") cat \"$dir/run_view.json\" ;;\n  \"pr view\") cat \"$dir/pr_view.json\" ;;\n  *) exit 2 ;;\nesac\n";
            let gh = bin.join("gh");
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let hooks = ProbeHooks::for_test(&work, &home, &bin, "acme/widgets");
            Self { bin, hooks }
        }

        fn answer(&self, name: &str, json: &str) {
            std::fs::write(self.bin.join(format!("{name}.json")), json).unwrap();
        }

        fn calls(&self) -> Option<String> {
            std::fs::read_to_string(self.bin.join("calls")).ok()
        }

        fn hosts(&self) -> Option<String> {
            std::fs::read_to_string(self.bin.join("hosts")).ok()
        }

        fn poll(&self, args: Value) -> Result<Option<String>, String> {
            self.poll_within(args, Duration::from_secs(30))
        }

        fn poll_within(&self, args: Value, budget: Duration) -> Result<Option<String>, String> {
            let kind = args["kind"].as_str().unwrap().to_string();
            self.hooks
                .poll_event(&kind, &args, budget)
                .expect("kind is answered")
        }
    }

    fn ci_args(sha: &str) -> Value {
        json!({"kind": "ci", "sha": sha, "workflow": "lane"})
    }

    #[test]
    fn ci_probe_pending_then_success_at_exact_head() {
        let f = Fake::new("pending");
        f.answer("run_list", "[]");
        assert_eq!(f.poll(ci_args(SHA)), Ok(None));
        f.answer(
            "run_list",
            &format!(r#"[{{"databaseId":7,"status":"in_progress","conclusion":"","headSha":"{SHA}"}}]"#),
        );
        assert_eq!(f.poll(ci_args(SHA)), Ok(None));
        f.answer(
            "run_list",
            &format!(r#"[{{"databaseId":7,"status":"completed","conclusion":"success","headSha":"{SHA}"}}]"#),
        );
        let done = f.poll(ci_args(SHA)).unwrap().expect("done");
        assert_eq!(done, format!("lane run 7 at {SHA}: success"));
        let calls = f.calls().unwrap();
        assert!(calls.contains(&format!(
            "run list -R acme/widgets --workflow lane --commit {SHA} --json databaseId,status,conclusion,headSha -L 5"
        )));
        assert!(
            !calls.contains("run view"),
            "a successful run needs no job lookup"
        );
    }

    #[test]
    fn ci_probe_ignores_run_for_other_sha() {
        let f = Fake::new("othersha");
        f.answer(
            "run_list",
            &format!(
                r#"[{{"databaseId":9,"status":"completed","conclusion":"success","headSha":"{OTHER}"}}]"#
            ),
        );
        assert_eq!(f.poll(ci_args(SHA)), Ok(None));
    }

    #[test]
    fn ci_probe_failure_lists_failed_jobs() {
        let f = Fake::new("failure");
        f.answer(
            "run_list",
            &format!(
                r#"[{{"databaseId":11,"status":"completed","conclusion":"failure","headSha":"{SHA}"}}]"#
            ),
        );
        f.answer(
            "run_view",
            r#"{"jobs":[{"name":"build","conclusion":"success"},{"name":"test (ubuntu)","conclusion":"failure"},{"name":"lint","conclusion":"failure"},{"name":"docs","conclusion":"skipped"}]}"#,
        );
        let done = f.poll(ci_args(SHA)).unwrap().expect("done");
        assert_eq!(
            done,
            format!("lane run 11 at {SHA}: failure; failed jobs: test (ubuntu), lint")
        );
        assert!(f
            .calls()
            .unwrap()
            .contains("run view 11 -R acme/widgets --json jobs"));
    }

    #[test]
    fn ci_probe_shares_one_deadline_across_subprocesses() {
        let f = Fake::new("shared-deadline");
        f.answer(
            "run_list",
            &format!(
                r#"[{{"databaseId":11,"status":"completed","conclusion":"failure","headSha":"{SHA}"}}]"#
            ),
        );
        f.answer("run_view", r#"{"jobs":[]}"#);
        std::fs::write(f.bin.join("delay"), "0.25").unwrap();

        let started = Instant::now();
        assert!(f.poll_within(ci_args(SHA), Duration::from_millis(400)).is_err());
        assert!(
            started.elapsed() < Duration::from_millis(650),
            "each subprocess must not receive a fresh 400ms budget"
        );
    }

    #[test]
    fn probe_rejects_malformed_sha_and_workflow() {
        let f = Fake::new("invalid");
        f.answer("run_list", "[]");
        for sha in [
            "",
            "abc",
            "0123456789abcdef0123456789abcdef0123456z",
            &format!("{SHA}0"),
            "--help",
        ] {
            assert!(f.poll(ci_args(sha)).is_err(), "sha {sha:?}");
        }
        for wf in ["", "-x", "a;b", "a b$(id)", "a/b", &"w".repeat(101)] {
            let args = json!({"kind": "ci", "sha": SHA, "workflow": wf});
            assert!(f.poll(args).is_err(), "workflow {wf:?}");
        }
        assert!(f.poll(json!({"kind": "ci", "sha": SHA})).is_err());
        for pr in [json!(0), json!(-3), json!("12"), json!(1.5), Value::Null] {
            assert!(f.poll(json!({"kind": "pr_merged", "pr": pr})).is_err(), "pr {pr}");
        }
        assert_eq!(f.calls(), None, "gh must not run for refused input");
    }

    #[test]
    fn probe_gh_missing_is_an_error() {
        let f = Fake::new("missing");
        let empty = f.bin.parent().unwrap().join("empty");
        std::fs::DirBuilder::new().mode(0o755).create(&empty).unwrap();
        let cwd = f.bin.parent().unwrap().join("work");
        let home = f.bin.parent().unwrap().join("home");
        let hooks = ProbeHooks::for_test(&cwd, &home, &empty, "acme/widgets");
        let e = hooks
            .poll_event("ci", &ci_args(SHA), Duration::from_secs(30))
            .unwrap()
            .unwrap_err();
        assert!(e.contains("gh"), "{e}");
    }

    #[test]
    fn pr_merged_probe_pending_merged_and_closed() {
        let f = Fake::new("pr");
        let pr = json!({"kind": "pr_merged", "pr": 42});
        f.answer(
            "pr_view",
            &format!(r#"{{"state":"OPEN","mergedAt":null,"headRefOid":"{SHA}"}}"#),
        );
        assert_eq!(f.poll(pr.clone()), Ok(None));
        f.answer(
            "pr_view",
            &format!(r#"{{"state":"MERGED","mergedAt":"2026-11-18T10:00:00Z","headRefOid":"{SHA}"}}"#),
        );
        let done = f.poll(pr.clone()).unwrap().expect("done");
        assert!(
            done.starts_with("PR #42 merged at 2026-11-18T10:00:00Z"),
            "{done}"
        );
        assert!(f
            .calls()
            .unwrap()
            .contains("pr view 42 -R acme/widgets --json state,mergedAt,headRefOid"));
        f.answer(
            "pr_view",
            &format!(r#"{{"state":"CLOSED","mergedAt":null,"headRefOid":"{SHA}"}}"#),
        );
        assert!(f.poll(pr).unwrap_err().contains("closed without being merged"));
    }

    #[test]
    fn probe_leaves_other_kinds_to_other_sets() {
        let f = Fake::new("kinds");
        assert!(f
            .hooks
            .poll_event("deploy", &json!({"kind": "deploy"}), Duration::from_secs(30))
            .is_none());
    }

    #[test]
    fn runner_owned_worker_sessions_never_run_gh() {
        let f = Fake::new("runner");
        f.answer("run_list", "[]");
        f.answer(
            "pr_view",
            &format!(r#"{{"state":"OPEN","mergedAt":null,"headRefOid":"{SHA}"}}"#),
        );
        let owned = ProbeHooks::for_test_runner_owned(
            &f.hooks.cwd,
            &f.hooks.home,
            Path::new(f.hooks.path.as_deref().unwrap()),
            "acme/widgets",
        );
        for args in [ci_args(SHA), json!({"kind": "pr_merged", "pr": 7})] {
            let kind = args["kind"].as_str().unwrap().to_string();
            let error = owned
                .poll_event(&kind, &args, Duration::from_secs(30))
                .unwrap()
                .unwrap_err();
            assert!(error.contains("runner-owned"), "{error}");
        }
        assert_eq!(f.calls(), None, "no gh call runs in a runner-owned session");
    }

    #[test]
    fn a_wait_with_no_time_left_runs_no_probe() {
        let f = Fake::new("zerobudget");
        f.answer("run_list", "[]");
        assert_eq!(
            f.poll_within(ci_args(SHA), Duration::ZERO),
            Ok(None),
            "a zero budget is pending, not a shortened call"
        );
        assert_eq!(f.calls(), None);
    }

    #[test]
    fn gh_calls_are_pinned_to_github_com() {
        let f = Fake::new("host");
        f.answer("run_list", "[]");
        assert_eq!(f.poll(ci_args(SHA)), Ok(None));
        let hosts = f.hosts().expect("the fake gh records GH_HOST");
        assert!(hosts.contains("host=github.com"), "{hosts}");
        assert!(!hosts.contains("host=enterprise"), "{hosts}");
    }

    #[test]
    fn github_remote_forms_parse_and_others_are_refused() {
        for url in [
            "git@github.com:oxstack/ox-llm.git",
            "https://github.com/oxstack/ox-llm.git",
            "https://github.com/oxstack/ox-llm/",
            "ssh://git@github.com/oxstack/ox-llm",
        ] {
            assert_eq!(
                parse_github_remote(url).as_deref(),
                Some("oxstack/ox-llm"),
                "{url}"
            );
        }
        for url in [
            "",
            "https://evil.example/oxstack/ox-llm",
            "git@github.com:a/b/c",
            "git@github.com:-x/y",
            "/srv/git/repo",
        ] {
            assert_eq!(parse_github_remote(url), None, "{url}");
        }
    }
}
