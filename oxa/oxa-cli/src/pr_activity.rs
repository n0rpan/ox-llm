//! `wait` until `{"kind":"pr_activity","pr":<n>,"head":<40 hex>,"since":<RFC 3339 UTC, optional>}`:
//! a lead that pinned a head waits for the next review or PR comment instead of polling.
//!
//! Pending while the PR is open, at `head`, and nothing is new against the baseline. Returns at
//! once when a review or conversation comment is new, when the head moved (waiting at a stale
//! head is pointless), or when the PR is merged or closed. The baseline lives in the session
//! (`ProbeHooks::activity`), keyed by PR: the first poll for a (pr, head) records what is
//! there, unless `since` is given, then items after that time count as new. Evidence that was
//! returned joins the baseline, so the same wait does not wake twice on it.
//!
//! SECURITY. Same rules as `wait_probe.rs`: one fixed `gh pr view` argument vector, no shell,
//! repository from the `origin` remote only, `GH_HOST` pinned, read only, own read cap (a bigger
//! answer is an error, never a partial read). Author, state and body are GitHub-controlled text
//! and pass through the credential scrubber before the model sees them.
use crate::wait_probe::{valid_sha, ProbeHooks};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Instant;

/// Most bytes of one `gh pr view ... comments,reviews` answer; more is an error.
const ACTIVITY_READ_CAP: usize = 4 * 1024 * 1024;
/// Most new items listed in the evidence.
const MAX_LISTED: usize = 10;
/// Most characters of one body in the evidence.
const BODY_CHARS: usize = 1500;

/// What a session has already seen of one PR at one head.
pub(crate) struct ActivityBaseline {
    head: String,
    seen: HashSet<String>,
}

struct Item {
    id: String,
    review: bool,
    state: String,
    login: String,
    time: String,
    url: String,
    body: String,
}

/// `YYYY-MM-DDTHH:MM:SSZ`, digits checked, so it compares as text with GitHub's timestamps.
fn valid_since(since: Option<&Value>) -> Result<Option<String>, String> {
    let Some(value) = since.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = value.as_str().unwrap_or("");
    let shape = b"dddd-dd-ddTdd:dd:ddZ";
    let ok = text.len() == shape.len()
        && text.bytes().zip(shape.iter()).all(|(b, s)| match *s {
            b'd' => b.is_ascii_digit(),
            other => b == other,
        });
    if ok {
        Ok(Some(text.to_string()))
    } else {
        Err("since must be a UTC time like 2026-11-20T10:00:00Z".into())
    }
}

fn items(view: &Value) -> Vec<Item> {
    let text = |v: &Value, key: &str| v[key].as_str().unwrap_or("").to_string();
    let mut out = Vec::new();
    for (key, review, time_key) in [("reviews", true, "submittedAt"), ("comments", false, "createdAt")] {
        for v in view[key].as_array().into_iter().flatten() {
            let url = text(v, "url");
            let id = Some(text(v, "id"))
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| url.clone());
            if id.is_empty() {
                continue;
            }
            out.push(Item {
                id,
                review,
                state: text(v, "state"),
                login: v["author"]["login"].as_str().unwrap_or("unknown").to_string(),
                time: text(v, time_key),
                url,
                body: text(v, "body"),
            });
        }
    }
    out
}

