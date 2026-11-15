//! The report-issue workflow for an explicit user-launched --report-problems run.
//! Normal `report-issue submit` retains its controlling-terminal confirmation.
use crate::report_cmd::{clean_notes, report_repo, save_draft, scrub, Runner, ScrubCtx};
use oxa_config::{Dirs, Settings};
use oxa_work::{ProblemNote, ReportRecord};
use serde_json::Value;
use std::{
    fs::File,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) fn process(program: &str, args: &[String], cwd: &Path, lock: &File) -> Option<(i32, String)> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::from(lock.try_clone().ok()?))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let result = (&mut stdout).take(1_048_576).read_to_string(&mut out);
        let _ = tx.send(result.ok().map(|_| out));
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some((
                    status.code().unwrap_or(1),
                    rx.recv_timeout(Duration::from_secs(2)).ok()??,
                ))
            }
            Ok(None) if start.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
fn call(runner: Runner<'_>, args: &[String]) -> Result<String, String> {
    match runner("gh", args) {
        Some((0, text)) => Ok(text),
        _ => Err("GitHub search or write failed; the local draft is retained".into()),
    }
}
fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}
fn matches(note: &ProblemNote, candidate: &Value) -> bool {
    let title = candidate["title"].as_str().unwrap_or_default().to_lowercase();
    if title == note.title.to_lowercase() {
        return true;
    }
    let all = format!(
        "{title} {}",
        candidate["body"].as_str().unwrap_or_default().to_lowercase()
    );
    let terms: std::collections::HashSet<_> = note
        .keywords
        .iter()
        .flat_map(|q| q.split_whitespace())
        .filter(|word| word.len() >= 2)
        .map(str::to_lowercase)
        .collect();
    terms.len() >= 3 && terms.iter().all(|word| all.contains(word))
}
pub(crate) fn report(
    note: &ProblemNote,
    dirs: &Dirs,
    settings: &Settings,
    ctx: &ScrubCtx,
    offline: bool,
    runner: Runner<'_>,
) -> Result<ReportRecord, String> {
    let (draft, title, body) =
        save_draft(&note.title, &note.notes, dirs, ctx, runner).map_err(|e| e.to_string())?;
    let mut result = ReportRecord {
        disposition: "local".into(),
        draft,
        url: None,
        detail: String::new(),
    };
    if offline {
        result.detail = "offline: report retained locally".into();
        return Ok(result);
    }
    let Some(repo) = report_repo(settings) else {
        result.detail = "issue reporting is not configured; draft retained locally".into();
        return Ok(result);
    };
    let note = ProblemNote {
        title,
        notes: clean_notes(&note.notes, ctx),
        keywords: note.keywords.iter().map(|q| scrub(q, ctx)).collect(),
    };
    if note.notes.is_empty()
        || !(2..=3).contains(&note.keywords.len())
        || note.keywords.iter().any(|q| q.trim().is_empty())
    {
        result.detail = "report has no safe symptom text or queries; draft retained locally".into();
        return Ok(result);
    }
    let attempt = (|| -> Result<(), String> {
        let mut candidates = vec![];
        for query in &note.keywords {
            for kind in ["issue", "pr"] {
                let text = call(
                    runner,
                    &words(&[
                        kind,
                        "list",
                        "--repo",
                        &repo,
                        "--state",
                        "open",
                        "--search",
                        query,
                        "--limit",
                        "5",
                        "--json",
                        "number,title,url",
                    ]),
                )?;
                let rows: Vec<Value> =
                    serde_json::from_str(&text).map_err(|_| "invalid GitHub search response".to_string())?;
                for row in rows {
                    let number = row["number"].as_u64().ok_or("search match has no number")?;
                    if !candidates.contains(&(kind, number)) {
                        candidates.push((kind, number));
                    }
                }
            }
        }
        // Read all top matches before any write, including PRs which may already fix it.
        let mut read = vec![];
        for (kind, number) in candidates {
            let fields = if kind == "issue" {
                "title,body,url,comments"
            } else {
                "title,body,url"
            };
            let text = call(
                runner,
                &words(&[
                    kind,
                    "view",
                    &number.to_string(),
                    "--repo",
                    &repo,
                    "--json",
                    fields,
                ]),
            )?;
            let value: Value =
                serde_json::from_str(&text).map_err(|_| "invalid GitHub match response".to_string())?;
            if !value.is_object()
                || value["title"].as_str().is_none()
                || value["body"].as_str().is_none()
                || value["url"].as_str().is_none()
            {
                return Err("incomplete GitHub match; draft retained locally".into());
            }
            read.push((kind, number, value));
        }
        if let Some((_, _, value)) = read
            .iter()
            .find(|(kind, _, v)| *kind == "pr" && matches(&note, v))
        {
            result.disposition = "existing_pr".into();
            result.url = value["url"].as_str().map(str::to_string);
            return Ok(());
        }
        let body_path = result.draft.with_extension("work-body.md");
        if let Some((_, number, value)) = read
            .iter()
            .find(|(kind, _, v)| *kind == "issue" && matches(&note, v))
        {
            let already = value["body"].as_str().unwrap_or_default().contains(&note.notes)
                || value["comments"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|c| c["body"].as_str().unwrap_or_default().contains(&note.notes))
                });
            if !already {
                oxa_resources::write_atomic(&body_path, note.notes.as_bytes()).map_err(|e| e.to_string())?;
                call(
                    runner,
                    &words(&[
                        "issue",
                        "comment",
                        &number.to_string(),
                        "--repo",
                        &repo,
                        "--body-file",
                        &body_path.to_string_lossy(),
                    ]),
                )?;
            }
            result.disposition = "existing_issue".into();
            result.url = value["url"].as_str().map(str::to_string);
        } else {
            let links = read
                .iter()
                .filter_map(|(_, _, v)| v["url"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let body = if links.is_empty() {
                body.clone()
            } else {
                format!("{body}\n\nRelated open reports (different symptoms):\n{links}\n")
            };
            let body = scrub(&body, ctx);
            oxa_resources::write_atomic(&body_path, body.as_bytes()).map_err(|e| e.to_string())?;
            let url = call(
                runner,
                &words(&[
                    "issue",
                    "create",
                    "--repo",
                    &repo,
                    "--title",
                    &note.title,
                    "--body-file",
                    &body_path.to_string_lossy(),
                ]),
            )?;
            result.disposition = "filed".into();
            result.url = Some(url.trim().into());
        }
        let _ = std::fs::remove_file(body_path);
        Ok(())
    })();
    if let Err(error) = attempt {
        result.detail = error;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    fn note() -> ProblemNote {
        ProblemNote {
            title: "oxa work commit is read only".into(),
            notes: "Another synthetic reproduction of a read only git commit.".into(),
            keywords: vec!["oxa work".into(), "read only git".into()],
        }
    }
    fn settings() -> Settings {
        Settings::from_values(
            serde_json::json!({"oxa":{"reportIssue":{"repo":"example/project"}}}),
            serde_json::json!({}),
        )
    }
    #[test]
    fn matching_issue_is_commented_after_all_queries_and_reads() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut log = vec![];
        let mut runner = |program: &str, args: &[String]| {
            if program != "gh" {
                return Some((0, "{}".into()));
            }
            log.push(args.to_vec());
            match (args[0].as_str(), args[1].as_str()) {
                ("issue", "list") => Some((0, r#"[{"number":7,"title":"oxa work commit is read only"}]"#.into())),
                ("pr", "list") => Some((0, "[]".into())),
                ("issue", "view") => Some((0, r#"{"title":"oxa work commit is read only","body":"Original symptom","url":"https://github.com/example/project/issues/7","comments":[]}"#.into())),
                ("issue", "comment") => { let body = std::fs::read_to_string(args.last().unwrap()).unwrap(); assert_eq!(body, note().notes); Some((0, "commented".into())) },
                _ => panic!("unexpected write: {args:?}"),
            }
        };
        let result = report(
            &note(),
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut runner,
        )
        .unwrap();
        assert_eq!(result.disposition, "existing_issue");
        assert_eq!(log.iter().filter(|a| a[1] == "list").count(), 4);
        assert_eq!(log.last().unwrap()[1], "comment");
    }
    #[test]
    fn failed_search_and_offline_never_file_blind() {
        for offline in [false, true] {
            let env = TestEnv::new();
            let dirs = Dirs::under(env.root());
            let mut gh_calls = 0;
            let mut runner = |program: &str, _: &[String]| {
                if program == "gh" {
                    gh_calls += 1;
                    None
                } else {
                    Some((0, "{}".into()))
                }
            };
            let result = report(
                &note(),
                &dirs,
                &settings(),
                &ScrubCtx::default(),
                offline,
                &mut runner,
            )
            .unwrap();
            assert_eq!(result.disposition, "local");
            assert!(result.draft.exists());
            assert!(!result.detail.is_empty());
            assert_eq!(gh_calls, if offline { 0 } else { 1 });
        }
    }
    #[test]
    fn an_open_fixing_pr_prevents_issue_or_comment_writes() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut runner = |program: &str, args: &[String]| {
            if program != "gh" {
                return Some((0, "{}".into()));
            }
            if args[1] == "list" {
                return Some((
                    0,
                    if args[0] == "pr" {
                        r#"[{"number":9}]"#
                    } else {
                        "[]"
                    }
                    .into(),
                ));
            }
            assert_eq!(args[1], "view");
            Some((0, r#"{"title":"oxa work commit is read only","body":"Fixed by this PR","url":"https://github.com/example/project/pull/9"}"#.into()))
        };
        let result = report(
            &note(),
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut runner,
        )
        .unwrap();
        assert_eq!(result.disposition, "existing_pr");
    }
    #[test]
    fn new_report_is_scrubbed_and_related_reports_are_linked() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut n = note();
        n.notes = "Synthetic reproduction token=example at /home/synthetic/project with user@example.invalid"
            .into();
        let mut runner = |program: &str, args: &[String]| {
            if program != "gh" {
                return Some((0, "{}".into()));
            }
            match args[1].as_str() {
                "list" => Some((0, if args[0] == "issue" { r#"[{"number":8}]"# } else { "[]" }.into())),
                "view" => Some((0, r#"{"title":"Different symptom","body":"Unrelated reproduction","url":"https://github.com/example/project/issues/8","comments":[]}"#.into())),
                "create" => { let body = std::fs::read_to_string(args.last().unwrap()).unwrap(); assert!(!body.contains("token=example") && !body.contains("/home/synthetic") && !body.contains("user@example.invalid")); assert!(body.contains("issues/8")); Some((0, "https://github.com/example/project/issues/10".into())) },
                _ => panic!("unexpected command"),
            }
        };
        let result = report(&n, &dirs, &settings(), &ScrubCtx::default(), false, &mut runner).unwrap();
        assert_eq!(result.disposition, "filed");
    }
}
