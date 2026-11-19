//! RP5a: host-side GitHub read probes for sandboxed sessions.
//!
//! A sandboxed bash has no GitHub credential (the token store is masked, the network is
//! guarded), so a session could not read a PR, its checks, a failed job log or an issue. These
//! probes run on the host, in the fixed-argv style of the `ci` / `pr_merged` wait probes
//! (`wait_probe.rs`), and answer at once through `wait` with an `until` object:
//!
//! * `{"kind":"pr_view","pr":<n>}`: state, head, base, review decision, mergeability, body.
//! * `{"kind":"pr_checks","pr":<n>}`: the check counts and every check that is not passing.
//! * `{"kind":"run_log_failed","run":<id>}`: the tail of the failed-step log of a workflow run.
//! * `{"kind":"issue_view","issue":<n>}`: title, state, labels and body.
//!
//! SECURITY. Read only, no GitHub write. The numbers are positive integers; the repository is
//! the `origin` remote of the session cwd (github.com only) and never comes from the call;
//! `gh` is started with an argument vector (never a shell), found with
//! `netguard::trusted_executable_for` (a project shim never runs), pinned to github.com, with a
//! deadline and a read cap. The job log is cut to its tail and scrubbed of credentials before
//! the model sees it. A runner-owned worker and offline mode refuse, as for the other probes.
use crate::report_cmd::{scrub, ScrubCtx};
use crate::wait_probe::ProbeHooks;
use serde_json::Value;
use std::time::Duration;

/// Most bytes of a job log read from `gh`; a bigger log is an error, never a partial read.
const LOG_READ_CAP: usize = 8 * 1024 * 1024;
/// Most bytes of the log tail handed to the model.
const LOG_TAIL_BYTES: usize = 24 * 1024;
/// Most characters of a PR or issue body handed to the model.
const BODY_CHARS: usize = 8000;
/// Most non-passing checks listed.
const MAX_CHECKS_LISTED: usize = 50;

/// `true` for the kinds this module answers.
pub(crate) fn is_read_kind(kind: &str) -> bool {
    matches!(kind, "pr_view" | "pr_checks" | "run_log_failed" | "issue_view")
}

/// A positive integer under `key`.
fn number(args: &Value, key: &str) -> Result<u64, String> {
    args[key]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{key} must be a positive integer"))
}

fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push_str(" [cut]");
    cut
}

/// The last `max` bytes of `log` starting on a line boundary, with a note when text was dropped.
fn tail(log: &str, max: usize) -> String {
    if log.len() <= max {
        return log.to_string();
    }
    let mut start = log.len() - max;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    // Drop the partial first line so a secret cut in half never starts the tail.
    let rest = &log[start..];
    let rest = rest.split_once('\n').map_or(rest, |(_, after)| after);
    format!(
        "[log cut: the last {} bytes of {}]\n{rest}",
        rest.len(),
        log.len()
    )
}

impl ProbeHooks {
    /// Answer one read probe; the evidence text is final (a read has no pending state).
    pub(crate) fn read_probe(&self, kind: &str, args: &Value, budget: Duration) -> Result<String, String> {
        match kind {
            "pr_view" => self.pr_view(number(args, "pr")?, budget),
            "pr_checks" => self.pr_checks(number(args, "pr")?, budget),
            "run_log_failed" => self.run_log_failed(number(args, "run")?, budget),
            "issue_view" => self.issue_view(number(args, "issue")?, budget),
            _ => Err(format!("unknown GitHub read probe: {kind}")),
        }
    }

    /// The scrub context for evidence that leaves oxa as tool output.
    fn scrub_ctx(&self) -> ScrubCtx {
        ScrubCtx {
            home: Some(self.home_string()),
            user: std::env::var("USER")
                .ok()
                .or_else(|| std::env::var("LOGNAME").ok()),
            host: None,
        }
    }

    /// Scrub, then cut: a credential a cut could separate from its marker stays recognisable.
    fn scrubbed(&self, text: &str, max: usize) -> String {
        cut_chars(&scrub(text, &self.scrub_ctx()), max)
    }

