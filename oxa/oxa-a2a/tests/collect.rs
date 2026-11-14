use oxa_a2a::{collect::Collector, wire::*};

fn task(state: TaskState) -> Task {
    Task {
        id: "t".into(),
        context_id: "c".into(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts: vec![],
        history: vec![],
    }
}
fn chunk(text: &str, append: bool, last_chunk: bool) -> Event {
    Event::Artifact(ArtifactUpdate {
        task_id: "t".into(),
        context_id: "c".into(),
        artifact: Artifact {
            artifact_id: "a".into(),
            parts: vec![Part::Text { text: text.into() }],
            name: None,
            description: None,
        },
        append,
        last_chunk,
    })
}
fn status(state: TaskState, final_event: bool) -> Event {
    Event::Status(StatusUpdate {
        task_id: "t".into(),
        context_id: "c".into(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        final_event,
    })
}

#[test]
fn chunks_survive_reconciliation_and_pause_is_not_completion() {
    let mut collector = Collector::new(4096);
    collector.apply(Event::Task(task(TaskState::Working))).unwrap();
    collector.apply(chunk("first", false, false)).unwrap();
    collector.reconcile(task(TaskState::Working)).unwrap();
    collector.apply(chunk("second", true, true)).unwrap();
    collector.apply(status(TaskState::InputRequired, true)).unwrap();
    assert!(collector.stream_final());
    assert!(!collector.task().unwrap().status.state.is_terminal());
    collector.reconcile(task(TaskState::Working)).unwrap();
    collector.apply(status(TaskState::Completed, true)).unwrap();
    let parts = &collector.task().unwrap().artifacts[0].parts;
    assert_eq!(
        parts,
        &vec![
            Part::Text { text: "first".into() },
            Part::Text {
                text: "second".into()
            }
        ]
    );
}

#[test]
fn invalid_appends_identity_changes_and_post_final_events_fail_closed() {
    let mut missing = Collector::new(4096);
    assert!(missing.apply(chunk("missing", true, true)).is_err());
    let mut closed = Collector::new(4096);
    closed.apply(chunk("one", false, true)).unwrap();
    assert!(closed.apply(chunk("two", true, true)).is_err());
    assert!(closed.reconcile(task(TaskState::Completed)).is_err());
    let mut changed = Collector::new(4096);
    changed.apply(Event::Task(task(TaskState::Working))).unwrap();
    let mut other = task(TaskState::Completed);
    other.context_id = "other".into();
    assert!(changed.reconcile(other).is_err());
    let mut terminal = Collector::new(4096);
    assert!(terminal.apply(status(TaskState::Completed, false)).is_err());
    let mut final_task = Collector::new(4096);
    final_task.apply(Event::Task(task(TaskState::Completed))).unwrap();
    assert!(final_task.apply(chunk("late", false, true)).is_err());
}

#[test]
fn collection_limits_and_direct_message_roles_are_checked() {
    let mut collector = Collector::new(512);
    collector.apply(Event::Task(task(TaskState::Working))).unwrap();
    assert!(collector.apply(chunk(&"x".repeat(1024), false, true)).is_err());
    assert!(collector.task().unwrap().artifacts.is_empty());
    for role in [Role::User, Role::Agent] {
        let mut collector = Collector::new(4096);
        let result = collector.apply(Event::Message(Message {
            message_id: "m".into(),
            role: role.clone(),
            parts: vec![Part::Text {
                text: "direct".into(),
            }],
            task_id: None,
            context_id: None,
        }));
        assert_eq!(result.is_ok(), role == Role::Agent);
        assert!(collector.task().is_none());
    }
}

#[test]
fn recovery_cannot_promote_an_incomplete_artifact_to_a_complete_result() {
    let mut collector = Collector::new(4096);
    collector.apply(Event::Task(task(TaskState::Working))).unwrap();
    collector.apply(chunk("partial", false, false)).unwrap();
    assert!(collector.reconcile(task(TaskState::Completed)).is_err());
    assert!(collector.is_failed());
    assert_eq!(collector.task().unwrap().status.state, TaskState::Working);
}
