//! B15 acceptance tests: Invariant checker and golden runner.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::golden::{compare_golden, request_hashes};
use oxa_fake::{
    check_all, check_hints, check_w1, check_w2, check_w3, check_w5, check_w6, Recorded, TestEnv, Violation,
};
use oxa_proto::{codes, ToolCall, WireMsg};
use serde_json::json;

const SYSTEM: &str = "You are oxa.\n\nCurrent working directory: /w";

/// One recorded request of session `s1`. A `prefill` gets `max_tokens` 0 and no reply; summary
/// kinds and prefill are background; prefill keeps its snapshot.
fn req(seq: usize, kind: &str, messages: Vec<WireMsg>, reply: Option<WireMsg>) -> Recorded {
    Recorded {
        seq,
        system: SYSTEM.to_string(),
        tools_json: "[]".to_string(),
        messages,
        model: "fake-model".to_string(),
        max_tokens: if kind == "prefill" { 0 } else { 1024 },
        thinking: false,
        session: "s1".to_string(),
        idem_key: format!("key-{seq}"),
        kind: kind.to_string(),
        background: matches!(kind, "compaction" | "branch_summary" | "prefill" | "subagent"),
        snapshot_keep: kind == "prefill",
        preserve_thinking: Some(true),
        temperature: 0.0,
        headers: Vec::new(),
        body: Vec::new(),
        reply: if kind == "prefill" { None } else { reply },
        usage: None,
    }
}

/// A clean two-request chat session: request 1 extends request 0 with its reply.
fn clean_log() -> Vec<Recorded> {
    let u1 = WireMsg::user("fix the parser");
    let a1 = WireMsg::assistant("looking");
    let u2 = WireMsg::user("thanks");
    let a2 = WireMsg::assistant("done");
    vec![
        req(0, "chat", vec![u1.clone()], Some(a1.clone())),
        req(1, "chat", vec![u1, a1, u2], Some(a2)),
    ]
}

fn rules(v: &[Violation]) -> Vec<&'static str> {
    v.iter().map(|x| x.rule).collect()
}

fn has_rule(v: &[Violation], rule: &str) -> bool {
    v.iter().any(|x| x.rule == rule)
}

/// A valid HTTP body for W6.
fn good_body() -> Vec<u8> {
    json!({
        "model": "fake-model",
        "temperature": 0,
        "cache_prompt": true,
        "chat_template_kwargs": {"preserve_thinking": true, "enable_thinking": true},
    })
    .to_string()
    .into_bytes()
}

/// A mid-run session: a tool call, its result, then a compaction request over the whole wire.
/// Returns the four requests (chat, chat with tool result, compaction, post-compaction head).
fn mid_run_flow(head: Vec<WireMsg>) -> Vec<Recorded> {
    let prompt = WireMsg::user("fix bug");
    let call = WireMsg {
        images: Vec::new(),
        role: oxa_proto::Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args_raw: "{}".into(),
        }],
        provider_history: None,
        tool_call_id: None,
    };
    let tool = WireMsg::tool_result("c1", "file text");
    let a1 = WireMsg::assistant("found it");
    let mut compact_msgs = vec![prompt.clone(), call.clone(), tool.clone(), a1.clone()];
    compact_msgs.push(WireMsg::user("summarize"));
    let mut out = vec![
        req(0, "chat", vec![prompt.clone()], Some(call.clone())),
        req(1, "chat", vec![prompt, call, tool], Some(a1)),
        req(2, "compaction", compact_msgs, Some(WireMsg::assistant("SUMMARY"))),
    ];
    out.push(req(3, "chat", head, Some(WireMsg::assistant("ok"))));
    out
}

// ---------- positive controls ----------

#[test]
fn clean_log_is_green() {
    let log = clean_log();
    assert_eq!(check_all(&log, &[]), Vec::new());
}

/// a mutated earlier message is flagged, clean log is green
#[test]
fn w1_negative_control() {
    let clean = clean_log();
    assert!(check_w1(&clean, &[]).is_empty(), "clean log must be green");

    let mut bad = clean_log();
    bad[1].messages[0].content = Some("fix the lexer".to_string());
    let v = check_w1(&bad, &[]);
    assert_eq!(rules(&v), vec!["W1"]);
    assert_eq!(v[0].seq, 1);
}

/// a trimmed assistant echo is flagged
#[test]
fn w2_negative_control() {
    let mut log = clean_log();
    // The reply went out with trailing spaces; the wire carries it trimmed.
    log[0].reply = Some(WireMsg::assistant("looking  "));
    log[1].messages[1] = WireMsg::assistant("looking");
    let v = check_w2(&log, &[]);
    assert_eq!(rules(&v), vec!["W2"]);
    // The same mismatch also breaks W1 (the previous wire no longer matches).
    assert!(has_rule(&check_w1(&log, &[]), "W1"));
    assert_eq!(v[0].seq, 1);
    assert!(check_w2(&clean_log(), &[]).is_empty());
}