    fn pr_view(&self, pr: u64, budget: Duration) -> Result<String, String> {
        let repo = self.repo(budget)?;
        let n = pr.to_string();
        let v = self.gh(
            &[
                "pr",
                "view",
                &n,
                "-R",
                &repo,
                "--json",
                "number,title,state,isDraft,headRefName,headRefOid,baseRefName,mergeable,reviewDecision,url,body",
            ],
            budget,
        )?;
        let s = |k: &str| v[k].as_str().unwrap_or("unknown").to_string();
        Ok(format!(
            "PR #{pr} {}: {}\nstate {}{}; head {} ({}); base {}; mergeable {}; review {}\n{}\n\n{}",
            s("url"),
            self.scrubbed(&s("title"), BODY_CHARS),
            s("state"),
            if v["isDraft"].as_bool() == Some(true) {
                " (draft)"
            } else {
                ""
            },
            self.scrubbed(&s("headRefName"), 200),
            s("headRefOid"),
            self.scrubbed(&s("baseRefName"), 200),
            s("mergeable"),
            v["reviewDecision"]
                .as_str()
                .filter(|d| !d.is_empty())
                .unwrap_or("none"),
            "--- body ---",
            // A pasted credential in a PR would otherwise reach the model as tool output.
            self.scrubbed(v["body"].as_str().unwrap_or(""), BODY_CHARS),
        ))
    }

    fn pr_checks(&self, pr: u64, budget: Duration) -> Result<String, String> {
        let repo = self.repo(budget)?;
        let n = pr.to_string();
        // `gh pr checks` exits 1 for a failing and 8 for a pending check but still prints the JSON.
        let out = self.run_accepting(
            "gh",
            &[
                "pr".into(),
                "checks".into(),
                n,
                "-R".into(),
                repo,
                "--json".into(),
                "name,state,bucket,workflow".into(),
            ],
            budget,
            crate::wait_probe::OUTPUT_CAP,
            &[0, 1, 8],
        )?;
        let checks: Value =
            serde_json::from_str(&out).map_err(|_| "gh printed output that is not JSON".to_string())?;
        let all = checks.as_array().cloned().unwrap_or_default();
        let count = |bucket: &str| {
            all.iter()
                .filter(|c| c["bucket"].as_str() == Some(bucket))
                .count()
        };
        let mut text = format!(
            "PR #{pr}: {} checks; pass {}, fail {}, pending {}, skipping {}, cancel {}",
            all.len(),
            count("pass"),
            count("fail"),
            count("pending"),
            count("skipping"),
            count("cancel"),
        );
        let open: Vec<&Value> = all
            .iter()
            .filter(|c| !matches!(c["bucket"].as_str(), Some("pass" | "skipping")))
            .collect();
        for c in open.iter().take(MAX_CHECKS_LISTED) {
            // A workflow or check name is repository-controlled text: scrub it like a body.
            text.push_str(&format!(
                "\n{}: {} / {}",
                c["bucket"].as_str().unwrap_or("unknown"),
                self.scrubbed(
                    c["workflow"].as_str().filter(|w| !w.is_empty()).unwrap_or("-"),
                    100
                ),
                self.scrubbed(c["name"].as_str().unwrap_or("?"), 100),
            ));
        }
        if open.len() > MAX_CHECKS_LISTED {
            text.push_str(&format!(
                "\n... {} more not listed",
                open.len() - MAX_CHECKS_LISTED
            ));
        }
        Ok(text)
    }

    fn run_log_failed(&self, run: u64, budget: Duration) -> Result<String, String> {
        let repo = self.repo(budget)?;
        let log = self.run_accepting(
            "gh",
            &[
                "run".into(),
                "view".into(),
                run.to_string(),
                "-R".into(),
                repo,
                "--log-failed".into(),
            ],
            budget,
            LOG_READ_CAP,
            &[0],
        )?;
        if log.trim().is_empty() {
            return Ok(format!(
                "run {run}: no failed-step log (the run has no failed step yet)"
            ));
        }
        // Scrub the whole bounded log, then cut: scrubbing the tail alone would lose the
        // `BEGIN ... PRIVATE KEY` marker a cut can remove, leaving the key material behind for
        // a matcher that needs the envelope.
        let ctx = ScrubCtx {
            home: Some(self.home_string()),
            user: std::env::var("USER")
                .ok()
                .or_else(|| std::env::var("LOGNAME").ok()),
            host: None,
        };
        Ok(format!(
            "run {run} failed steps:\n{}",
            tail(&scrub(&log, &ctx), LOG_TAIL_BYTES)
        ))
    }

