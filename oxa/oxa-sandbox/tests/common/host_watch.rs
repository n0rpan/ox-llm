//! #318: a sandboxed command must never create (even for a moment) an entry on the host
//! outside what it may write. [`HostWatch`] snapshots the listings of the watched directories
//! (the fake account home, the cwd's ancestors), polls them every few milliseconds while it
//! lives, and [`HostWatch::finish`] reports every entry that appeared or vanished, also one that
//! came and went between the snapshots.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

type Listing = Vec<(PathBuf, BTreeSet<String>)>;

fn listing(dirs: &[PathBuf]) -> Listing {
    dirs.iter()
        .map(|d| {
            let names = std::fs::read_dir(d)
                .map(|it| {
                    it.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            (d.clone(), names)
        })
        .collect()
}

fn diff(before: &Listing, now: &Listing, out: &mut BTreeSet<String>) {
    for ((d, a), (_, b)) in before.iter().zip(now) {
        for n in b.difference(a) {
            out.insert(format!("appeared: {}", d.join(n).display()));
        }
        for n in a.difference(b) {
            out.insert(format!("vanished: {}", d.join(n).display()));
        }
    }
}

pub struct HostWatch {
    dirs: Vec<PathBuf>,
    before: Listing,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<BTreeSet<String>>>,
    poller: Option<JoinHandle<()>>,
}

impl HostWatch {
    /// Watch `home` and every ancestor of `cwd` up to `root` (the test's own tree, so other
    /// tests' directories do not count; not `cwd` itself: the command may write there).
    pub fn new(root: &Path, home: &Path, cwd: &Path) -> HostWatch {
        let mut dirs: Vec<PathBuf> = vec![home.to_path_buf()];
        dirs.extend(
            cwd.ancestors()
                .skip(1)
                .filter(|a| a.starts_with(root))
                .map(Path::to_path_buf),
        );
        dirs.sort();
        dirs.dedup();
        let before = listing(&dirs);
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(BTreeSet::new()));
        let poller = {
            let (dirs, before, stop, seen) = (dirs.clone(), before.clone(), stop.clone(), seen.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut s = BTreeSet::new();
                    diff(&before, &listing(&dirs), &mut s);
                    seen.lock().expect("watch lock").extend(s);
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        HostWatch {
            dirs,
            before,
            stop,
            seen,
            poller: Some(poller),
        }
    }

    /// Stop polling; every change seen during the watch and after it, sorted.
    pub fn finish(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(p) = self.poller.take() {
            p.join().expect("watch poller");
        }
        let mut seen = self.seen.lock().expect("watch lock").clone();
        diff(&self.before, &listing(&self.dirs), &mut seen);
        seen.into_iter().collect()
    }
}
