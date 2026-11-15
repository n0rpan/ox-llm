use oxa_a2a::loopback::{Capability, Client, Limits};
use oxa_a2a::mock::{Behavior, MockAgent};
use oxa_a2a::wire::*;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};

fn prompt(text: &str) -> SendParams {
    SendParams {
        message: MessageObject::Message(Message {
            message_id: "synthetic-message".into(),
            role: Role::User,
            parts: vec![Part::Text { text: text.into() }],
            task_id: None,
            context_id: None,
        }),
    }
}

#[test]
fn discover_send_get_and_protocol_errors_over_authenticated_http() {
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let mut client = mock.client(Limits::default()).unwrap();
    let card = client.card().unwrap();
    assert!(card.capabilities.streaming);
    let Event::Task(task) = client.send(prompt("synthetic success")).unwrap() else {
        panic!()
    };
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(
        task.artifacts[0].parts,
        vec![Part::Text {
            text: "synthetic success".into()
        }]
    );
    assert_eq!(client.get(&task.id).unwrap(), task);
    assert!(client
        .cancel(&task.id)
        .unwrap_err()
        .to_string()
        .contains("-32002"));
    assert!(client.get("absent").unwrap_err().to_string().contains("-32001"));
    assert!(client
        .call("unknown", json!({}))
        .unwrap_err()
        .to_string()
        .contains("-32601"));
}

#[test]
fn stream_collects_artifact_and_terminal_status() {
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let mut client = mock.client(Limits::default()).unwrap();
    let mut stream = client.stream(prompt("streamed synthetic result")).unwrap();
    let Some(Event::Task(task)) = stream.next_event().unwrap() else {
        panic!()
    };
    assert_eq!(task.status.state, TaskState::Working);
    assert!(matches!(stream.next_event().unwrap(), Some(Event::Artifact(_))));
    let Some(Event::Status(status)) = stream.next_event().unwrap() else {
        panic!()
    };
    assert_eq!(status.status.state, TaskState::Completed);
    assert!(status.final_event);
    assert!(stream.next_event().unwrap().is_none());
    assert_eq!(client.get(&task.id).unwrap().status.state, TaskState::Completed);
}

#[test]
fn cancel_paused_task_and_resubscribe_without_relaunching() {
    let mock = MockAgent::start(Behavior::AwaitInput).unwrap();
    let mut client = mock.client(Limits::default()).unwrap();
    let mut stream = client.stream(prompt("await input")).unwrap();
    let Some(Event::Task(task)) = stream.next_event().unwrap() else {
        panic!()
    };
    let Some(Event::Status(status)) = stream.next_event().unwrap() else {
        panic!()
    };
    assert!(status.final_event);
    assert!(!status.status.state.is_terminal());
    assert!(stream.next_event().unwrap().is_none());
    assert_eq!(client.cancel(&task.id).unwrap().status.state, TaskState::Canceled);
    let mut resumed = client.resubscribe(&task.id).unwrap();
    let Some(Event::Task(task)) = resumed.next_event().unwrap() else {
        panic!()
    };
    assert_eq!(task.status.state, TaskState::Canceled);
    assert!(resumed.next_event().unwrap().is_none());
}

#[test]
fn network_and_authentication_fail_closed() {
    let capability = Capability::generate().unwrap();
    assert_eq!(format!("{capability:?}"), "Capability([redacted])");
    assert!(Client::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 80),
        capability.clone(),
        Limits::default()
    )
    .is_err());
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let mut wrong_client = Client::new(mock.address(), capability, Limits::default()).unwrap();
    assert!(wrong_client.send(prompt("denied")).is_err());
    let mut socket = TcpStream::connect(mock.address()).unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    socket
        .write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 401"));
}

#[test]
fn response_size_limit_is_enforced_before_allocation() {
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let client = mock
        .client(Limits {
            body_bytes: 8,
            ..Limits::default()
        })
        .unwrap();
    assert!(client.card().is_err());
}

#[test]
fn shared_response_allowance_cannot_be_overspent_by_concurrent_clients() {
    use oxa_a2a::loopback::ResponseBudget;
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let client = mock.client(Limits::default()).unwrap();
    let budget = ResponseBudget::new(900);
    let first = client.fork(Limits::default(), budget.clone()).unwrap();
    let second = client.fork(Limits::default(), budget.clone()).unwrap();
    let a = std::thread::spawn(move || first.card().is_ok());
    let b = std::thread::spawn(move || second.card().is_ok());
    let successes = usize::from(a.join().unwrap()) + usize::from(b.join().unwrap());
    assert!(successes <= 1);
    assert!(budget.used() <= 900);
}

#[test]
fn unused_observation_allowance_can_fund_cleanup_without_widening_the_budget() {
    use oxa_a2a::loopback::ResponseBudget;
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let client = mock.client(Limits::default()).unwrap();
    let read = ResponseBudget::new(4096);
    let stop = ResponseBudget::new(512);
    let observed = client.fork(Limits::default(), read.clone()).unwrap();
    observed.card().unwrap();
    assert!(read.transfer_remaining_to(&stop).unwrap() > 0);
    assert_eq!(read.remaining(), 0);
    let cleanup = client.fork(Limits::default(), stop.clone()).unwrap();
    cleanup.card().unwrap();
    assert_eq!(read.used() + stop.used() + stop.remaining(), 4096 + 512);
}