impl ProbeHooks {
    pub(crate) fn pr_activity(&self, args: &Value, deadline: Instant) -> Result<Option<String>, String> {
        let pr = args["pr"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("pr must be a positive integer")?;
        let head = valid_sha(args["head"].as_str())?;
        let since = valid_since(args.get("since"))?;
        let repo = self.repo(Self::remaining(deadline)?)?;
        let call: Vec<String> = [
            "pr",
            "view",
            &pr.to_string(),
            "-R",
            &repo,
            "--json",
            "state,headRefOid,comments,reviews",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let out = self.run_accepting("gh", &call, Self::remaining(deadline)?, ACTIVITY_READ_CAP, &[0])?;
        let view: Value =
            serde_json::from_str(&out).map_err(|_| "gh printed output that is not JSON".to_string())?;
        let oid = view["headRefOid"].as_str().unwrap_or("").to_ascii_lowercase();
        let state = view["state"].as_str().unwrap_or("");
        if matches!(state, "MERGED" | "CLOSED") {
            return Ok(Some(format!("PR #{pr} is {state}; head {oid}")));
        }
        if oid != head {
            return Ok(Some(format!("PR #{pr} head changed: pinned {head}, now {oid}")));
        }
        let all = items(&view);
        let mut baselines = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        let mut fresh: Vec<Item> = if let Some(b) = baselines.get(&pr).filter(|b| b.head == head) {
            all.into_iter().filter(|i| !b.seen.contains(&i.id)).collect()
        } else {
            baselines.insert(
                pr,
                ActivityBaseline {
                    head: head.clone(),
                    seen: all.iter().map(|i| i.id.clone()).collect(),
                },
            );
            all.into_iter()
                .filter(|i| since.as_deref().is_some_and(|s| i.time.as_str() > s))
                .collect()
        };
        if fresh.is_empty() {
            return Ok(None);
        }
        if let Some(b) = baselines.get_mut(&pr) {
            b.seen.extend(fresh.iter().map(|i| i.id.clone()));
        }
        drop(baselines);
        fresh.sort_by(|a, b| a.time.cmp(&b.time));
        let reviews = fresh.iter().filter(|i| i.review).count();
        let mut text = format!(
            "PR #{pr} at {head}: {reviews} new review(s), {} new comment(s)",
            fresh.len() - reviews
        );
        for item in fresh.iter().take(MAX_LISTED) {
            let login = self.scrubbed(&item.login, 100);
            let url = self.scrubbed(&item.url, 300);
            if item.review {
                let state = self.scrubbed(&item.state, 40);
                text.push_str(&format!("\nreview {state} by {login} at {} {url}", item.time));
            } else {
                text.push_str(&format!("\ncomment by {login} at {} {url}", item.time));
            }
            let body = self.scrubbed(&item.body, BODY_CHARS);
            if !body.trim().is_empty() {
                text.push('\n');
                text.push_str(&body);
            }
        }
        if fresh.len() > MAX_LISTED {
            text.push_str(&format!("\n... {} more not listed", fresh.len() - MAX_LISTED));
        }
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::Hooks;
    use serde_json::json;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;
    use std::time::Duration;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    /// A synthetic `gh` answering `pr view` from `pr_view.json` and recording its argv in `calls`.
    struct Fake {
        bin: PathBuf,
        hooks: ProbeHooks,
    }

    impl Fake {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("oxa-pa-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for d in ["bin", "work", "home"] {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(root.join(d))
                    .unwrap();
            }
            let gh = root.join("bin/gh");
            std::fs::write(
                &gh,
                "#!/bin/sh\ndir=\"$(dirname \"$0\")\"\necho \"$@\" >> \"$dir/calls\"\n[ \"$1 $2\" = \"pr view\" ] && cat \"$dir/pr_view.json\" || exit 2\n",
            )
            .unwrap();
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

        fn view(&self, state: &str, head: &str, reviews: Value, comments: Value) {
            let doc = json!({"state": state, "headRefOid": head, "reviews": reviews, "comments": comments});
            std::fs::write(self.bin.join("pr_view.json"), doc.to_string()).unwrap();
        }

        fn calls(&self) -> Option<String> {
            std::fs::read_to_string(self.bin.join("calls")).ok()
        }

        fn poll(&self, args: Value) -> Result<Option<String>, String> {
            self.hooks
                .poll_event("pr_activity", &args, Duration::from_secs(30))
                .expect("pr_activity is answered")
        }
    }

    fn args() -> Value {
        json!({"kind": "pr_activity", "pr": 7, "head": HEAD})
    }

    fn comment(id: &str, at: &str, body: &str) -> Value {
        json!({"id": id, "author": {"login": "alice"}, "createdAt": at, "url": format!("https://github.com/acme/widgets/pull/7#{id}"), "body": body})
    }

    fn review(id: &str, state: &str, at: &str) -> Value {
        json!({"id": id, "author": {"login": "bob"}, "submittedAt": at, "state": state, "url": format!("https://github.com/acme/widgets/pull/7#{id}"), "body": "please fix"})
    }

    #[test]
    fn unchanged_baseline_stays_pending_then_a_new_review_wakes_once() {
        let f = Fake::new("wake");
        let old = comment("c1", "2026-11-20T09:00:00Z", "old");
        f.view("OPEN", HEAD, json!([]), json!([old.clone()]));
        assert_eq!(f.poll(args()), Ok(None));
        assert_eq!(f.poll(args()), Ok(None));
        let r = review("r1", "CHANGES_REQUESTED", "2026-11-20T10:00:00Z");
        f.view("OPEN", HEAD, json!([r.clone()]), json!([old.clone()]));
        let text = f.poll(args()).unwrap().expect("wakes");
        assert!(text.contains("1 new review(s), 0 new comment(s)"), "{text}");
        assert!(
            text.contains("CHANGES_REQUESTED") && text.contains("bob") && text.contains("#r1"),
            "{text}"
        );
        assert_eq!(f.poll(args()), Ok(None), "no duplicate wake");
        let c = comment("c2", "2026-11-20T11:00:00Z", "ping");
        f.view("OPEN", HEAD, json!([r]), json!([old, c]));
        let text = f.poll(args()).unwrap().expect("wakes again");
        assert!(
            text.contains("0 new review(s), 1 new comment(s)") && text.contains("ping"),
            "{text}"
        );
        assert!(!text.contains("CHANGES_REQUESTED"), "{text}");
    }

    #[test]
    fn head_change_and_closed_pr_return_at_once() {
        let f = Fake::new("head");
        f.view("OPEN", OTHER, json!([]), json!([]));
        let text = f.poll(args()).unwrap().expect("returns");
        assert_eq!(text, format!("PR #7 head changed: pinned {HEAD}, now {OTHER}"));
        f.view("MERGED", HEAD, json!([]), json!([]));
        let text = f.poll(args()).unwrap().expect("returns");
        assert!(text.contains("MERGED") && text.contains(HEAD), "{text}");
        f.view("CLOSED", HEAD, json!([]), json!([]));
        assert!(f.poll(args()).unwrap().unwrap().contains("CLOSED"));
    }

    #[test]
    fn since_counts_items_after_the_given_time_on_the_first_poll() {
        let f = Fake::new("since");
        f.view(
            "OPEN",
            HEAD,
            json!([]),
            json!([
                comment("old", "2026-11-20T08:00:00Z", "before"),
                comment("new", "2026-11-20T10:30:00Z", "after")
            ]),
        );
        let mut a = args();
        a["since"] = json!("2026-11-20T10:00:00Z");
        let text = f.poll(a.clone()).unwrap().expect("the newer item wakes");
        assert!(text.contains("after") && !text.contains("before"), "{text}");
        assert_eq!(f.poll(a), Ok(None));
    }

    #[test]
    fn arguments_are_validated_before_gh_runs() {
        let f = Fake::new("args");
        f.view("OPEN", HEAD, json!([]), json!([]));
        for bad in [
            json!({"kind": "pr_activity", "pr": 0, "head": HEAD}),
            json!({"kind": "pr_activity", "pr": "7", "head": HEAD}),
            json!({"kind": "pr_activity", "pr": 7, "head": "abc"}),
            json!({"kind": "pr_activity", "pr": 7, "head": HEAD, "since": "yesterday"}),
            json!({"kind": "pr_activity", "pr": 7, "head": HEAD, "since": "2026-11-20 10:00:00Z"}),
            json!({"kind": "pr_activity", "pr": 7, "head": HEAD, "since": 5}),
        ] {
            assert!(f.poll(bad.clone()).is_err(), "{bad}");
        }
        assert_eq!(f.calls(), None);
    }

    #[test]
    fn refused_offline_and_in_a_runner_owned_session() {
        let f = Fake::new("refused");
        f.view("OPEN", HEAD, json!([]), json!([]));
        let root = f.bin.parent().unwrap();
        let owned = ProbeHooks::for_test_runner_owned_paths(
            &root.join("work"),
            &root.join("home"),
            &f.bin,
            "acme/widgets",
        );
        let error = owned
            .poll_event("pr_activity", &args(), Duration::from_secs(30))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("runner-owned"), "{error}");
        oxa_proto::set_net_policy(true, false);
        let offline = f.poll(args());
        oxa_proto::set_net_policy(false, false);
        assert!(offline.is_err());
        assert_eq!(f.calls(), None, "no gh call is made");
    }

    #[test]
    fn a_secret_shaped_body_is_scrubbed_and_long_bodies_are_cut() {
        let f = Fake::new("scrub");
        let token = format!("ghp_{}", "Q3w4E5r6T7".repeat(4));
        f.view("OPEN", HEAD, json!([]), json!([]));
        assert_eq!(f.poll(args()), Ok(None));
        let long = "x".repeat(5000);
        f.view(
            "OPEN",
            HEAD,
            json!([]),
            json!([
                comment("c1", "2026-11-20T10:00:00Z", &format!("pasted {token}")),
                comment("c2", "2026-11-20T10:01:00Z", &long)
            ]),
        );
        let text = f.poll(args()).unwrap().expect("wakes");
        assert!(!text.contains(&token), "{text}");
        assert!(!text.contains(&"x".repeat(1600)), "long body must be cut");
    }

    #[test]
    fn at_most_ten_items_are_listed() {
        let f = Fake::new("many");
        f.view("OPEN", HEAD, json!([]), json!([]));
        assert_eq!(f.poll(args()), Ok(None));
        let many: Vec<Value> = (0..13)
            .map(|n| comment(&format!("c{n}"), &format!("2026-11-20T10:{n:02}:00Z"), "hi"))
            .collect();
        f.view("OPEN", HEAD, json!([]), json!(many));
        let text = f.poll(args()).unwrap().unwrap();
        assert!(
            text.contains("13 new comment(s)") && text.ends_with("... 3 more not listed"),
            "{text}"
        );
    }

    #[test]
    fn the_call_is_one_fixed_read() {
        let f = Fake::new("argv");
        f.view("OPEN", HEAD, json!([]), json!([]));
        f.poll(args()).unwrap();
        f.poll(args()).unwrap();
        assert_eq!(
            f.calls().unwrap(),
            "pr view 7 -R acme/widgets --json state,headRefOid,comments,reviews\n".repeat(2)
        );
    }
}
