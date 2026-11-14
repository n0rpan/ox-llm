//! Lane `search`: the result shape (W4: deterministic, capped) and the argument rules.
mod common;

use common::*;
use oxa_proto::{Access, CancelToken, Mode, Tool};
use oxa_tools::descriptions::web_search as text;
use oxa_web::{parse_query, shape, Brave, Budgets, Found, WebSearchTool};
use serde_json::json;
use std::sync::Arc;

fn found(i: usize, snippets: Vec<String>) -> Found {
    Found {
        title: format!("T{i}"),
        url: format!("https://e.com/{i}"),
        age: None,
        snippets,
    }
}

#[test]
fn empty_is_no_results() {
    assert_eq!(
        shape("q", &[], None, &Budgets::default()),
        "Web search results for: q\n\nNo results."
    );
    // A source without a URL is not a source.
    let f = Found {
        title: "x".into(),
        ..Found::default()
    };
    assert_eq!(
        shape("q", &[f], None, &Budgets::default()),
        "Web search results for: q\n\nNo results."
    );
}

#[test]
fn snippets_are_normalised_deduplicated_and_capped() {
    let long = "word ".repeat(400);
    let f = Found {
        title: "  A\n title\t ".into(),
        url: "https://e.com/a".into(),
        age: Some("2024-01-02".into()),
        snippets: vec!["  one\n\n two ".into(), "one two".into(), "".into(), long.clone()],
    };
    let out = shape("  multi   word\nquery ", &[f], None, &Budgets::default());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Web search results for: multi word query");
    assert_eq!(lines[2], "[1] A title");
    assert_eq!(lines[3], "https://e.com/a");
    assert_eq!(lines[4], "Published: 2024-01-02");
    assert_eq!(lines[5], "one two");
    assert!(lines[6].ends_with(text::CUT));
    assert!(lines[6].len() <= Budgets::default().per_snippet);
    assert_eq!(lines.len(), 7, "the duplicate and the empty snippet are dropped");
}

#[test]
fn oversized_results_stay_within_the_budgets_with_the_omitted_line() {
    let big: Vec<Found> = (0..50)
        .map(|i| {
            found(
                i,
                (0..6).map(|j| format!("{i}-{j} {}", "x".repeat(900))).collect(),
            )
        })
        .collect();
    let b = Budgets::default();
    let out = shape("big", &big, None, &b);
    assert!(out.len() <= b.total, "{} > {}", out.len(), b.total);
    assert!(out.len() < 16384, "fits the default tool-output cap");
    let last = out.lines().last().unwrap();
    assert!(
        last.starts_with('(') && last.ends_with("more sources omitted)"),
        "{last}"
    );
    let shown = out.lines().filter(|l| l.starts_with('[')).count();
    assert_eq!(last, format!("({} more sources omitted)", 50 - shown));
    // Each block within the per-source budget.
    for block in out.split("\n\n").skip(1).filter(|b| b.starts_with('[')) {
        assert!(block.len() <= b.per_source, "{}", block.len());
    }
    // The order is the provider's.
    assert!(out.find("[1] T0").unwrap() < out.find("[2] T1").unwrap());
    // Deterministic.
    assert_eq!(out, shape("big", &big, None, &b));
}

#[test]
fn count_limits_the_sources() {
    let many: Vec<Found> = (0..5).map(|i| found(i, vec!["s".into()])).collect();
    let out = shape("q", &many, Some(2), &Budgets::default());
    assert_eq!(out.lines().filter(|l| l.starts_with('[')).count(), 2);
    assert!(!out.contains("omitted"));
}

#[test]
fn the_600_character_rule() {
    let ok = "é".repeat(600);
    assert_eq!(parse_query(&json!({"query": ok})).unwrap().q.chars().count(), 600);
    let long = "a".repeat(601);
    assert_eq!(
        parse_query(&json!({"query": long})).unwrap_err(),
        "Invalid query: it has 601 characters, the limit is 600\nFix: shorten the query to 600 characters or fewer"
    );
}

