//! Synthetic private replay persistence and invalidation (#723).
use oxa_fake::TestEnv;
use oxa_proto::{
    rebase_why, Entry, EntryBody, FrozenPrefix, ProviderHistory, Role, WireMsg, MAX_PROVIDER_HISTORY_BYTES,
    MAX_PROVIDER_HISTORY_ITEMS,
};
use oxa_session::{decode_entry, encode_entry, FsStore, SessionRef, SessionStore, Transcript};
use serde_json::{json, Value};
use std::{fs, io::Write, process::Command};

fn history() -> ProviderHistory {
    ProviderHistory::new([17;32], [29;32], vec![
        json!({"type":"reasoning","encrypted_content":"synthetic-private-replay"}),
        json!({"type":"function_call","call_id":"synthetic-call","name":"read","arguments":"{ \"path\" : \"note.txt\" }"})]).unwrap()
}
fn assistant() -> WireMsg {
    let mut m = WireMsg::user("visible answer");
    m.role = Role::Assistant;
    m.provider_history = Some(history());
    m
}
fn entry(id: &str, parent: Option<&str>, wire: WireMsg) -> Entry {
    Entry {
        id: id.into(),
        parent: parent.map(str::to_owned),
        ts_ms: 1000,
        body: EntryBody::Msg {
            wire,
            parts: vec![],
            tool: None,
        },
    }
}
fn encoded(e: &Entry) -> Value {
    let mut b = vec![];
    encode_entry(e, &mut b);
    serde_json::from_slice(&b).unwrap()
}
fn prefix(text: &str) -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("system".into(), text.into())],
        tools_json: "[]".into(),
        sha256: [0; 32],
    }
}