/// a date line in the system text and a mid-epoch tool change are flagged
#[test]
fn w3_negative_control() {
    let mut dated = clean_log();
    dated[0].system = "Today is 2026-11-13.\n\nCurrent working directory: /w".to_string();
    let v = check_w3(&dated, &[]);
    assert!(has_rule(&v, "W3"), "date line must be flagged: {v:?}");

    let mut changed = clean_log();
    changed[1].tools_json = r#"[{"name":"read"}]"#.to_string();
    let v = check_w3(&changed, &[]);
    assert_eq!(rules(&v), vec!["W3"]);
    assert_eq!(v[0].seq, 1);

    // The same tool change is allowed at a marked epoch.
    assert!(check_w3(&changed, &[(1, "epoch")]).is_empty());
    assert!(check_w3(&clean_log(), &[]).is_empty());
}

#[test]
fn w3_month_git_mode_and_cwd_lines() {
    for system in [
        "Date: March 3, 2026\n\nCurrent working directory: /w",
        "Git status: clean\n\nCurrent working directory: /w",
        "Mode: workspace\n\nCurrent working directory: /w",
        "Current working directory: /w\n\nBe concise.",
    ] {
        let mut log = clean_log();
        log[0].system = system.to_string();
        assert!(has_rule(&check_w3(&log, &[]), "W3"), "must flag: {system}");
    }
    // Month names without a year are not dates.
    let mut log = clean_log();
    for r in &mut log {
        r.system = "Work on March tasks.\n\nCurrent working directory: /w".to_string();
    }
    assert!(check_w3(&log, &[]).is_empty());
}

/// a folded prompt and a non-append summary request are flagged
#[test]
fn w5_negative_control() {
    // Positive: the summary request appends one instruction, the head keeps the prompt verbatim.
    let ok = mid_run_flow(vec![WireMsg::user("SUMMARY"), WireMsg::user("fix bug")]);
    let rebases = [(3, "compaction")];
    assert_eq!(
        check_all(&ok, &rebases),
        Vec::new(),
        "clean compaction flow must be green"
    );

    // Folded prompt: the head carries the summary only, the mid-run prompt is gone.
    let folded = mid_run_flow(vec![WireMsg::user("SUMMARY, fixing the bug")]);
    let v = check_w5(&folded, &rebases);
    assert_eq!(rules(&v), vec!["W5"], "folded prompt must be flagged: {v:?}");
    assert_eq!(v[0].seq, 3);

    // Non-append summary request: the tool result is dropped from the request.
    let mut shrunk = mid_run_flow(vec![WireMsg::user("SUMMARY"), WireMsg::user("fix bug")]);
    shrunk[2].messages = vec![WireMsg::user("fix bug"), WireMsg::user("summarize")];
    let v = check_w5(&shrunk, &[]);
    assert!(
        has_rule(&v, "W5"),
        "non-append summary request must be flagged: {v:?}"
    );

    // A compaction must run without thinking. Its priority is not W5 (F12: only the
    // threshold summary is background; `/compact` and overflow recovery are foreground).
    let mut thinking = mid_run_flow(vec![WireMsg::user("SUMMARY"), WireMsg::user("fix bug")]);
    thinking[2].thinking = true;
    thinking[2].background = false;
    let v = check_w5(&thinking, &rebases);
    assert_eq!(v.iter().filter(|x| x.seq == 2).count(), 1, "{v:?}");
}

/// missing cache_prompt / temperature is flagged
#[test]
fn w6_negative_control() {
    let mut log = clean_log();
    log[0].body = good_body();
    assert!(check_w6(&log).is_empty(), "valid body must be green");

    let mut no_cache = clean_log();
    no_cache[0].body = json!({
        "temperature": 0,
        "chat_template_kwargs": {"preserve_thinking": true, "enable_thinking": false},
    })
    .to_string()
    .into_bytes();
    let v = check_w6(&no_cache);
    assert_eq!(rules(&v), vec!["W6"]);
    assert!(v[0].detail.contains("cache_prompt"));

    let mut warm = clean_log();
    warm[1].body = json!({
        "temperature": 0.7,
        "cache_prompt": true,
        "chat_template_kwargs": {"preserve_thinking": true, "enable_thinking": true},
    })
    .to_string()
    .into_bytes();
    let v = check_w6(&warm);
    assert_eq!(rules(&v), vec!["W6"]);
    assert_eq!(v[0].seq, 1);

    // Scripted records have no body and are skipped.
    assert!(check_w6(&clean_log()).is_empty());
}

/// background on chat, missing session header
#[test]
fn hints_negative_control() {
    let mut bg_chat = clean_log();
    bg_chat[1].background = true;
    let v = check_hints(&bg_chat);
    assert_eq!(rules(&v), vec!["H3"]);
    assert_eq!(v[0].seq, 1);

    let mut no_session = clean_log();
    no_session[0].session = String::new();
    let v = check_hints(&no_session);
    assert_eq!(rules(&v), vec!["H1"]);

    assert!(check_hints(&clean_log()).is_empty());
}

