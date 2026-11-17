#![cfg(all(target_os = "linux", feature = "a2a"))]

use oxa_orchestrator::control::{admit, Request};
use serde_json::{json, Value};
use std::sync::mpsc;

fn send(value: Value, sender: &mpsc::SyncSender<Request>) -> Value {
    admit(&serde_json::to_vec(&value).unwrap(), sender)
}

#[test]
fn lifecycle_and_steering_keep_request_identity_and_owner_revision() {
    let (tx, rx) = mpsc::sync_channel(4);
    for method in ["pause", "resume", "cancel"] {
        let result = send(json!({"method":method,"id":method,"revision":7}), &tx);
        assert_eq!(result, json!({"id":method,"queued":true}));
    }
    assert_eq!(
        send(
            json!({"method":"steer","id":"four","revision":0,"text":"Keep the original budget"}),
            &tx
        ),
        json!({"id":"four","queued":true})
    );
    assert_eq!(
        rx.recv().unwrap(),
        Request::Pause {
            id: "pause".into(),
            revision: 7
        }
    );
    assert_eq!(
        rx.recv().unwrap(),
        Request::Resume {
            id: "resume".into(),
            revision: 7
        }
    );
    assert_eq!(
        rx.recv().unwrap(),
        Request::Cancel {
            id: "cancel".into(),
            revision: 7
        }
    );
    // Admission does not invent freshness/authority: only the owner can accept 0.
    assert_eq!(
        rx.recv().unwrap(),
        Request::Steer {
            id: "four".into(),
            revision: 0,
            text: "Keep the original budget".into()
        }
    );
}

#[test]
fn queue_pressure_and_disconnected_owner_are_bounded_receipts() {
    let (tx, rx) = mpsc::sync_channel(1);
    let pause = json!({"method":"pause","id":"first","revision":1});
    assert_eq!(send(pause.clone(), &tx)["queued"], true);
    assert_eq!(send(pause.clone(), &tx)["error"], "control_queue_full");
    assert_eq!(rx.recv().unwrap().id(), "first");
    assert_eq!(send(pause.clone(), &tx)["queued"], true);
    drop(rx);
    assert_eq!(send(pause, &tx)["error"], "session_stopped");
}

#[test]
fn malformed_unknown_or_authority_fields_never_reach_owner() {
    let (tx, rx) = mpsc::sync_channel(8);
    assert_eq!(admit(b"not json", &tx), json!({"error":"invalid_request"}));
    for value in [
        json!({"method":"steer","id":"one","revision":1,"text":"do work","authority":"owner"}),
        json!({"method":"pause","id":"one","revision":1,"budget":999}),
        json!({"method":"set_goal","id":"one","revision":1,"text":"other"}),
        json!({"method":"pause","id":"one","revision":-1}),
    ] {
        assert_eq!(send(value, &tx), json!({"error":"invalid_request"}));
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn invalid_ids_and_steering_text_are_not_echoed_or_admitted() {
    let (tx, rx) = mpsc::sync_channel(8);
    for id in [String::new(), "x".repeat(129), "line\nbreak".into()] {
        assert_eq!(
            send(json!({"method":"pause","id":id,"revision":1}), &tx),
            json!({"error":"invalid_id"})
        );
    }
    for text in [String::new(), "x".repeat(16 * 1024 + 1), "nul\0byte".into()] {
        assert_eq!(
            send(json!({"method":"steer","id":"one","revision":1,"text":text}), &tx),
            json!({"id":"one","error":"invalid_steer"})
        );
    }
    assert_eq!(
        admit(&vec![b'x'; 32 * 1024 + 1], &tx),
        json!({"error":"request_too_large"})
    );
    assert!(rx.try_recv().is_err());
}
