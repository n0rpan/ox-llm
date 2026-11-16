//! Fork publication and spill reclamation must be one serializable lifecycle.
use oxa_proto::{Entry, EntryBody, OxaError, WireMsg};
use oxa_session::{
    clone_session, fork_file, inherited_spill_dirs, remove_session_preserving_spills,
    remove_spill_dir_unless_forked, FsStore, SessionHeader, SessionMeta, SessionRef, SessionStore,
};
use std::fs;
use std::path::Path;
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

struct PausedLoad {
    store: FsStore,
    loaded: Arc<Barrier>,
    resume: Arc<Barrier>,
}

impl SessionStore for PausedLoad {
    fn spill_lifecycle_lock(
        &self,
        session: &SessionRef,
    ) -> Result<oxa_session::SpillLifecycleLock, OxaError> {
        self.store.spill_lifecycle_lock(session)
    }
    fn create(&self, cwd: &Path, id: Option<&str>, parent: Option<&Path>) -> Result<SessionRef, OxaError> {
        self.store.create(cwd, id, parent)
    }
    fn append(&self, s: &SessionRef, e: &Entry) -> Result<(), OxaError> {
        self.store.append(s, e)
    }
    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError> {
        let entries = self.store.load(s)?;
        self.loaded.wait();
        self.resume.wait();
        Ok(entries)
    }
    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError> {
        self.store.header(s)
    }
    fn list(&self, cwd: Option<&Path>) -> Vec<SessionMeta> {
        self.store.list(cwd)
    }
    fn resolve(&self, id: &str) -> Result<SessionRef, OxaError> {
        self.store.resolve(id)
    }
    fn delete(&self, s: &SessionRef) -> Result<(), OxaError> {
        self.store.delete(s)
    }
}

#[test]
fn deletion_waits_for_fork_publication_including_cross_store_and_clone() {
    for cross_store in [false, true] {
        for clone in [false, true] {
            let env = oxa_fake::TestEnv::new();
            let root = env.root().join("sessions-a");
            let dst_root = if cross_store {
                env.root().join("sessions-b")
            } else {
                root.clone()
            };
            let tmp = env.root().join("tmp");
            let store = FsStore::new(root.clone());
            let cwd = Path::new("/work/project");
            let source = store.create(cwd, Some("source"), None).unwrap();
            fs::create_dir_all(tmp.join("source")).unwrap();
            let output = tmp.join("source/output.txt");
            fs::write(&output, "inherited output").unwrap();
            store
                .append(
                    &source,
                    &Entry {
                        id: "00000001".into(),
                        parent: None,
                        ts_ms: 1,
                        body: EntryBody::Msg {
                            wire: WireMsg::user(format!("full output in {}", output.display())),
                            parts: vec![],
                            tool: None,
                        },
                    },
                )
                .unwrap();
            // The long-lived writer lock must not prevent normal --fork or /clone.
            store.acquire(&source).unwrap();
            store.commit_lock();
            let loaded = Arc::new(Barrier::new(2));
            let resume = Arc::new(Barrier::new(2));
            let paused = PausedLoad {
                store: FsStore::new(dst_root.clone()),
                loaded: loaded.clone(),
                resume: resume.clone(),
            };
            let fork_source = source.clone();
            let fork = std::thread::spawn(move || {
                if clone {
                    clone_session(&paused, &fork_source, "00000001", cwd)
                } else {
                    fork_file(&paused, &fork_source, cwd, None)
                }
            });
            // The source history is loaded, but no child or cross-store link is published yet.
            loaded.wait();
            let (done_tx, done_rx) = mpsc::channel();
            let delete_source = source.clone();
            let delete_tmp = tmp.clone();
            let delete = std::thread::spawn(move || {
                let store = FsStore::new(root);
                remove_session_preserving_spills(&store, &delete_tmp, &delete_source).unwrap();
                remove_spill_dir_unless_forked(&store, &delete_tmp, &delete_source).unwrap();
                done_tx.send(()).unwrap();
            });
            let blocked = done_rx.recv_timeout(Duration::from_millis(200)).is_err();
            resume.wait();
            let child = fork.join().unwrap().unwrap();
            delete.join().unwrap();
            assert!(
                output.is_file(),
                "a successful fork must not lose its source spills"
            );
            assert!(blocked, "deletion must wait until the loaded fork is published");
            let reopened = FsStore::new(dst_root);
            assert_eq!(
                inherited_spill_dirs(&reopened, &tmp, &child),
                vec![tmp.join("source")]
            );
            remove_session_preserving_spills(&reopened, &tmp, &child).unwrap();
            remove_spill_dir_unless_forked(&reopened, &tmp, &child).unwrap();
            assert!(
                !output.exists(),
                "the final child must still reclaim the ancestor"
            );
        }
    }
}
