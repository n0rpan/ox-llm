//! K2 acceptance for `oxa lead board`: the owner and the lead run the whole board from a
//! terminal, every `--json` read round-trips, and a worker environment can only note or stick
//! its own card. Synthetic boards only, in a temporary home; no engine, no network.
use oxa_cli::run_lead_board;
use oxa_config::{Dirs, Env};
use oxa_fake::TestEnv;
use oxa_proto::{codes, OxaError};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

struct Fixture {
    env: TestEnv,
    dirs: Dirs,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new();
        let cwd = env.work();
        for args in [
            vec!["init", "-q"],
            vec!["remote", "add", "origin", "git@github.com:owner/repo.git"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&cwd)
                    .args(&args)
                    .status()
                    .expect("run git")
                    .success(),
                "git {args:?}"
            );
        }
        let dirs = Dirs::under(env.root());
        let fixture = Self { env, dirs, cwd };
        // The board is created on first use with a synthetic repository name.
        fixture.ok(&["list"]);
        fixture
    }
    fn owner(&self) -> Env {
        Env::from_pairs(&[])
    }
    fn lead(&self) -> Env {
        Env::from_pairs(&[("OXA_CODING_AGENT", "1")])
    }
    fn worker(&self, item: Option<&str>) -> Env {
        match item {
            Some(item) => Env::from_pairs(&[("OXA_WORKER_DEPTH", "1"), ("OXA_BOARD_ITEM", item)]),
            None => Env::from_pairs(&[("OXA_WORKER_DEPTH", "1")]),
        }
    }
    fn run(&self, env: &Env, words: &[&str]) -> Result<String, OxaError> {
        let words: Vec<String> = words.iter().map(|s| (*s).to_string()).collect();
        let mut out = Vec::new();
        run_lead_board(&words, &self.dirs, &self.cwd, env, &mut out)?;
        Ok(String::from_utf8(out).expect("utf-8 board output"))
    }
    /// A command that must succeed.
    fn ok(&self, words: &[&str]) -> String {
        self.run(&self.owner(), words)
            .unwrap_or_else(|e| panic!("{words:?} failed: {e}"))
    }
    fn json(&self, words: &[&str]) -> Value {
        let text = self.ok(words);
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{words:?} is not JSON ({e}): {text}"))
    }
    fn file(&self, name: &str, content: &str) -> String {
        let path = self.env.write(&format!("json/{name}"), content);
        path.to_string_lossy().into_owned()
    }
}

