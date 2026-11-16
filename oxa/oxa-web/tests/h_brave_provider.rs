//! Lane `search` (O-169 / O-170): the Brave provider against a loopback mock. Both endpoints,
//! the cached fallback with its one notice, retries, errors, serialisation and cancel.
mod common;

use common::*;
use oxa_proto::{tool_errors, CancelToken, Mode, Tool};
use oxa_tools::descriptions::web_search as text;
use oxa_web::{Brave, BraveOpts, Endpoint, Query, WebSearchTool, FALLBACK_NOTICE};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const LLM: &str = "/res/v1/llm/context";
const WEB: &str = "/res/v1/web/search";

fn q(s: &str) -> Query {
    Query {
        q: s.to_string(),
        ..Query::default()
    }
}

#[test]
fn llm_context_is_the_primary_endpoint_with_the_documented_request() {
    let mock = Mock::start(|_, _| ok(&llm_body(2, "hello")));
    let brave = Brave::new(key(), opts(&mock.url));
    let query = Query {
        q: "rust  async".to_string(),
        count: Some(5),
        freshness: Some("pw".to_string()),
        country: Some("de".to_string()),
        search_lang: Some("en".to_string()),
    };
    let found = brave.search(&query, &CancelToken::new()).expect("search");
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].url, "https://example.com/0");
    assert_eq!(found[0].title, "Title 0");
    assert_eq!(found[0].age.as_deref(), Some("2024-05-06"));
    assert_eq!(found[0].snippets, vec!["hello 0", "second 0"]);
    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    let r = &seen[0];
    assert_eq!(r.path, LLM);
    assert_eq!(r.header("x-subscription-token").as_deref(), Some(FAKE_KEY));
    assert_eq!(r.header("accept").as_deref(), Some("application/json"));
    assert_eq!(r.param("q").as_deref(), Some("rust  async"));
    assert_eq!(r.param("maximum_number_of_tokens").as_deref(), Some("2048"));
    assert_eq!(r.param("maximum_number_of_urls").as_deref(), Some("5"));
    assert_eq!(r.param("freshness").as_deref(), Some("pw"));
    assert_eq!(r.param("country").as_deref(), Some("de"));
    assert_eq!(r.param("search_lang").as_deref(), Some("en"));
    assert_eq!(brave.shared().endpoint(), Endpoint::LlmContext);
    assert_eq!(brave.take_notice(), None);
}

#[test]
fn max_tokens_is_clamped_to_the_api_range() {
    let mock = Mock::start(|_, _| ok(&llm_body(1, "x")));
    for (asked, sent) in [(10u32, "1024"), (99_999, "32768"), (4096, "4096")] {
        let brave = Brave::new(
            key(),
            BraveOpts {
                max_tokens: asked,
                ..opts(&mock.url)
            },
        );
        brave.search(&q("x"), &CancelToken::new()).unwrap();
        let last = mock.requests().pop().unwrap();
        assert_eq!(last.param("maximum_number_of_tokens").as_deref(), Some(sent));
    }
}

#[test]
fn option_not_in_plan_falls_back_once_and_for_good_with_one_notice() {
    let mock = Mock::start(|s, _| {
        if s.path == LLM {
            status(400, NOT_IN_PLAN)
        } else {
            ok(&web_body(3))
        }
    });
    let shared = oxa_web::BraveShared::new();
    // Two provider instances (two epochs) share the decision.
    let first = Brave::with_shared(key(), opts(&mock.url), shared.clone());
    let found = first
        .search(&q("one"), &CancelToken::new())
        .expect("fallback search");
    assert_eq!(found.len(), 3);
    assert_eq!(found[0].title, "Result 0");
    assert_eq!(found[0].snippets, vec!["Desc & bold 0", "extra 0"]);
    assert_eq!(found[0].age.as_deref(), Some("2024-05-06"));
    assert_eq!(first.take_notice().as_deref(), Some(FALLBACK_NOTICE));
    assert_eq!(first.take_notice(), None, "the notice is given once");
    let second = Brave::with_shared(key(), opts(&mock.url), shared.clone());
    for word in ["two", "three", "four"] {
        second.search(&q(word), &CancelToken::new()).expect("web search");
    }
    assert_eq!(second.take_notice(), None);
    assert_eq!(mock.hits(LLM), 1, "LLM Context is asked exactly once");
    assert_eq!(mock.hits(WEB), 4);
    assert_eq!(shared.endpoint(), Endpoint::WebSearch);
    let web = mock.requests().into_iter().find(|s| s.path == WEB).unwrap();
    assert_eq!(web.param("count").as_deref(), Some("10"));
    assert_eq!(web.param("maximum_number_of_tokens"), None);
    assert_eq!(web.header("x-subscription-token").as_deref(), Some(FAKE_KEY));
}