#[test]
fn hints_rules_h2_h4_h5_h6() {
    // H4 + H6: a prefill without snapshot keep and with a decode budget.
    let mut pre = clean_log();
    pre.push(req(2, "prefill", vec![WireMsg::user("x")], None));
    pre[2].snapshot_keep = false;
    pre[2].max_tokens = 1024;
    let v = check_hints(&pre);
    assert!(has_rule(&v, "H4") && has_rule(&v, "H6"), "{v:?}");

    // H5: kind missing or unknown.
    let mut kinds = clean_log();
    kinds[0].kind = String::new();
    kinds[1].kind = "mystery".to_string();
    let v = check_hints(&kinds);
    assert_eq!(rules(&v), vec!["H5", "H5"]);

    // H2: the same key on two different bodies.
    let mut keys = clean_log();
    keys[1].idem_key = keys[0].idem_key.clone();
    keys[0].body = b"{\"a\":1}".to_vec();
    keys[1].body = b"{\"a\":2}".to_vec();
    let v = check_hints(&keys);
    assert_eq!(rules(&v), vec!["H2"]);
    assert_eq!(v[0].seq, 1);
}

/// marked rebase is not a W1 violation
#[test]
fn rebase_marks_allow_change() {
    let u1 = WireMsg::user("first");
    let a1 = WireMsg::assistant("one");
    let u2 = WireMsg::user("second");
    let a2 = WireMsg::assistant("two");
    let log = vec![
        req(0, "chat", vec![u1.clone()], Some(a1.clone())),
        req(1, "chat", vec![u1.clone(), a1.clone(), u2.clone()], Some(a2)),
        // A leaf move back to the first prompt: the wire changes shape.
        req(
            2,
            "chat",
            vec![u1, WireMsg::user("another branch")],
            Some(WireMsg::assistant("three")),
        ),
    ];
    let v = check_w1(&log, &[]);
    assert_eq!(rules(&v), vec!["W1"]);
    assert_eq!(v[0].seq, 2);
    assert!(check_w1(&log, &[(2, "leaf_move")]).is_empty());
    assert!(check_all(&log, &[(2, "leaf_move")]).is_empty());
}

/// Replies that failed or were cut never appear on the wire.
#[test]
fn failed_reply_never_appears() {
    let u1 = WireMsg::user("hi");
    let mut log = vec![req(0, "chat", vec![u1.clone()], None)];
    log.push(req(
        1,
        "chat",
        vec![u1, WireMsg::assistant("sent after a failure")],
        Some(WireMsg::assistant("ok")),
    ));
    let v = check_w2(&log, &[]);
    assert_eq!(rules(&v), vec!["W2"]);
}

/// Sessions resumed in the test declare their imported messages with a marked first rebase.
#[test]
fn imported_session_is_declared() {
    let u0 = WireMsg::user("earlier");
    let old = WireMsg::assistant("imported reply");
    let u1 = WireMsg::user("now");
    let new = WireMsg::assistant("new reply");
    let log = vec![
        req(
            0,
            "chat",
            vec![u0.clone(), old.clone()],
            Some(WireMsg::assistant("answer")),
        ),
        req(
            1,
            "chat",
            vec![u0, old, WireMsg::assistant("answer"), WireMsg::user("later"), u1],
            Some(new),
        ),
    ];
    assert!(check_w2(&log, &[(0, "resume")]).is_empty());
    assert!(check_w1(&log, &[(0, "resume")]).is_empty());
    // Without the declaration the imported message is not a reply of this engine.
    assert!(has_rule(&check_w2(&log, &[]), "W2"));
}

/// session id and idem key do not change hashes
#[test]
fn hashes_exclude_random_ids() {
    let a = clean_log();
    let mut b = clean_log();
    for r in &mut b {
        r.session = "another-session".to_string();
        r.idem_key = "zzz".to_string();
    }
    assert_eq!(request_hashes(&a), request_hashes(&b));

    // The hashed fields do change the hash.
    let mut c = clean_log();
    c[1].kind = "subagent".to_string();
    assert_ne!(request_hashes(&a)[1], request_hashes(&c)[1]);
    assert_eq!(request_hashes(&a)[0].len(), 64);
}

/// compare and rewrite
#[test]
fn golden_hash_stable_and_update() {
    let env = TestEnv::new();
    let path = env.work().join("golden/requests.txt");
    let hashes = request_hashes(&clean_log());
    assert_eq!(hashes, request_hashes(&clean_log()), "hashes are deterministic");

    // Missing golden file: an error, not a silent pass.
    assert!(compare_golden(&path, &hashes, false).is_err());

    compare_golden(&path, &hashes, true).expect("rewrite golden");
    compare_golden(&path, &hashes, false).expect("golden matches");

    let mut changed = hashes.clone();
    changed[1] = "f".repeat(64);
    let err = compare_golden(&path, &changed, false).expect_err("mismatch must fail");
    assert_eq!(err.code, codes::INVALID);
    assert!(err.message.contains("line 2"), "{}", err.message);
    assert!(
        err.message.contains(&hashes[1]) && err.message.contains(&changed[1]),
        "{}",
        err.message
    );

    compare_golden(&path, &changed, true).expect("rewrite golden");
    compare_golden(&path, &changed, false).expect("golden matches after update");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2);
}