    fn issue_view(&self, issue: u64, budget: Duration) -> Result<String, String> {
        let repo = self.repo(budget)?;
        let n = issue.to_string();
        let v = self.gh(
            &[
                "issue",
                "view",
                &n,
                "-R",
                &repo,
                "--json",
                "number,title,state,labels,url,body",
            ],
            budget,
        )?;
        let labels: Vec<&str> = v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str())
            .take(30)
            .collect();
        Ok(format!(
            "Issue #{issue} {}: {}\nstate {}; labels {}\n--- body ---\n{}",
            v["url"].as_str().unwrap_or("unknown"),
            self.scrubbed(v["title"].as_str().unwrap_or("unknown"), BODY_CHARS),
            v["state"].as_str().unwrap_or("unknown"),
            if labels.is_empty() {
                "none".to_string()
            } else {
                // A label is repository-controlled text like a body.
                self.scrubbed(&labels.join(", "), BODY_CHARS)
            },
            self.scrubbed(v["body"].as_str().unwrap_or(""), BODY_CHARS),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::Hooks;
    use serde_json::json;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;

    /// A fake `gh` that answers from files next to it and records each argument vector.
    struct Fake {
        bin: PathBuf,
        hooks: ProbeHooks,
    }

    impl Fake {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("oxa-gp-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for d in ["bin", "work", "home"] {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(root.join(d))
                    .unwrap();
            }
            let script = "#!/bin/sh\ndir=\"$(dirname \"$0\")\"\necho \"$@\" >> \"$dir/calls\"\ncase \"$1 $2\" in\n  \"pr view\") cat \"$dir/pr_view.json\" ;;\n  \"pr checks\") cat \"$dir/pr_checks.json\"; exit 8 ;;\n  \"run view\") cat \"$dir/run_log\" ;;\n  \"issue view\") cat \"$dir/issue_view.json\" ;;\n  *) exit 2 ;;\nesac\n";
            let gh = root.join("bin").join("gh");
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let hooks = ProbeHooks::for_test_paths(
                &root.join("work"),
                &root.join("home"),
                &root.join("bin"),
                "acme/widgets",
            );
            Self {
                bin: root.join("bin"),
                hooks,
            }
        }

        fn answer(&self, name: &str, text: &str) {
            std::fs::write(self.bin.join(name), text).unwrap();
        }

        fn calls(&self) -> Option<String> {
            std::fs::read_to_string(self.bin.join("calls")).ok()
        }

        fn poll(&self, args: Value) -> Result<Option<String>, String> {
            let kind = args["kind"].as_str().unwrap().to_string();
            self.hooks
                .poll_event(&kind, &args, Duration::from_secs(30))
                .expect("kind is answered")
        }
    }

    /// A private-key envelope that straddles the 24 KB cutoff: scrubbing after the cut would lose
    /// the `BEGIN` marker and leave the wrapped key lines in the tail.
    #[test]
    fn run_log_failed_scrubs_a_key_envelope_across_the_cutoff() {
        let f = Fake::new("log");
        let label = "PRIVATE KEY";
        let mut log = String::new();
        // Lines of roughly 40 bytes: 700 of them put the cutoff inside the envelope below.
        for i in 0..700 {
            log.push_str(&format!("step line {i} ........................\n"));
        }
        log.push_str(&format!("-----BEGIN {label}-----\n"));
        // Wrapped base64 lines shorter than the 32-character fallback boundary.
        for _ in 0..40 {
            log.push_str("c2hvcnQtc2VjcmV0LWxpbmU\n");
        }
        log.push_str(&format!("-----END {label}-----\n"));
        log.push_str("step line 999 ........................\n");
        f.answer("run_log", &log);
        let out = f
            .poll(json!({"kind": "run_log_failed", "run": 99}))
            .unwrap()
            .expect("answered at once");
        assert!(out.contains("[log cut"), "the log is cut to its tail");
        assert!(
            !out.contains("c2hvcnQtc2VjcmV0LWxpbmU"),
            "key material must not survive the cut: {out}"
        );
        assert!(!out.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn run_log_failed_is_capped_and_redacted() {
        let f = Fake::new("log-envelope");
        let token = format!("ghp_{}", "A1b2C3d4E5".repeat(4));
        let mut log = String::new();
        for i in 0..4000 {
            log.push_str(&format!("step line {i} ........................\n"));
        }
        log.push_str(&format!("error: auth failed with token {token}\n"));
        // Exercise the PEM redactor with a synthetic envelope and no key material.
        let key_label = "PRIVATE KEY";
        log.push_str(&format!(
            "-----BEGIN {key_label}-----\nsynthetic fixture\n-----END {key_label}-----\n"
        ));
        f.answer("run_log", &log);
        let out = f
            .poll(json!({"kind": "run_log_failed", "run": 99}))
            .unwrap()
            .expect("answered at once");
        assert!(!out.contains(&token), "token must be redacted");
        assert!(!out.contains("BEGIN PRIVATE KEY"), "private key must be redacted");
        assert!(out.contains("<secret>"));
        assert!(out.contains("[log cut"), "the log is cut to its tail");
        assert!(
            out.len() <= LOG_TAIL_BYTES + 400,
            "tail stays capped, got {}",
            out.len()
        );
        assert!(!out.contains("step line 0 "), "the head of the log is dropped");
        assert!(f
            .calls()
            .unwrap()
            .contains("run view 99 -R acme/widgets --log-failed"));
    }

    #[test]
    fn probe_repo_comes_from_origin_only() {
        let f = Fake::new("repo");
        f.answer(
            "issue_view.json",
            r#"{"number":5,"title":"t","state":"OPEN","labels":[{"name":"bug"}],"url":"u","body":"b"}"#,
        );
        // A repository named in the call is ignored: the repo is the origin remote.
        let out = f
            .poll(json!({"kind": "issue_view", "issue": 5, "repo": "evil/other", "R": "evil/other"}))
            .unwrap()
            .unwrap();
        assert!(out.contains("Issue #5") && out.contains("labels bug"), "{out}");
        let calls = f.calls().unwrap();
        assert!(calls.contains("issue view 5 -R acme/widgets"), "{calls}");
        assert!(!calls.contains("evil"), "{calls}");
        for bad in [json!(0), json!(-1), json!("5"), json!(2.5), Value::Null] {
            assert!(f.poll(json!({"kind": "pr_view", "pr": bad})).is_err());
        }
    }

    #[test]
    fn pr_view_and_checks_answer_with_the_facts() {
        let f = Fake::new("pr");
        f.answer(
            "pr_view.json",
            r#"{"number":7,"title":"Add x","state":"OPEN","isDraft":true,"headRefName":"agent/x","headRefOid":"abc","baseRefName":"main","mergeable":"MERGEABLE","reviewDecision":"","url":"https://github.com/acme/widgets/pull/7","body":"text"}"#,
        );
        let view = f.poll(json!({"kind": "pr_view", "pr": 7})).unwrap().unwrap();
        assert!(
            view.contains("(draft)") && view.contains("agent/x") && view.contains("review none"),
            "{view}"
        );
        f.answer(
            "pr_checks.json",
            r#"[{"name":"build","bucket":"pass","workflow":"lane"},{"name":"test","bucket":"fail","workflow":"lane"},{"name":"lint","bucket":"pending","workflow":""}]"#,
        );
        let checks = f.poll(json!({"kind": "pr_checks", "pr": 7})).unwrap().unwrap();
        assert!(checks.contains("3 checks; pass 1, fail 1, pending 1"), "{checks}");
        assert!(
            checks.contains("fail: lane / test") && checks.contains("pending: - / lint"),
            "{checks}"
        );
        assert!(!checks.contains("build"), "{checks}");
    }

    /// A check name and a workflow name are repository-controlled text: neither may carry a
    /// credential through to the model.
    #[test]
    fn check_names_and_workflows_are_scrubbed() {
        let f = Fake::new("checks-scrub");
        let token = format!("ghp_{}", "Q3w4E5r6T7".repeat(4));
        f.answer(
            "pr_checks.json",
            &format!(r#"[{{"name":"fail {token}","bucket":"fail","workflow":"lane {token}"}}]"#),
        );
        let checks = f.poll(json!({"kind": "pr_checks", "pr": 7})).unwrap().unwrap();
        assert!(!checks.contains(&token), "check evidence leaked: {checks}");
    }

    /// A credential pasted into a PR or an issue must not reach the model as probe evidence.
    #[test]
    fn pr_and_issue_bodies_are_scrubbed() {
        let f = Fake::new("scrub");
        let token = format!("ghp_{}", "Z9y8X7w6V5".repeat(4));
        f.answer(
            "pr_view.json",
            &format!(
                r#"{{"number":7,"title":"leak {token}","state":"OPEN","isDraft":false,"headRefName":"agent/{token}","headRefOid":"abc","baseRefName":"main","mergeable":"MERGEABLE","reviewDecision":"","url":"https://github.com/acme/widgets/pull/7","body":"pasted {token}"}}"#
            ),
        );
        f.answer(
            "issue_view.json",
            &format!(
                r#"{{"number":5,"title":"leak {token}","state":"OPEN","labels":[{{"name":"area {token}"}}],"url":"https://github.com/acme/widgets/issues/5","body":"pasted {token}"}}"#
            ),
        );
        for kind in ["pr_view", "issue_view"] {
            let args = if kind == "pr_view" {
                json!({"kind": "pr_view", "pr": 7})
            } else {
                json!({"kind": "issue_view", "issue": 5})
            };
            let out = f.poll(args).unwrap().expect("answered at once");
            assert!(!out.contains(&token), "{kind} leaked the token: {out}");
        }
    }

    #[test]
    fn probe_refuses_untrusted_gh() {
        let f = Fake::new("untrusted");
        let empty = f.bin.parent().unwrap().join("empty");
        std::fs::DirBuilder::new().mode(0o755).create(&empty).unwrap();
        let hooks = ProbeHooks::for_test_paths(
            &f.bin.parent().unwrap().join("work"),
            &f.bin.parent().unwrap().join("home"),
            &empty,
            "acme/widgets",
        );
        for kind in ["pr_view", "pr_checks", "run_log_failed", "issue_view"] {
            let args = json!({"kind": kind, "pr": 1, "run": 1, "issue": 1});
            let e = hooks
                .poll_event(kind, &args, Duration::from_secs(30))
                .unwrap()
                .unwrap_err();
            assert!(e.contains("trusted PATH"), "{kind}: {e}");
        }
        // A runner-owned worker session runs no gh at all.
        let owned = ProbeHooks::for_test_runner_owned_paths(
            &f.bin.parent().unwrap().join("work"),
            &f.bin.parent().unwrap().join("home"),
            &f.bin,
            "acme/widgets",
        );
        let e = owned
            .poll_event(
                "pr_view",
                &json!({"kind": "pr_view", "pr": 1}),
                Duration::from_secs(30),
            )
            .unwrap()
            .unwrap_err();
        assert!(e.contains("runner-owned"), "{e}");
        assert_eq!(f.calls(), None);
    }

    #[test]
    fn probe_refused_offline() {
        let f = Fake::new("offline");
        f.answer("pr_view.json", "{}");
        oxa_proto::set_net_policy(true, false);
        let e = f.poll(json!({"kind": "pr_view", "pr": 1}));
        oxa_proto::set_net_policy(false, false);
        let e = e.unwrap_err();
        assert!(e.contains("offline") || e.contains("network"), "{e}");
        assert_eq!(f.calls(), None, "no gh call while offline");
    }
}