#[test]
fn private_codec_round_trip_is_exact_and_absence_stays_absent() {
    for wire in [assistant(), WireMsg::user("plain")] {
        let e = entry("a", None, wire);
        let value = encoded(&e);
        let actual = decode_entry(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(actual, e);
        if let EntryBody::Msg { wire, .. } = actual.body {
            assert_eq!(
                value["wire"].get("provider_history").is_some(),
                wire.provider_history.is_some()
            );
        }
    }
}

#[test]
fn append_resume_in_a_fresh_process_preserves_all_private_items() {
    if let Some(path) = std::env::var_os("OXA_TEST_REPLAY_SESSION") {
        let store = FsStore::new(std::path::PathBuf::from(&path).parent().unwrap().to_path_buf());
        let entries = store
            .load(&SessionRef {
                path: path.into(),
                id: "synthetic".into(),
            })
            .unwrap();
        assert_eq!(entries, vec![entry("a", None, assistant())]);
        return;
    }
    let env = TestEnv::new();
    let store = FsStore::new(env.root().join("sessions"));
    let s = store.create(&env.work(), Some("synthetic"), None).unwrap();
    store.append(&s, &entry("a", None, assistant())).unwrap();
    store.sync(&s).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "append_resume_in_a_fresh_process_preserves_all_private_items",
        ])
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .env("OXA_TEST_REPLAY_SESSION", &s.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn invalid_private_state_is_not_silently_discarded_on_decode_or_load() {
    let good = history().to_session_value();
    let mut future = good.clone();
    future["v"] = json!(2);
    let mut too_many = good.clone();
    too_many["items"] = json!(vec![json!({}); MAX_PROVIDER_HISTORY_ITEMS + 1]);
    let mut too_large = good.clone();
    too_large["items"] = json!([{"text":"x".repeat(MAX_PROVIDER_HISTORY_BYTES)}]);
    for bad in [Value::Null, json!({}), future, too_many, too_large] {
        let mut value = encoded(&entry("a", None, assistant()));
        value["wire"]["provider_history"] = bad;
        let line = serde_json::to_vec(&value).unwrap();
        assert!(decode_entry(&line).is_err());
        let env = TestEnv::new();
        let store = FsStore::new(env.root().join("sessions"));
        let s = store.create(&env.work(), None, None).unwrap();
        let mut f = fs::OpenOptions::new().append(true).open(&s.path).unwrap();
        f.write_all(&line).unwrap();
        f.write_all(b"\n").unwrap();
        assert!(
            store.load(&s).is_err(),
            "complete malformed private state must not be skipped"
        );
    }
}

#[test]
fn rebase_preserves_only_the_unchanged_visible_prefix_and_original_authority() {
    let user = WireMsg::user("question");
    let reply = assistant();
    let mut t = Transcript::default();
    t.push(user.clone());
    t.push(reply.clone());
    t.rebase(
        vec![user.clone(), reply.clone(), WireMsg::user("next")],
        rebase_why::EXTENSION,
    );
    assert_eq!(t.wire()[1].provider_history, Some(history()));
    let mut rewritten = reply.clone();
    rewritten.content = Some("changed".into());
    t.rebase(vec![user.clone(), rewritten], rebase_why::EXTENSION);
    assert!(t.wire()[1].provider_history.is_none());
    t.rebase(vec![user, reply], rebase_why::EXTENSION);
    assert!(
        t.wire()[1].provider_history.is_none(),
        "rebase cannot install supplied authority"
    );
}

#[test]
fn prefix_epoch_and_compaction_invalidate_changed_history() {
    let mut t = Transcript::default();
    t.apply(&Entry {
        id: "p".into(),
        parent: None,
        ts_ms: 0,
        body: EntryBody::Prefix(prefix("old")),
    });
    t.apply(&entry("a", Some("p"), assistant()));
    t.apply(&Entry {
        id: "p2".into(),
        parent: Some("a".into()),
        ts_ms: 0,
        body: EntryBody::Prefix(prefix("new")),
    });
    assert!(t.wire()[0].provider_history.is_none());
    let mut t = Transcript::default();
    t.apply(&entry("u", None, WireMsg::user("question")));
    t.apply(&entry("a", Some("u"), assistant()));
    t.apply(&Entry {
        id: "c".into(),
        parent: Some("a".into()),
        ts_ms: 0,
        body: EntryBody::Compaction {
            summary: "summary".into(),
            first_kept: "a".into(),
            tokens_before: 99,
            details: Value::Null,
            from_hook: false,
        },
    });
    assert!(t.wire()[1].provider_history.is_none());
}

#[test]
fn compaction_rejects_open_and_split_tool_exchanges_before_writing() {
    for cut in ["tool", ""] {
        let env = TestEnv::new();
        let store = FsStore::new(env.root().join("sessions"));
        let s = store.create(&env.work(), None, None).unwrap();
        let mut call = assistant();
        call.tool_calls.push(oxa_proto::ToolCall {
            id: "c".into(),
            name: "read".into(),
            args_raw: "{}".into(),
        });
        store.append(&s, &entry("call", None, call)).unwrap();
        if !cut.is_empty() {
            store
                .append(
                    &s,
                    &entry("tool", Some("call"), WireMsg::tool_result("c", "done")),
                )
                .unwrap();
        }
        let before = fs::read(&s.path).unwrap();
        let c = Entry {
            id: "compact".into(),
            parent: Some(if cut.is_empty() { "call" } else { "tool" }.into()),
            ts_ms: 1000,
            body: EntryBody::Compaction {
                summary: "summary".into(),
                first_kept: cut.into(),
                tokens_before: 1,
                details: Value::Null,
                from_hook: false,
            },
        };
        assert!(
            store.append(&s, &c).is_err(),
            "open or split exchanges must refuse compaction"
        );
        assert_eq!(fs::read(&s.path).unwrap(), before);
    }
}

#[test]
fn visible_entry_rebase_and_debug_never_disclose_replay() {
    let e = entry("a", None, assistant());
    let mut visible = vec![];
    oxa_session::encode_visible_entry(&e, &mut visible);
    for text in [
        String::from_utf8(visible).unwrap(),
        Transcript::encode_rebase(&[assistant()]).to_string(),
        format!("{e:?}"),
    ] {
        assert!(!text.contains("synthetic-private-replay"));
        assert!(!text.contains("encrypted_content"));
    }
    assert!(!String::from_utf8_lossy(&assistant().to_json()).contains("synthetic-private-replay"));
}

#[test]
fn complete_closed_tool_batch_compacts_and_resumes_without_private_rebinding() {
    let env = TestEnv::new();
    let store = FsStore::new(env.root().join("sessions"));
    let s = store.create(&env.work(), None, None).unwrap();
    let mut call = assistant();
    call.tool_calls.push(oxa_proto::ToolCall {
        id: "c".into(),
        name: "read".into(),
        args_raw: "{}".into(),
    });
    let entries = vec![
        entry("old", None, WireMsg::user("earlier")),
        entry("call", Some("old"), call),
        entry("tool", Some("call"), WireMsg::tool_result("c", "done")),
        Entry {
            id: "compact".into(),
            parent: Some("tool".into()),
            ts_ms: 1000,
            body: EntryBody::Compaction {
                summary: "summary".into(),
                first_kept: "call".into(),
                tokens_before: 1,
                details: Value::Null,
                from_hook: false,
            },
        },
    ];
    store.append_many(&s, &entries).unwrap();
    let loaded = store.load(&s).unwrap();
    let t = Transcript::project(&loaded);
    assert_eq!(t.wire().len(), 3);
    assert!(t.wire().iter().all(|m| m.provider_history.is_none()));
    assert_eq!(t.wire()[1].tool_calls[0].id, "c");
    assert_eq!(t.wire()[2].tool_call_id.as_deref(), Some("c"));
}

#[test]
fn malformed_nested_replay_is_an_error_and_valid_hook_json_has_no_authority() {
    for compact in [false, true] {
        let body = if compact {
            EntryBody::Compaction {
                summary: "summary".into(),
                first_kept: String::new(),
                tokens_before: 1,
                details: json!({"keptMessages":[{"role":"assistant","content":"visible","provider_history":null}]}),
                from_hook: false,
            }
        } else {
            EntryBody::Custom {
                custom_type: Transcript::EXTENSION_REBASE_ENTRY.into(),
                data: json!({"messages":[{"role":"assistant","content":"visible","provider_history":null}]}),
            }
        };
        let e = Entry {
            id: "c".into(),
            parent: None,
            ts_ms: 1000,
            body,
        };
        assert!(decode_entry(&serde_json::to_vec(&encoded(&e)).unwrap()).is_err());
    }
}

#[test]
fn malformed_persisted_compaction_cannot_resume_half_a_tool_batch() {
    let env = TestEnv::new();
    let store = FsStore::new(env.root().join("sessions"));
    let s = store.create(&env.work(), None, None).unwrap();
    let mut call = assistant();
    call.tool_calls.push(oxa_proto::ToolCall {
        id: "c".into(),
        name: "read".into(),
        args_raw: "{}".into(),
    });
    store
        .append_many(
            &s,
            &[
                entry("call", None, call),
                entry("tool", Some("call"), WireMsg::tool_result("c", "done")),
            ],
        )
        .unwrap();
    let c = Entry {
        id: "compact".into(),
        parent: Some("tool".into()),
        ts_ms: 1000,
        body: EntryBody::Compaction {
            summary: "summary".into(),
            first_kept: "tool".into(),
            tokens_before: 1,
            details: Value::Null,
            from_hook: false,
        },
    };
    let mut line = vec![];
    encode_entry(&c, &mut line);
    line.push(b'\n');
    fs::OpenOptions::new()
        .append(true)
        .open(&s.path)
        .unwrap()
        .write_all(&line)
        .unwrap();
    assert!(store.load(&s).is_err());
}

#[test]
fn unchanged_epoch_and_visible_prefix_keep_the_original_shared_carrier() {
    let mut t = Transcript::default();
    let p = Entry {
        id: "p".into(),
        parent: None,
        ts_ms: 0,
        body: EntryBody::Prefix(prefix("same")),
    };
    t.apply(&p);
    t.push(assistant());
    t.apply(&p);
    let retained = t.wire()[0].provider_history.as_ref().unwrap();
    let clone = retained.clone();
    assert!(std::ptr::eq(retained.items().as_ptr(), clone.items().as_ptr()));
    let mut supplied = t.wire().to_vec();
    supplied[0].provider_history = Some(ProviderHistory::new([99; 32], [98; 32], vec![]).unwrap());
    t.rebase(supplied, rebase_why::EXTENSION);
    assert_eq!(
        t.wire()[0].provider_history,
        Some(history()),
        "supplied tags never replace existing authority"
    );
}

#[test]
fn changed_earlier_message_invalidates_every_later_carrier() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("old question"));
    t.push(assistant());
    t.rebase(
        vec![WireMsg::user("new question"), assistant()],
        rebase_why::CONTEXT_EDIT,
    );
    assert!(t.wire()[1].provider_history.is_none());
}