#[test]
fn another_400_is_an_error_and_does_not_fall_back() {
    let mock = Mock::start(|_, _| {
        status(
            400,
            r#"{"error":{"id":"abc-123","code":"VALIDATION","detail":"Unable to validate request parameter(s)."}}"#,
        )
    });
    let brave = Brave::new(key(), opts(&mock.url));
    let err = brave.search(&q("x"), &CancelToken::new()).unwrap_err();
    assert_eq!(
        err,
        "web_search failed: Brave Search answered HTTP 400 (Unable to validate request parameter(s).)\nFix: check the query and the optional parameters"
    );
    assert!(!err.contains("abc-123"), "no request ids");
    assert_eq!(brave.shared().endpoint(), Endpoint::LlmContext);
    assert_eq!(mock.hits(WEB), 0);
}

#[test]
fn rate_limit_waits_for_retry_after_then_succeeds() {
    let mock = Mock::start(|_, n| {
        if n == 0 {
            let mut c = status(429, r#"{"error":{"code":"RATE_LIMITED"}}"#);
            c.headers.push(("Retry-After".to_string(), "1".to_string()));
            c
        } else {
            ok(&llm_body(1, "after"))
        }
    });
    let brave = Brave::new(key(), opts(&mock.url));
    let t = Instant::now();
    let found = brave.search(&q("x"), &CancelToken::new()).expect("retried");
    assert!(
        t.elapsed() >= Duration::from_millis(950),
        "Retry-After honoured: {:?}",
        t.elapsed()
    );
    assert_eq!(found[0].snippets[0], "after 0");
    assert_eq!(mock.hits(LLM), 2);
}

#[test]
fn rate_limit_reset_header_is_honoured_too() {
    let mock = Mock::start(|_, n| {
        if n == 0 {
            let mut c = status(429, "{}");
            c.headers
                .push(("X-RateLimit-Reset".to_string(), "1, 1419704".to_string()));
            c
        } else {
            ok(&llm_body(1, "ok"))
        }
    });
    let brave = Brave::new(key(), opts(&mock.url));
    let t = Instant::now();
    brave.search(&q("x"), &CancelToken::new()).expect("retried");
    assert!(t.elapsed() >= Duration::from_millis(950));
}

#[test]
fn server_errors_are_retried_three_times_then_reported() {
    let mock = Mock::start(|_, _| status(503, "busy"));
    let brave = Brave::new(key(), opts(&mock.url));
    let err = brave.search(&q("x"), &CancelToken::new()).unwrap_err();
    assert_eq!(err, text::SERVER_ERROR.replace("{status}", "503"));
    assert_eq!(mock.hits(LLM), 3, "max 3 attempts");
    let mock = Mock::start(|_, _| status(429, "{}"));
    let brave = Brave::new(key(), opts(&mock.url));
    assert_eq!(
        brave.search(&q("x"), &CancelToken::new()).unwrap_err(),
        text::RATE_LIMITED
    );
    assert_eq!(mock.hits(LLM), 3);
}

#[test]
fn unauthorized_names_both_key_locations_and_never_the_key() {
    for code in [401u16, 403] {
        let mock =
            Mock::start(move |_, _| status(code, r#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID"}}"#));
        let brave = Brave::new(key(), opts(&mock.url));
        let err = brave.search(&q("x"), &CancelToken::new()).unwrap_err();
        assert_eq!(err, text::KEY_INVALID);
        assert!(err.contains("BRAVE_SEARCH_API_KEY") && err.contains("brave-search.key"));
        assert!(!err.contains(FAKE_KEY));
        assert_eq!(mock.hits(LLM), 1, "a key error is not retried");
    }
}

#[test]
fn a_slow_server_times_out_with_a_fixed_text() {
    let mock = Mock::start(|_, _| {
        let mut c = ok(&llm_body(1, "late"));
        c.delay = Duration::from_millis(1500);
        c
    });
    let brave = Brave::new(
        key(),
        BraveOpts {
            timeout: Duration::from_millis(300),
            ..opts(&mock.url)
        },
    );
    assert_eq!(
        brave.search(&q("x"), &CancelToken::new()).unwrap_err(),
        text::TIMEOUT
    );
}

#[test]
fn malformed_json_is_a_fixed_error() {
    for body in ["not json", "[1,2]", "{\"grounding\": "] {
        let b = body.to_string();
        let mock = Mock::start(move |_, _| ok(&b));
        let brave = Brave::new(key(), opts(&mock.url));
        assert_eq!(
            brave.search(&q("x"), &CancelToken::new()).unwrap_err(),
            text::MALFORMED,
            "{body}"
        );
    }
    // A valid object without results is "no results", not an error.
    let mock = Mock::start(|_, _| ok(r#"{"grounding":{"generic":[]},"sources":{}}"#));
    let brave = Brave::new(key(), opts(&mock.url));
    assert!(brave.search(&q("x"), &CancelToken::new()).unwrap().is_empty());
}

#[test]
fn unreachable_host_is_a_fixed_error() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let brave = Brave::new(key(), opts(&format!("http://127.0.0.1:{port}")));
    assert_eq!(
        brave.search(&q("x"), &CancelToken::new()).unwrap_err(),
        text::UNREACHABLE
    );
}

#[test]
fn cancel_ends_a_request_and_a_backoff_at_once() {
    let mock = Mock::start(|_, _| {
        let mut c = ok(&llm_body(1, "late"));
        c.delay = Duration::from_secs(3);
        c
    });
    let brave = Arc::new(Brave::new(key(), opts(&mock.url)));
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        c2.cancel();
    });
    let t = Instant::now();
    assert_eq!(brave.search(&q("x"), &cancel).unwrap_err(), tool_errors::ABORTED);
    assert!(t.elapsed() < Duration::from_secs(2));

    let mock = Mock::start(|_, _| {
        let mut c = status(429, "{}");
        c.headers.push(("Retry-After".to_string(), "2".to_string()));
        c
    });
    let brave = Brave::new(key(), opts(&mock.url));
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        c2.cancel();
    });
    let t = Instant::now();
    assert_eq!(brave.search(&q("x"), &cancel).unwrap_err(), tool_errors::ABORTED);
    assert!(t.elapsed() < Duration::from_millis(1500));
}