fn items(value: &Value) -> Vec<(u64, String, String)> {
    value
        .as_array()
        .expect("an item array")
        .iter()
        .map(|item| {
            (
                item["id"].as_u64().expect("id"),
                item["title"].as_str().expect("title").to_owned(),
                item["column"].as_str().expect("column").to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_owner_runs_the_whole_board_from_a_terminal() {
    let f = Fixture::new();
    // Add one Ready card, then a planning chunk as one write.
    let added = f.json(&[
        "add",
        "--title",
        "Plan the slice",
        "--done-when",
        "cargo check is green",
        "--ready",
        "--json",
    ]);
    assert_eq!(added["id"], 1, "{added}");
    let chunk = f.file(
        "chunk.json",
        r#"[{"title":"Write the store","done_when":"the tests pass"},
            {"title":"Sample GitHub","done_when":"one sample lands"}]"#,
    );
    f.ok(&["add", "--from-json", &chunk]);
    let listed = items(&f.json(&["list", "--json"]));
    assert_eq!(
        listed,
        vec![
            (2, "Write the store".into(), "backlog".into()),
            (3, "Sample GitHub".into(), "backlog".into()),
            (1, "Plan the slice".into(), "ready".into()),
        ]
    );
    // A card printed by `show --json` is added again as a new card: the JSON round-trips.
    let shown = f.json(&["show", "K2", "--json"]);
    let round_trip = f.file("card.json", &shown.to_string());
    let added = f.json(&["add", "--from-json", &round_trip, "--json"]);
    assert_eq!(added["applied"][0]["id"], 4, "{added}");
    let copied = f.json(&["show", "K4", "--json"]);
    assert_eq!(copied["title"], shown["title"]);
    assert_eq!(copied["done_when"], shown["done_when"]);
    assert_eq!(copied["kind"], shown["kind"]);
    // Move and reorder are relative and keep the pull order.
    f.ok(&["move", "K3", "ready", "--before", "K1"]);
    assert_eq!(
        items(&f.json(&["list", "--column", "ready", "--json"]))
            .iter()
            .map(|(id, _, _)| *id)
            .collect::<Vec<_>>(),
        vec![3, 1]
    );
    f.ok(&["reorder", "K1", "--top"]);
    assert_eq!(
        items(&f.json(&["list", "--column", "ready", "--json"]))
            .iter()
            .map(|(id, _, _)| *id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    // Split replaces the card in place with the children.
    let children = f.file(
        "children.json",
        r#"[{"title":"child a","done_when":"a is green"},{"title":"child b","done_when":"b is green"}]"#,
    );
    f.ok(&["split", "K1", "--from-json", &children]);
    let ready: Vec<u64> = items(&f.json(&["list", "--column", "ready", "--json"]))
        .iter()
        .map(|(id, _, _)| *id)
        .collect();
    assert_eq!(ready, vec![5, 6, 3], "children take the parent's place");
    // Stuck, unstick, note and a linked PR read back on the card.
    f.ok(&["stuck", "K2", "--cause", "ci_red", "--detail", "clippy"]);
    let card = f.json(&["show", "K2", "--json"]);
    assert_eq!(card["stuck"]["cause"], "ci_red");
    f.ok(&["unstick", "K2"]);
    f.ok(&["note", "K2", "--text", "waiting on review"]);
    f.ok(&["link", "K2", "--pr", "42"]);
    let card = f.json(&["show", "K2", "--json"]);
    assert!(card["stuck"].is_null(), "{card}");
    assert_eq!(card["note"], "waiting on review");
    assert_eq!(card["pr"]["number"], 42);
    // Capacity and the goal with a measurable done-when.
    f.ok(&["capacity", "n0rpan", "8"]);
    f.ok(&["capacity", "air", "6"]);
    f.ok(&["capacity", "--pause", "air"]);
    assert!(f.ok(&["board"]).contains("air 0/6 (paused)"));
    f.ok(&[
        "goal",
        "--text",
        "Ship the pull board",
        "--measure",
        "issues:2373,2262",
        "--measure",
        "manual:Installs on yoga",
        "--target",
        "10",
    ]);
    let goal = f.json(&["goal", "--json"]);
    assert_eq!(goal["measures"][0]["measure"], "issues_closed");
    assert_eq!(goal["measures"][1]["measure"], "manual");
    assert_eq!(goal["target_per_hour"], 10.0);
    f.ok(&["check", "M2", "--passed", "--evidence", "dogfood day"]);
    let goal = f.json(&["goal", "--json"]);
    assert_eq!(goal["measures"][1]["result"]["passed"], true);
    // Dispatch is the owner's switch and undo returns the previous state.
    f.ok(&["dispatch", "start"]);
    assert!(f.ok(&["board"]).contains("Dispatch: running"));
    f.ok(&["undo"]);
    assert!(f.ok(&["board"]).contains("Dispatch: paused"));
    // Remove and undo bring the card back with its text.
    f.ok(&["rm", "K2"]);
    assert!(
        f.json(&["list", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != 2),
        "K2 is removed"
    );
    f.ok(&["undo"]);
    assert_eq!(f.json(&["show", "K2", "--json"])["title"], "Write the store");
    // The whole board is one typed JSON object.
    let board = f.json(&["board", "--json"]);
    assert_eq!(board["repo"], "owner/repo");
    assert_eq!(board["schema"], 1);
    assert!(board["items"].as_array().unwrap().len() >= 4, "{board}");
}

#[test]
fn a_worker_may_only_note_and_stick_its_own_card() {
    let f = Fixture::new();
    f.ok(&["add", "--title", "Own card", "--done-when", "green"]);
    f.ok(&["add", "--title", "Other card", "--done-when", "green"]);
    let worker = f.worker(Some("K1"));
    f.run(&worker, &["note", "K1", "--text", "half way"]).unwrap();
    f.run(&worker, &["stuck", "K1", "--cause", "ci_red", "--detail", "red"])
        .unwrap();
    let refused = f
        .run(&worker, &["note", "K2", "--text", "not mine"])
        .expect_err("a worker notes only its own card");
    assert!(refused.message.contains("own item"), "{refused}");
    let refused = f
        .run(&worker, &["add", "--title", "sneak", "--done-when", "x"])
        .expect_err("a worker cannot add cards");
    assert!(refused.message.contains("owner or lead"), "{refused}");
    // Without the item the dispatcher gave it, a worker cannot write at all.
    let refused = f
        .run(&f.worker(None), &["note", "K1", "--text", "x"])
        .expect_err("a worker needs its card id");
    assert_eq!(refused.code, codes::DENIED, "{refused}");
    // Reading the board is fine for any environment.
    f.run(&worker, &["list", "--json"]).unwrap();
}

#[test]
fn a_lead_plans_but_does_not_set_capacity_the_goal_or_dispatch() {
    let f = Fixture::new();
    let lead = f.lead();
    f.run(&lead, &["add", "--title", "Lead card", "--done-when", "green"])
        .unwrap();
    f.run(
        &lead,
        &["add", "--title", "Lead ready", "--done-when", "green", "--ready"],
    )
    .unwrap();
    f.run(&lead, &["move", "K1", "ready"]).unwrap();
    let children = f.file(
        "lead-children.json",
        r#"[{"title":"lead child","done_when":"green"}]"#,
    );
    f.run(&lead, &["split", "K2", "--from-json", &children]).unwrap();
    assert!(f.run(&lead, &["capacity", "n0rpan", "4"]).is_err());
    assert!(f.run(&lead, &["goal", "--text", "lead goal"]).is_err());
    assert!(f.run(&lead, &["dispatch", "start"]).is_err());
    // A card the owner placed is not the lead's to remove.
    f.ok(&["add", "--title", "Owner card", "--done-when", "green"]);
    assert!(f.run(&lead, &["rm", "K4"]).is_err());
    assert!(f.run(&lead, &["rm", "K3"]).is_ok(), "the lead's own card");
    // The lead's own planning text still edits.
    f.run(&lead, &["edit", "K1", "--title", "Lead card, revised"])
        .unwrap();
}

#[test]
fn a_stale_edit_shows_both_versions_and_unknown_words_are_usage_errors() {
    let f = Fixture::new();
    f.ok(&["add", "--title", "original", "--done-when", "green"]);
    f.ok(&["edit", "K1", "--title", "server version"]);
    let stale = f
        .run(
            &f.owner(),
            &["edit", "K1", "--seq", "1", "--title", "client version"],
        )
        .expect_err("a stale edit is refused");
    assert!(stale.message.contains("server version"), "{stale}");
    assert!(stale.message.contains("client version"), "{stale}");
    assert_eq!(f.json(&["show", "K1", "--json"])["title"], "server version");
    for words in [vec!["frobnicate"], vec!["list", "--nonsense"], vec!["move", "K1"]] {
        let error = f.run(&f.owner(), &words).expect_err("usage error");
        assert_eq!(error.code, codes::USAGE, "{words:?}: {error}");
    }
}