#[test]
fn raw_wire_rebase_cannot_gain_destination_authority_from_a_leaf_move_label() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("outgoing"));
    t.rebase(
        vec![WireMsg::user("destination"), assistant()],
        rebase_why::LEAF_MOVE,
    );
    assert!(t.wire()[1].provider_history.is_none());
}

#[test]
fn persisted_restore_respects_the_destinations_existing_prefix_invalidation() {
    let entries = vec![
        entry("u", None, WireMsg::user("original")),
        entry("a", Some("u"), assistant()),
        Entry {
            id: "edit".into(),
            parent: Some("a".into()),
            ts_ms: 1000,
            body: EntryBody::ContextEdit {
                target: "u".into(),
                replacement: Some("changed".into()),
            },
        },
    ];
    let destination = Transcript::project(&entries);
    assert!(destination.wire()[1].provider_history.is_none());
    let mut t = Transcript::default();
    t.push(WireMsg::user("unrelated"));
    t.push(assistant());
    t.restore_persisted(destination.clone());
    assert_eq!(t.wire(), destination.wire());
    assert_eq!(t.rebases().last().unwrap().why, rebase_why::LEAF_MOVE);
    assert!(t.wire()[1].provider_history.is_none());
}

#[test]
fn model_change_invalidates_authority_even_when_the_prefix_is_unchanged() {
    let entries = vec![
        Entry {
            id: "p".into(),
            parent: None,
            ts_ms: 0,
            body: EntryBody::Prefix(prefix("same")),
        },
        entry("a", Some("p"), assistant()),
        Entry {
            id: "m".into(),
            parent: Some("a".into()),
            ts_ms: 0,
            body: EntryBody::ModelChange("other-route/model".into()),
        },
        Entry {
            id: "p2".into(),
            parent: Some("m".into()),
            ts_ms: 0,
            body: EntryBody::Prefix(prefix("same")),
        },
    ];
    assert!(Transcript::project(&entries[..2]).wire()[0]
        .provider_history
        .is_some());
    assert!(
        Transcript::project(&entries).wire()[0].provider_history.is_none(),
        "a model/route switch cannot carry old route authority through an identical prefix epoch"
    );
}