#[test]
fn parallel_calls_are_serialised() {
    let mock = Mock::start(|_, _| {
        let mut c = ok(&llm_body(1, "x"));
        c.delay = Duration::from_millis(100);
        c
    });
    let shared = oxa_web::BraveShared::new();
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let brave = Brave::with_shared(key(), opts(&mock.url), shared.clone());
            std::thread::spawn(move || brave.search(&q(&format!("q{i}")), &CancelToken::new()).is_ok())
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap());
    }
    assert_eq!(mock.max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(shared.requests(), 4);
}

#[test]
fn isolation_refuses_the_real_host() {
    let brave = Brave::new(
        key(),
        BraveOpts {
            isolation: true,
            ..BraveOpts::default()
        },
    );
    assert_eq!(
        brave.search(&q("x"), &CancelToken::new()).unwrap_err(),
        text::TEST_REAL_HOST
    );
    assert_eq!(brave.shared().requests(), 0);
}

#[test]
fn the_tool_shapes_both_endpoints_the_same_way_and_notifies_once() {
    let mock = Mock::start(|s, _| {
        if s.path == LLM {
            status(400, NOT_IN_PLAN)
        } else {
            ok(&web_body(2))
        }
    });
    let tool = WebSearchTool::new(Arc::new(Brave::new(key(), opts(&mock.url))));
    let cwd = std::env::temp_dir();
    let (sb, cancel) = (NoSandbox, CancelToken::new());
    let cx = ctx(&cwd, &sb, Mode::Workspace, &cancel);
    let mut ui = oxa_proto::HeadlessUi::default();
    let out = tool.run_with_ui(&json!({"query": "rust"}), &cx, &mut |_| {}, &mut ui);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        out.text,
        "Web search results for: rust\n\n[1] Result 0\nhttps://example.org/0\nPublished: 2024-05-06\nDesc & bold 0\nextra 0\n\n[2] Result 1\nhttps://example.org/1\nPublished: 2024-05-06\nDesc & bold 1\nextra 1"
    );
    assert_eq!(ui.notes.len(), 1);
    assert_eq!(ui.notes[0].1, FALLBACK_NOTICE);
    tool.run_with_ui(&json!({"query": "again"}), &cx, &mut |_| {}, &mut ui);
    assert_eq!(ui.notes.len(), 1, "one notice per process");

    let mock = Mock::start(|_, _| ok(&llm_body(2, "snip")));
    let tool = WebSearchTool::new(Arc::new(Brave::new(key(), opts(&mock.url))));
    let out = tool.run(&json!({"query": "rust"}), &cx, &mut |_| {});
    assert_eq!(
        out.text,
        "Web search results for: rust\n\n[1] Title 0\nhttps://example.com/0\nPublished: 2024-05-06\nsnip 0\nsecond 0\n\n[2] Title 1\nhttps://example.com/1\nPublished: 2024-05-06\nsnip 1\nsecond 1"
    );
    // Deterministic: the same answer gives the same bytes.
    assert_eq!(
        out.text,
        tool.run(&json!({"query": "rust"}), &cx, &mut |_| {}).text
    );
}
