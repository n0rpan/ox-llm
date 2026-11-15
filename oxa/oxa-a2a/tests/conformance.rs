//! Selected pinned examples, with normative-schema discrepancies made explicit.
use oxa_a2a::wire::*;
use serde_json::{json, Value};

#[test]
fn specification_send_example_requires_the_schema_message_discriminant() {
    let mut example: Value = serde_json::from_str(include_str!("fixtures/send-request.json")).unwrap();
    let request: Request = serde_json::from_value(example.clone()).unwrap();
    request.validate().unwrap();
    // The published 9.1 example omits kind, which its normative schema requires.
    assert!(serde_json::from_value::<SendParams>(request.params).is_err());
    example["params"]["message"]["kind"] = json!("message");
    let params: SendParams = serde_json::from_value(example["params"].clone()).unwrap();
    let MessageObject::Message(message) = params.message;
    assert_eq!(message.role, Role::User);
    assert_eq!(
        message.parts,
        vec![Part::Text {
            text: "tell me a joke".into()
        }]
    );
}

#[test]
fn specification_task_example_with_corrected_history_discriminant() {
    let mut example: Value = serde_json::from_str(include_str!("fixtures/task-response.json")).unwrap();
    let result = response_result(example.clone(), &RequestId::Number(1)).unwrap();
    assert!(serde_json::from_value::<Event>(result).is_err());
    example["result"]["history"][0]["kind"] = json!("message");
    let Event::Task(task) = serde_json::from_value(example["result"].clone()).unwrap() else {
        panic!()
    };
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(task.artifacts[0].name.as_deref(), Some("joke"));
    assert_eq!(task.history.len(), 1);
}

#[test]
fn specification_direct_message_example_with_corrected_role() {
    let mut example: Value = serde_json::from_str(include_str!("fixtures/message-response.json")).unwrap();
    let result = response_result(example.clone(), &RequestId::Number(1)).unwrap();
    assert!(serde_json::from_value::<Event>(result).is_err());
    // Published 9.2 omits role, also required by the normative Message schema.
    example["result"]["role"] = json!("agent");
    let Event::Message(message) = serde_json::from_value(example["result"].clone()).unwrap() else {
        panic!()
    };
    assert!(message.task_id.is_none());
    assert_eq!(message.role, Role::Agent);
}
