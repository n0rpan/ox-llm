//! Issue #581: a request reads the wire without copying it.

use oxa_proto::WireMsg;
use oxa_session::Transcript;

#[test]
fn wire_shared_is_the_wire_itself_not_a_copy() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("one"));
    t.push(WireMsg::assistant("two"));
    let shared = t.wire_shared();
    assert!(
        std::ptr::eq(shared.as_ptr(), t.wire().as_ptr()),
        "the handle must share the wire"
    );
    assert_eq!(shared.len(), 2);
}

#[test]
fn a_push_while_a_handle_is_held_leaves_the_handle_unchanged() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("one"));
    let held = t.wire_shared();
    t.push(WireMsg::assistant("two"));
    assert_eq!(
        held.len(),
        1,
        "a request in flight keeps the wire it was built from"
    );
    assert_eq!(t.wire().len(), 2);
    assert_eq!(&t.wire()[..1], &held[..]);
}
