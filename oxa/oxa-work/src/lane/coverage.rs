//! Lane coverage as data: which changed paths a green lane run can be evidence for.
//!
//! `tools/ci/lane_coverage.json` is the one source. `tools/ci/lane_select.py` selects the
//! lane's packages from it; this module answers the same question for the Rust lane guard.
//! The file is read from the trusted base commit, never from the pushed head, so a head that
//! edits the data cannot widen its own coverage. Do not add path lists to the guard: change
//! the data file instead.
use crate::RunPolicy;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Where the data lives in the repository.
pub(crate) const COVERAGE_FILE: &str = "tools/ci/lane_coverage.json";
const MAX_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    workspaces: BTreeMap<String, Spec>,
    contract: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    packages: Packages,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Packages {
    All(String),
    List(Vec<String>),
}

/// The loaded coverage rule. An empty rule (the file is missing or invalid) covers nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Coverage {
    /// Workspace directory -> true when every package of it runs on the lane.
    workspaces: BTreeMap<String, bool>,
    /// Why the data could not be used, for the refusal text.
    pub(crate) problem: Option<String>,
}

impl Coverage {
    /// Parse the data file's bytes.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Coverage, String> {
        if bytes.len() > MAX_BYTES {
            return Err(format!("{COVERAGE_FILE} is larger than {MAX_BYTES} bytes"));
        }
        let raw: Raw =
            serde_json::from_slice(bytes).map_err(|e| format!("{COVERAGE_FILE} is invalid: {e}"))?;
        let _ = raw.contract;
        let mut workspaces = BTreeMap::new();
        for (name, spec) in raw.workspaces {
            if name.is_empty() || name.contains('/') || name.starts_with('.') {
                return Err(format!("{COVERAGE_FILE} names an invalid workspace {name:?}"));
            }
            let all = match spec.packages {
                Packages::All(word) if word == "all" => true,
                Packages::All(other) => {
                    return Err(format!(
                        "{COVERAGE_FILE}: packages of {name} is {other:?}, expected \"all\" or a list"
                    ))
                }
                Packages::List(_) => false,
            };
            workspaces.insert(name, all);
        }
        Ok(Coverage {
            workspaces,
            problem: None,
        })
    }

    /// Read the data from the trusted `base` commit. A missing or invalid file means no
    /// workspace is covered, and says which file to fix.
    pub(crate) fn load(policy: &RunPolicy, repo: &Path, base: &str) -> Coverage {
        let spec = format!("{base}:{COVERAGE_FILE}");
        let read = crate::output(crate::git(policy, repo, &["show", &spec]))
            .map_err(|e| e.to_string())
            .and_then(|out| {
                if out.status.success() {
                    Ok(out.stdout)
                } else {
                    Err(format!("{COVERAGE_FILE} is missing at the base"))
                }
            })
            .and_then(|bytes| Coverage::parse(&bytes));
        read.unwrap_or_else(|problem| Coverage {
            workspaces: BTreeMap::new(),
            problem: Some(problem),
        })
    }

    /// The changed paths no lane run is evidence for. A path is covered only under a workspace
    /// whose packages are all tested. It is uncovered under a workspace that is only partly
    /// tested (fixtures and shared inputs included), and anywhere outside the listed
    /// workspaces with a `Cargo.toml` ancestor (a crate the lane does not know). Paths with no
    /// Cargo ancestor outside the workspaces (documentation, the README) need no evidence.
    pub(crate) fn uncovered<'a>(
        &self,
        changed: &'a [String],
        has_cargo_ancestor: impl Fn(&str) -> bool,
    ) -> Vec<&'a str> {
        changed
            .iter()
            .map(String::as_str)
            .filter(|path| match self.workspace_of(path) {
                Some(all) => !all,
                None => has_cargo_ancestor(path),
            })
            .collect()
    }

    fn workspace_of(&self, path: &str) -> Option<bool> {
        let first = path.split('/').next()?;
        // A file directly in the repository root is no workspace member.
        (first != path)
            .then(|| self.workspaces.get(first).copied())
            .flatten()
    }

    /// The refusal for the first uncovered path.
    pub(crate) fn refusal(&self, path: &str) -> String {
        match &self.problem {
            Some(problem) => format!(
                "the lane run cannot be evidence for {path}: {problem}; that change needs the hosted route"
            ),
            None => format!("the lane run does not test {path}; that change needs the hosted route"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const DATA: &str = r#"{"workspaces":{"oxa":{"packages":"all"},"engine":{"packages":["server"]}},"contract":["tools/ci/lane_coverage.json"]}"#;
    const WIDENED: &str =
        r#"{"workspaces":{"oxa":{"packages":"all"},"engine":{"packages":"all"}},"contract":[]}"#;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    fn cargo_ancestor(path: &str) -> bool {
        // A repository with crates in `fuzz/`, `newcrate/` and the workspaces only.
        ["fuzz/", "newcrate/", "oxa/", "engine/"]
            .iter()
            .any(|p| path.starts_with(p))
    }

    #[test]
    fn engine_and_unknown_crates_are_uncovered_and_oxa_and_docs_are_not() {
        let rule = Coverage::parse(DATA.as_bytes()).unwrap();
        let changed = paths(&[
            "engine/tests/fixtures/klein-4b-q8.tensors.txt",
            "engine/crates/server/src/lib.rs",
            "fuzz/oxa/src/lib.rs",
            "newcrate/src/lib.rs",
            "oxa/oxa-work/src/lib.rs",
            "oxa/Cargo.lock",
            "README.md",
            "docs/notes.md",
        ]);
        assert_eq!(
            rule.uncovered(&changed, cargo_ancestor),
            vec![
                "engine/tests/fixtures/klein-4b-q8.tensors.txt",
                "engine/crates/server/src/lib.rs",
                "fuzz/oxa/src/lib.rs",
                "newcrate/src/lib.rs",
            ]
        );
    }

    #[test]
    fn invalid_data_is_refused_and_covers_nothing() {
        assert!(Coverage::parse(b"{\"workspaces\":{}}").is_err());
        assert!(Coverage::parse(br#"{"workspaces":{},"contract":[],"extra":1}"#).is_err());
        assert!(Coverage::parse(br#"{"workspaces":{"oxa":{"packages":"some"}},"contract":[]}"#).is_err());
        let none = Coverage::default();
        let changed = paths(&["oxa/oxa-work/src/lib.rs", "README.md"]);
        assert_eq!(
            none.uncovered(&changed, cargo_ancestor),
            vec!["oxa/oxa-work/src/lib.rs"]
        );
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    fn policy(storage: &Path) -> RunPolicy {
        RunPolicy {
            git_binary: "git".into(),
            host_path: std::env::var_os("PATH").unwrap_or_default(),
            terminate_binary: None,
            offline: true,
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            runner_owns_github: false,
            autonomy: Default::default(),
            worker_github: Default::default(),
            authority_now: None,
            storage: storage.to_path_buf(),
        }
    }

    #[test]
    fn the_data_is_read_from_the_base_not_the_head() {
        let dir = std::env::temp_dir().join(format!("oxa-coverage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools/ci")).unwrap();
        git_in(&dir, &["init", "-q", "-b", "main", "."]);
        // A base without the file covers nothing and names the file.
        std::fs::write(dir.join("README.md"), "x").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "no data"]);
        let p = policy(&dir);
        let empty = crate::checked(crate::git(&p, &dir, &["rev-parse", "HEAD"])).unwrap();
        let rule = Coverage::load(&p, &dir, &empty);
        assert!(
            rule.refusal("oxa/x.rs").contains(COVERAGE_FILE),
            "{}",
            rule.refusal("oxa/x.rs")
        );
        assert_eq!(
            rule.uncovered(&paths(&["oxa/x.rs"]), cargo_ancestor),
            vec!["oxa/x.rs"]
        );
        // The base has the data; the head widens it to all of engine: the base still rules.
        std::fs::write(dir.join(COVERAGE_FILE), DATA).unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "data"]);
        let base = crate::checked(crate::git(&p, &dir, &["rev-parse", "HEAD"])).unwrap();
        std::fs::write(dir.join(COVERAGE_FILE), WIDENED).unwrap();
        git_in(&dir, &["commit", "-q", "-am", "widen"]);
        let rule = Coverage::load(&p, &dir, &base);
        assert!(rule.problem.is_none());
        assert_eq!(
            rule.uncovered(&paths(&["engine/crates/gguf/src/lib.rs"]), cargo_ancestor),
            vec!["engine/crates/gguf/src/lib.rs"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
