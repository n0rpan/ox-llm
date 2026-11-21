//! K1 acceptance: a board survives a kill with no torn file, two writers moving different
//! cards both win, a stale text edit is refused with both versions, each actor does exactly
//! its row of the plan's actor table, and undo restores the previous state. All fixtures are
//! synthetic and owner-private; no cloud call, no real credential, no script.
#[path = "support/private_dir.rs"]
mod test_dir;

use oxa_work::kanban::*;
use std::sync::{Arc, Barrier};
use test_dir::PrivateDir;

const REPO: &str = "owner/repo";

fn board_dir(fixture: &PrivateDir) -> std::path::PathBuf {
    fixture.path().join("kanban").join("owner__repo")
}
fn open(fixture: &PrivateDir) -> Store {
    Store::open(&board_dir(fixture), REPO).unwrap()
}
fn add_op(title: &str, column: Column) -> Op {
    Op::Add {
        title: title.into(),
        issue: None,
        done_when: "done when it is verified".into(),
        kind: "build".into(),
        model: None,
        host: HostPref::Any,
        needs: Default::default(),
        column,
    }
}
fn add(store: &Store, title: &str, column: Column) -> u32 {
    store.apply(Actor::Owner, add_op(title, column)).unwrap();
    store
        .read()
        .unwrap()
        .items
        .iter()
        .find(|i| i.title == title)
        .unwrap()
        .id
}
fn title_edit(id: u32, rev: u32, title: &str) -> Op {
    Op::Edit {
        id,
        rev,
        fields: EditFields {
            title: Some(title.into()),
            ..Default::default()
        },
    }
}
fn rev(store: &Store, id: u32) -> u32 {
    store.read().unwrap().item(id).unwrap().rev
}
fn set_manual_goal(store: &Store) {
    store
        .apply(
            Actor::Owner,
            Op::SetGoal {
                goal: Some(Goal {
                    text: "ship the slice".into(),
                    measures: vec![Measure {
                        id: "m1".into(),
                        kind: MeasureKind::Manual {
                            acceptance: "installs cleanly".into(),
                            result: None,
                        },
                    }],
                    target_per_hour: Some(10.0),
                    set_at: 1,
                    met_at: None,
                }),
            },
        )
        .unwrap();
}

#[test]
fn a_fresh_board_is_paused_and_owner_private() {
    let fixture = PrivateDir::new("kanban-fresh");
    let store = open(&fixture);
    let board = store.read().unwrap();
    assert_eq!(board.schema, 1);
    assert_eq!(board.dispatch, Dispatch::Paused);
    assert_eq!(board.revision, 0);
    assert!(board.items.is_empty());
    // The directory and board file are owner-only.
    use std::os::unix::fs::MetadataExt;
    let dir_mode = std::fs::metadata(board_dir(&fixture)).unwrap().mode() & 0o777;
    let file_mode = std::fs::metadata(board_dir(&fixture).join("board.json"))
        .unwrap()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(file_mode, 0o600);
}

#[test]
fn a_stale_text_edit_is_refused_with_both_versions() {
    let fixture = PrivateDir::new("kanban-stale");
    let store = open(&fixture);
    let id = add(&store, "original", Column::Backlog);
    let opened_rev = rev(&store, id);
    // Someone else edits first.
    store
        .apply(Actor::Owner, title_edit(id, opened_rev, "server version"))
        .unwrap();
    // The stale edit carries the revision it opened and is refused with both safe versions.
    let err = store
        .apply(Actor::Owner, title_edit(id, opened_rev, "client version"))
        .unwrap_err();
    match err {
        Error::Stale { current, attempted } => {
            assert_eq!(current.id, id);
            assert_eq!(current.title, "server version");
            assert_eq!(attempted.title, "client version");
        }
        other => panic!("expected a stale conflict, got {other:?}"),
    }
    // The refused edit changed nothing.
    assert_eq!(store.read().unwrap().item(id).unwrap().title, "server version");
}

