//! Owned filesystem jobs for a caller that must keep polling other workers.
//! Cancellation is cooperative; completion always includes joining the thread.
use crate::workspace::{Grant, ReviewBundle, WorkingCopy};
use std::io;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};

struct Job<T> {
    thread: Option<JoinHandle<io::Result<T>>>,
    cancel: Arc<AtomicBool>,
}
impl<T: Send + 'static> Job<T> {
    fn start(work: impl FnOnce(Arc<AtomicBool>) -> io::Result<T> + Send + 'static) -> io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let thread = thread::Builder::new()
            .name("oxa-workspace-io".into())
            .spawn(move || {
                check(&token)?;
                let result = work(token.clone());
                check(&token)?;
                result
            })?;
        Ok(Self {
            thread: Some(thread),
            cancel,
        })
    }
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
    fn poll(&mut self) -> io::Result<Option<T>> {
        let thread = self
            .thread
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "workspace result already taken"))?;
        if !thread.is_finished() {
            return Ok(None);
        }
        let result = self
            .thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("workspace job panicked"))?;
        check(&self.cancel)?;
        result.map(Some)
    }
}
impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn check(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "workspace job canceled",
        ))
    } else {
        Ok(())
    }
}

/// One explicitly owned private-copy job. The caller must count it against its
/// lane limit and keep polling other workers while it runs. A cancellation/error
/// can leave a partial copy; there is no automatic deletion or source apply.
/// Drop requests cancellation and joins: it can wait for an in-flight OS I/O.
pub struct Preparation {
    job: Job<WorkingCopy>,
}
impl Preparation {
    pub fn start(grant: Arc<Grant>, destination: PathBuf) -> io::Result<Self> {
        Ok(Self {
            job: Job::start(move |cancel| grant.materialize_cancellable(&destination, &cancel))?,
        })
    }
    /// Requests cancellation, not proof that writes already stopped.
    pub fn cancel(&self) {
        self.job.cancel();
    }
    pub fn is_finished(&self) -> bool {
        self.job.is_finished()
    }
    /// Polls after operation completion. A returned copy/error has joined its worker.
    /// After cancellation, even an unused completed result is withdrawn.
    pub fn poll(&mut self) -> io::Result<Option<WorkingCopy>> {
        self.job.poll()
    }
}
/// Bounded review collection outside the process-polling loop. The caller must
/// first stop/reap every process with access to the copy. This object cannot
/// establish that precondition, and completion never proves process cleanup.
pub struct Collection {
    job: Job<ReviewBundle>,
}
impl Collection {
    pub fn start(copy: Arc<WorkingCopy>, worker: String) -> io::Result<Self> {
        if worker.is_empty()
            || worker.len() > 64
            || !worker
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid review worker identity",
            ));
        }
        Ok(Self {
            job: Job::start(move |cancel| {
                check(&cancel)?;
                copy.collect(&worker)
            })?,
        })
    }
    pub fn cancel(&self) {
        self.job.cancel();
    }
    pub fn is_finished(&self) -> bool {
        self.job.is_finished()
    }
    pub fn poll(&mut self) -> io::Result<Option<ReviewBundle>> {
        self.job.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{RelativePath, Snapshot};
    use oxa_fake::TestEnv;
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::time::{Duration, Instant};
    fn fixture() -> (TestEnv, Arc<Grant>, PathBuf) {
        let env = TestEnv::new();
        fs::write(env.root().join("input.txt"), "before\n").unwrap();
        let snapshot = Arc::new(
            Snapshot::approve(env.root(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap(),
        );
        let grant = Arc::new(Grant::editing(snapshot, vec![]).unwrap());
        let parent = env.root().join("private");
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        (env, grant, parent.join("copy"))
    }
    #[test]
    fn copy_and_collection_are_owned_results_and_never_apply_to_source() {
        let (env, grant, destination) = fixture();
        let mut preparation = Preparation::start(grant, destination).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        let copy = loop {
            if let Some(copy) = preparation.poll().unwrap() {
                break Arc::new(copy);
            }
            assert!(Instant::now() < end);
            thread::yield_now();
        };
        assert!(preparation.is_finished());
        assert!(preparation.poll().is_err());
        fs::write(copy.path().join("input.txt"), "after\n").unwrap();
        // No worker process is started in this fixture; the collection's
        // stopped/reaped precondition is satisfied by construction.
        let mut collection = Collection::start(copy, "writer".into()).unwrap();
        let review = loop {
            if let Some(review) = collection.poll().unwrap() {
                break review;
            }
            assert!(Instant::now() < end);
            thread::yield_now();
        };
        assert!(collection.is_finished());
        assert_eq!(review.changes.len(), 1);
        assert_eq!(review.changes[0].after.as_deref(), Some("after\n"));
        assert_eq!(
            fs::read_to_string(env.root().join("input.txt")).unwrap(),
            "before\n"
        );
    }
    #[test]
    fn cancellation_before_materialization_has_no_destination_side_effect() {
        let (_env, grant, destination) = fixture();
        let canceled = AtomicBool::new(true);
        assert!(
            matches!(grant.materialize_cancellable(&destination,&canceled),Err(e) if e.kind()==io::ErrorKind::Interrupted)
        );
        assert!(!destination.exists());
    }
    #[test]
    fn dropping_an_owned_job_cancels_and_joins_before_returning() {
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let job = Job::start(move |cancel| {
            started.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(2);
            while !cancel.load(Ordering::Acquire) && Instant::now() < end {
                thread::yield_now();
            }
            assert!(cancel.load(Ordering::Acquire));
            observed.store(true, Ordering::Release);
            Ok(())
        })
        .unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!job.is_finished());
        drop(job);
        assert!(finished.load(Ordering::Acquire));
    }
    #[test]
    fn cancel_withdraws_a_completed_unused_result_and_panics_do_not_detach() {
        let mut job = Job::start(|_| Ok(7)).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while !job.is_finished() {
            assert!(Instant::now() < end);
            thread::yield_now();
        }
        job.cancel();
        assert_eq!(job.poll().unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(job.poll().is_err());
        let mut failed = Job::<()>::start(|_| panic!("synthetic job failure")).unwrap();
        while !failed.is_finished() {
            assert!(Instant::now() < end);
            thread::yield_now();
        }
        assert!(failed.poll().is_err());
        assert!(failed.is_finished());
    }
}