#[test]
fn argument_validation_is_model_facing_and_fixed() {
    assert_eq!(parse_query(&json!({})).unwrap_err(), text::QUERY_MISSING);
    assert_eq!(
        parse_query(&json!({"query": "   "})).unwrap_err(),
        text::QUERY_MISSING
    );
    assert_eq!(
        parse_query(&json!({"query": 3})).unwrap_err(),
        text::WRONG_TYPE.replace("{name}", "query")
    );
    for bad in [json!(0), json!(21), json!(2.5), json!("x")] {
        assert_eq!(
            parse_query(&json!({"query": "q", "count": bad})).unwrap_err(),
            text::BAD_COUNT
        );
    }
    assert_eq!(
        parse_query(&json!({"query": "q", "count": "3"})).unwrap().count,
        Some(3)
    );
    for good in ["pd", "pw", "pm", "py", "2022-04-01to2022-07-30"] {
        assert!(
            parse_query(&json!({"query": "q", "freshness": good})).is_ok(),
            "{good}"
        );
    }
    for bad in ["day", "2022-04-01", "2022-4-1to2022-07-30"] {
        assert_eq!(
            parse_query(&json!({"query": "q", "freshness": bad})).unwrap_err(),
            text::BAD_FRESHNESS
        );
    }
    assert_eq!(
        parse_query(&json!({"query": "q", "country": "DE"}))
            .unwrap()
            .country
            .as_deref(),
        Some("de")
    );
    assert_eq!(
        parse_query(&json!({"query": "q", "country": "deu"})).unwrap_err(),
        text::BAD_COUNTRY
    );
    assert_eq!(
        parse_query(&json!({"query": "q", "search_lang": "pt-br"}))
            .unwrap()
            .search_lang
            .as_deref(),
        Some("pt-br")
    );
    assert_eq!(
        parse_query(&json!({"query": "q", "search_lang": "e"})).unwrap_err(),
        text::BAD_LANG
    );
    assert_eq!(
        parse_query(&json!({"query": "q", "site": "x"})).unwrap_err(),
        text::UNKNOWN_ARG.replace("{name}", "site")
    );
}

#[test]
fn bad_arguments_never_reach_the_network() {
    let mock = Mock::start(|_, _| ok(&llm_body(1, "x")));
    let tool = WebSearchTool::new(Arc::new(Brave::new(key(), opts(&mock.url))));
    let cwd = std::env::temp_dir();
    let (sb, cancel) = (NoSandbox, CancelToken::new());
    let cx = ctx(&cwd, &sb, Mode::Workspace, &cancel);
    let out = tool.run(&json!({"query": "a".repeat(700)}), &cx, &mut |_| {});
    assert!(out.is_error);
    assert!(tool.access(&json!({"query": ""}), &cx).is_err());
    assert_eq!(mock.requests().len(), 0);
}

#[test]
fn access_per_mode() {
    let tool = WebSearchTool::new(Arc::new(Brave::new(key(), opts("http://127.0.0.1:9"))));
    let cwd = std::env::temp_dir();
    let (sb, cancel) = (NoSandbox, CancelToken::new());
    let args = json!({"query": "q"});
    // Workspace: no prompt; ask: the host. Plan (sec-prod item 2): the host too, so the policy
    // denies it (plan mode has no network).
    assert_eq!(
        tool.access(&args, &ctx(&cwd, &sb, Mode::Plan, &cancel)).unwrap(),
        vec![Access::Net {
            host: "127.0.0.1".to_string()
        }]
    );
    assert_eq!(
        tool.access(&args, &ctx(&cwd, &sb, Mode::Workspace, &cancel))
            .unwrap(),
        vec![]
    );
    assert_eq!(
        tool.access(&args, &ctx(&cwd, &sb, Mode::Ask, &cancel)).unwrap(),
        vec![Access::Net {
            host: "127.0.0.1".to_string()
        }]
    );
    // Through the real policy table: workspace runs without a prompt, plan is denied (no
    // network, sec-prod item 2), ask asks once and a session grant covers the host.
    use oxa_proto::{Decision, Policy};
    let mut p = oxa_perm::ModePolicy::new();
    let acc = tool
        .access(&args, &ctx(&cwd, &sb, Mode::Workspace, &cancel))
        .unwrap();
    assert!(acc
        .iter()
        .all(|a| p.decide(Mode::Workspace, a, &cwd) == Decision::Allow));
    let plan = tool.access(&args, &ctx(&cwd, &sb, Mode::Plan, &cancel)).unwrap();
    assert!(plan
        .iter()
        .all(|a| matches!(p.decide(Mode::Plan, a, &cwd), Decision::Deny(_))));
    let net = &tool.access(&args, &ctx(&cwd, &sb, Mode::Ask, &cancel)).unwrap()[0];
    assert_eq!(p.decide(Mode::Ask, net, &cwd), Decision::Ask);
    p.remember(net);
    assert_eq!(p.decide(Mode::Ask, net, &cwd), Decision::Allow);
}

#[test]
fn the_spec_is_strings_and_integers_only() {
    let tool = WebSearchTool::new(Arc::new(Brave::new(key(), opts("http://127.0.0.1:9"))));
    let spec = tool.spec();
    assert_eq!(spec.name, "web_search");
    assert_eq!(spec.class, oxa_proto::ToolClass::Net);
    let schema: serde_json::Value = serde_json::from_str(&spec.schema_json).unwrap();
    assert_eq!(schema["required"], json!(["query"]));
    for (_, p) in schema["properties"].as_object().unwrap() {
        assert!(matches!(p["type"].as_str(), Some("string" | "integer")), "{p}");
    }
}
