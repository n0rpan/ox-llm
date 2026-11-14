//! Wave H (SES-008): `/import` keeps the session's id and working directory (pi
//! slash-commands.md); a stored file opens as it is; a taken id gets a fresh one.
use oxa_fake::TestEnv;
use oxa_session::{import_session, FsStore, SessionStore};

const PI_V3: &str = concat!(
    r#"{"type":"session","version":3,"id":"0f6c2d9e-1111-4222-8333-944455556666","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/work/elsewhere"}"#,
    "\n",
    r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"imported question","timestamp":1}}"#,
    "\n",
    r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"imported answer"}],"stopReason":"stop","timestamp":2}}"#,
    "\n",
);

#[test]
fn keeps_id_and_cwd() {
    let env = TestEnv::new();
    let st = FsStore::new(env.root().join("sessions"));
    let src = env.write("import/pi.jsonl", PI_V3);
    let dst = import_session(&src, &st, &env.work()).unwrap();
    assert_eq!(dst.id, "0f6c2d9e-1111-4222-8333-944455556666");
    let h = st.header(&dst).unwrap();
    assert_eq!(h.cwd, std::path::PathBuf::from("/work/elsewhere"));
    assert_eq!(st.load(&dst).unwrap().len(), 2);
    // The stored file itself: opened as it is.
    assert_eq!(import_session(&dst.path, &st, &env.work()).unwrap(), dst);
    // The same source again: the id is taken, the copy gets a new one.
    let again = import_session(&src, &st, &env.work()).unwrap();
    assert_ne!(again.id, dst.id);
    assert_eq!(
        st.header(&again).unwrap().cwd,
        std::path::PathBuf::from("/work/elsewhere")
    );
}