#[test]
fn two_writers_moving_different_cards_both_win() {
    let fixture = PrivateDir::new("kanban-concurrent");
    let store = open(&fixture);
    let a = add(&store, "card a", Column::Ready);
    let b = add(&store, "card b", Column::Ready);
    let barrier = Arc::new(Barrier::new(2));
    let (s1, s2) = (store.clone(), store.clone());
    let (b1, b2) = (barrier.clone(), barrier.clone());
    let t1 = std::thread::spawn(move || {
        b1.wait();
        s1.apply(
            Actor::Owner,
            Op::Move {
                id: a,
                to: Column::Backlog,
                before: None,
            },
        )
    });
    let t2 = std::thread::spawn(move || {
        b2.wait();
        s2.apply(
            Actor::Owner,
            Op::Move {
                id: b,
                to: Column::Backlog,
                before: None,
            },
        )
    });
    assert!(t1.join().unwrap().is_ok());
    assert!(t2.join().unwrap().is_ok());
    let board = store.read().unwrap();
    assert_eq!(board.item(a).unwrap().column, Column::Backlog);
    assert_eq!(board.item(b).unwrap().column, Column::Backlog);
    // Two adds and two moves: no lost update.
    assert_eq!(board.revision, 4);
}

#[test]
fn many_concurrent_moves_of_distinct_cards_all_land() {
    let fixture = PrivateDir::new("kanban-concurrent-many");
    let store = open(&fixture);
    let ids: Vec<u32> = (0..6)
        .map(|i| add(&store, &format!("card {i}"), Column::Ready))
        .collect();
    let barrier = Arc::new(Barrier::new(ids.len()));
    let handles: Vec<_> = ids
        .iter()
        .map(|&id| {
            let s = store.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                // Each writer shuttles its own card; relative moves never clobber a sibling.
                for _ in 0..5 {
                    s.apply(
                        Actor::Owner,
                        Op::Move {
                            id,
                            to: Column::Backlog,
                            before: None,
                        },
                    )
                    .unwrap();
                    s.apply(
                        Actor::Owner,
                        Op::Move {
                            id,
                            to: Column::Ready,
                            before: None,
                        },
                    )
                    .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let board = store.read().unwrap();
    assert_eq!(board.revision, 6 + 6 * 10);
    for id in ids {
        assert_eq!(board.item(id).unwrap().column, Column::Ready);
    }
    // Ranks stay dense in every lane after the interleaving.
    for column in [Column::Ready, Column::Backlog] {
        let ranks: Vec<u32> = board.lane(column).iter().map(|i| i.rank).collect();
        assert_eq!(ranks, (0..ranks.len() as u32).collect::<Vec<_>>());
    }
}

#[test]
fn the_owner_may_do_everything() {
    let fixture = PrivateDir::new("kanban-owner");
    let store = open(&fixture);
    let id = add(&store, "owner card", Column::Backlog);
    set_manual_goal(&store);
    store
        .apply(
            Actor::Owner,
            Op::SetCapacity {
                host: "host-a".into(),
                slots: 4,
                paused: false,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Owner,
            Op::SetDispatch {
                state: Dispatch::Running,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Owner,
            Op::Move {
                id,
                to: Column::Ready,
                before: None,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Owner,
            Op::Move {
                id,
                to: Column::Running,
                before: None,
            },
        )
        .unwrap();
    store
        .apply(Actor::Owner, title_edit(id, rev(&store, id), "renamed"))
        .unwrap();
    store
        .apply(
            Actor::Owner,
            Op::Note {
                id,
                text: "owner note".into(),
            },
        )
        .unwrap();
    store.apply(Actor::Owner, Op::LinkPr { id, number: 42 }).unwrap();
    store
        .apply(
            Actor::Owner,
            Op::CheckMeasure {
                id: "m1".into(),
                result: MeasureResult {
                    passed: true,
                    evidence: "synthetic receipt".into(),
                    at: 2,
                },
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Owner,
            Op::SetStuck {
                id,
                cause: Cause::Manual,
                detail: "owner hold".into(),
            },
        )
        .unwrap();
    store.apply(Actor::Owner, Op::Unstick { id }).unwrap();
    store.apply(Actor::Owner, Op::Remove { id }).unwrap();
    assert!(store.read().unwrap().item(id).is_none());
}

#[test]
fn the_lead_does_exactly_its_row() {
    let fixture = PrivateDir::new("kanban-lead");
    let store = open(&fixture);
    set_manual_goal(&store);
    let owner_item = add(&store, "owner item", Column::Backlog);
    // Allowed: add, edit, move Backlog<->Ready, reorder, stick/unstick, link, split, tick manual.
    store
        .apply(Actor::Lead, add_op("lead item", Column::Backlog))
        .unwrap();
    let lead_item = store
        .read()
        .unwrap()
        .items
        .iter()
        .find(|i| i.title == "lead item")
        .unwrap()
        .id;
    store
        .apply(
            Actor::Lead,
            title_edit(lead_item, rev(&store, lead_item), "lead edit"),
        )
        .unwrap();
    store
        .apply(
            Actor::Lead,
            Op::Move {
                id: lead_item,
                to: Column::Ready,
                before: None,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Lead,
            Op::Move {
                id: lead_item,
                to: Column::Backlog,
                before: None,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Lead,
            Op::SetStuck {
                id: lead_item,
                cause: Cause::CiRed,
                detail: "synthetic".into(),
            },
        )
        .unwrap();
    store.apply(Actor::Lead, Op::Unstick { id: lead_item }).unwrap();
    store
        .apply(
            Actor::Lead,
            Op::LinkPr {
                id: lead_item,
                number: 7,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Lead,
            Op::CheckMeasure {
                id: "m1".into(),
                result: MeasureResult {
                    passed: true,
                    evidence: "lead evidence".into(),
                    at: 3,
                },
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Lead,
            Op::Split {
                id: lead_item,
                rev: rev(&store, lead_item),
                children: vec![
                    NewChild {
                        title: "child one".into(),
                        done_when: "first half verified".into(),
                    },
                    NewChild {
                        title: "child two".into(),
                        done_when: "second half verified".into(),
                    },
                ],
            },
        )
        .unwrap();
    // Refused: capacity, goal, dispatch on/off, removing an owner item, moving into a PR lane,
    // noting (a worker/owner action), and re-ranking above an owner-placed card.
    assert!(store
        .apply(
            Actor::Lead,
            Op::SetCapacity {
                host: "host-a".into(),
                slots: 2,
                paused: false
            }
        )
        .is_err());
    assert!(store.apply(Actor::Lead, Op::SetGoal { goal: None }).is_err());
    assert!(store
        .apply(
            Actor::Lead,
            Op::SetDispatch {
                state: Dispatch::Running
            }
        )
        .is_err());
    assert!(store.apply(Actor::Lead, Op::Remove { id: owner_item }).is_err());
    assert!(store
        .apply(
            Actor::Lead,
            Op::Note {
                id: owner_item,
                text: "x".into()
            }
        )
        .is_err());
    let ready_owner = add(&store, "owner ready", Column::Ready);
    // Created by the lead so it is lead-owned (the lead may remove its own item below).
    store
        .apply(Actor::Lead, add_op("lead ready", Column::Ready))
        .unwrap();
    let lead_ready = store
        .read()
        .unwrap()
        .items
        .iter()
        .find(|i| i.title == "lead ready")
        .unwrap()
        .id;
    assert!(store
        .apply(
            Actor::Lead,
            Op::Move {
                id: lead_ready,
                to: Column::Running,
                before: None
            }
        )
        .is_err());
    assert!(store
        .apply(
            Actor::Lead,
            Op::Reorder {
                id: lead_ready,
                before: Some(ready_owner)
            }
        )
        .is_err());
    // The owner may re-rank above its own card.
    store
        .apply(
            Actor::Owner,
            Op::Reorder {
                id: lead_ready,
                before: Some(ready_owner),
            },
        )
        .unwrap();
    // The lead can remove its own item.
    store.apply(Actor::Lead, Op::Remove { id: lead_ready }).unwrap();
}

#[test]
fn a_worker_notes_and_sticks_only_its_own_item() {
    let fixture = PrivateDir::new("kanban-worker");
    let store = open(&fixture);
    let own = add(&store, "worker card", Column::Backlog);
    let other = add(&store, "other card", Column::Backlog);
    store
        .apply(
            Actor::Worker(own),
            Op::Note {
                id: own,
                text: "synthetic progress".into(),
            },
        )
        .unwrap();
    store
        .apply(
            Actor::Worker(own),
            Op::SetStuck {
                id: own,
                cause: Cause::WorkerFailed,
                detail: "synthetic".into(),
            },
        )
        .unwrap();
    assert!(store
        .apply(
            Actor::Worker(own),
            Op::Note {
                id: other,
                text: "x".into()
            }
        )
        .is_err());
    assert!(store
        .apply(
            Actor::Worker(own),
            Op::SetStuck {
                id: other,
                cause: Cause::Manual,
                detail: "".into()
            }
        )
        .is_err());
    assert!(store
        .apply(Actor::Worker(own), title_edit(own, rev(&store, own), "x"))
        .is_err());
    assert!(store
        .apply(
            Actor::Worker(own),
            Op::Move {
                id: own,
                to: Column::Ready,
                before: None
            }
        )
        .is_err());
    assert!(store
        .apply(Actor::Worker(own), add_op("new", Column::Backlog))
        .is_err());
    assert!(store.apply(Actor::Worker(own), Op::Remove { id: own }).is_err());
    assert!(store.apply(Actor::Worker(own), Op::Unstick { id: own }).is_err());
}

#[test]
fn the_system_moves_pr_lanes_and_sets_automatic_stuck_and_goal_states() {
    let fixture = PrivateDir::new("kanban-system");
    let store = open(&fixture);
    let id = add(&store, "system card", Column::Ready);
    for to in [Column::Running, Column::PrOpen, Column::InQueue, Column::Merged] {
        store
            .apply(Actor::System, Op::Move { id, to, before: None })
            .unwrap();
    }
    store
        .apply(
            Actor::System,
            Op::SetStuck {
                id,
                cause: Cause::CiRed,
                detail: "synthetic".into(),
            },
        )
        .unwrap();
    store
        .apply(
            Actor::System,
            Op::SetDispatch {
                state: Dispatch::GoalMet,
            },
        )
        .unwrap();
    store
        .apply(
            Actor::System,
            Op::SetDispatch {
                state: Dispatch::Starved,
            },
        )
        .unwrap();
    // Refused: manual stuck, on/off dispatch, and everything outside its row.
    assert!(store
        .apply(
            Actor::System,
            Op::SetStuck {
                id,
                cause: Cause::Manual,
                detail: "".into()
            }
        )
        .is_err());
    assert!(store
        .apply(
            Actor::System,
            Op::SetDispatch {
                state: Dispatch::Running
            }
        )
        .is_err());
    assert!(store
        .apply(
            Actor::System,
            Op::SetDispatch {
                state: Dispatch::Paused
            }
        )
        .is_err());
    assert!(store
        .apply(
            Actor::System,
            Op::Move {
                id,
                to: Column::Backlog,
                before: None
            }
        )
        .is_err());
    assert!(store
        .apply(Actor::System, add_op("new", Column::Backlog))
        .is_err());
    assert!(store
        .apply(Actor::System, title_edit(id, rev(&store, id), "x"))
        .is_err());
    assert!(store
        .apply(
            Actor::System,
            Op::SetCapacity {
                host: "host-a".into(),
                slots: 1,
                paused: false
            }
        )
        .is_err());
    assert!(store.apply(Actor::System, Op::Remove { id }).is_err());
    assert!(store
        .apply(Actor::System, Op::Reorder { id, before: None })
        .is_err());
}

#[test]
fn undo_restores_the_previous_state() {
    let fixture = PrivateDir::new("kanban-undo");
    let store = open(&fixture);
    let id = add(&store, "v1", Column::Backlog);
    // Undo an edit.
    let edit = store
        .apply(Actor::Owner, title_edit(id, rev(&store, id), "v2"))
        .unwrap();
    assert_eq!(store.read().unwrap().item(id).unwrap().title, "v2");
    store.apply(Actor::Owner, Op::Undo { seq: edit.seq }).unwrap();
    assert_eq!(store.read().unwrap().item(id).unwrap().title, "v1");
    // Undo a move.
    let moved = store
        .apply(
            Actor::Owner,
            Op::Move {
                id,
                to: Column::Ready,
                before: None,
            },
        )
        .unwrap();
    assert_eq!(store.read().unwrap().item(id).unwrap().column, Column::Ready);
    store.apply(Actor::Owner, Op::Undo { seq: moved.seq }).unwrap();
    assert_eq!(store.read().unwrap().item(id).unwrap().column, Column::Backlog);
    // Undo an add removes the item.
    let count = store.read().unwrap().items.len();
    let added = store
        .apply(Actor::Owner, add_op("extra", Column::Backlog))
        .unwrap();
    assert_eq!(store.read().unwrap().items.len(), count + 1);
    store.apply(Actor::Owner, Op::Undo { seq: added.seq }).unwrap();
    assert_eq!(store.read().unwrap().items.len(), count);
    // Undo a remove restores the item.
    let removed = store.apply(Actor::Owner, Op::Remove { id }).unwrap();
    assert!(store.read().unwrap().item(id).is_none());
    store.apply(Actor::Owner, Op::Undo { seq: removed.seq }).unwrap();
    assert_eq!(store.read().unwrap().item(id).unwrap().title, "v1");
}

#[test]
fn undo_is_guarded_by_later_changes_and_actor() {
    let fixture = PrivateDir::new("kanban-undo-guard");
    let store = open(&fixture);
    let id = add(&store, "v1", Column::Backlog);
    let e1 = store
        .apply(Actor::Owner, title_edit(id, rev(&store, id), "v2"))
        .unwrap();
    let e2 = store
        .apply(Actor::Owner, title_edit(id, rev(&store, id), "v3"))
        .unwrap();
    // The item changed after e1, so e1 can no longer be undone.
    assert!(store.apply(Actor::Owner, Op::Undo { seq: e1.seq }).is_err());
    // The latest edit undoes cleanly.
    store.apply(Actor::Owner, Op::Undo { seq: e2.seq }).unwrap();
    assert_eq!(store.read().unwrap().item(id).unwrap().title, "v2");
    // An undo cannot itself be undone.
    let again = store.apply(Actor::Owner, Op::Undo { seq: e2.seq });
    assert!(again.is_err());
    // The lead undoes only its own ops.
    let lead_add = store
        .apply(Actor::Lead, add_op("lead card", Column::Backlog))
        .unwrap();
    store.apply(Actor::Lead, Op::Undo { seq: lead_add.seq }).unwrap();
    let owner_cap = store
        .apply(
            Actor::Owner,
            Op::SetCapacity {
                host: "host-a".into(),
                slots: 3,
                paused: false,
            },
        )
        .unwrap();
    assert!(store.apply(Actor::Lead, Op::Undo { seq: owner_cap.seq }).is_err());
    store
        .apply(Actor::Owner, Op::Undo { seq: owner_cap.seq })
        .unwrap();
    assert!(store.read().unwrap().capacity.is_empty());
}

#[test]
fn a_ready_item_needs_a_done_when() {
    let fixture = PrivateDir::new("kanban-ready");
    let store = open(&fixture);
    let mut no_done = add_op("no done-when", Column::Backlog);
    if let Op::Add { done_when, .. } = &mut no_done {
        done_when.clear();
    }
    // Backlog without a done-when is allowed; Ready is refused.
    store.apply(Actor::Owner, no_done.clone()).unwrap();
    let id = store
        .read()
        .unwrap()
        .items
        .iter()
        .find(|i| i.title == "no done-when")
        .unwrap()
        .id;
    if let Op::Add { column, .. } = &mut no_done {
        *column = Column::Ready;
    }
    assert!(store.apply(Actor::Owner, no_done).is_err());
    assert!(store
        .apply(
            Actor::Owner,
            Op::Move {
                id,
                to: Column::Ready,
                before: None
            }
        )
        .is_err());
}

#[test]
fn a_torn_board_is_refused_not_silently_loaded() {
    let fixture = PrivateDir::new("kanban-torn");
    let dir = board_dir(&fixture);
    let store = Store::open(&dir, REPO).unwrap();
    store
        .apply(Actor::Owner, add_op("card", Column::Backlog))
        .unwrap();
    drop(store);
    // Simulate a torn write: half the bytes. The atomic rename never leaves this state, and
    // the loader refuses it rather than turning it into an empty or partial board.
    let path = dir.join("board.json");
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
    assert!(Store::open(&dir, REPO).is_err());
}

#[test]
fn board_survives_a_kill_with_no_torn_file() {
    // Child mode: the parent below re-executes this exact test; write continuously until killed.
    if std::env::var_os("OXA_KANBAN_CRASH_WRITER").is_some() {
        let dir = std::path::PathBuf::from(std::env::var("OXA_KANBAN_CRASH_DIR").unwrap());
        let store = Store::open(&dir, REPO).unwrap();
        let id = store.read().unwrap().items[0].id;
        let mut to_ready = true;
        loop {
            let to = if to_ready { Column::Ready } else { Column::Backlog };
            let _ = store.apply(Actor::Owner, Op::Move { id, to, before: None });
            to_ready = !to_ready;
        }
    }
    let fixture = PrivateDir::new("kanban-crash");
    let dir = board_dir(&fixture);
    let store = Store::open(&dir, REPO).unwrap();
    store
        .apply(Actor::Owner, add_op("crash card", Column::Backlog))
        .unwrap();
    drop(store);
    for _ in 0..3 {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .env("OXA_KANBAN_CRASH_WRITER", "1")
            .env("OXA_KANBAN_CRASH_DIR", &dir)
            .args([
                "--exact",
                "board_survives_a_kill_with_no_torn_file",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        child.kill().unwrap();
        child.wait().unwrap();
        // After the kill the board loads whole: no torn file, no lost item.
        let reopened = Store::open(&dir, REPO).unwrap();
        let board = reopened.read().unwrap();
        assert_eq!(board.schema, 1);
        assert_eq!(board.items.len(), 1);
        assert!(board.revision >= 1);
    }
}
