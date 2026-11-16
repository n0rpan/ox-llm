#![cfg(all(feature = "upstream-openai", feature = "secrets"))]

use oxc_gateway::openai::chatgpt_tokens::{self, Error, TokenRecord};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Barrier,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ACCESS: &str = "synthetic-access-value";
const REFRESH: &str = "synthetic-refresh-value";
const ACCOUNT: &str = "synthetic-account-value";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "chatgpt-tokens-{}-{}-{}",
            std::process::id(),
            now(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn tokens(&self, expires: f64) -> PathBuf {
        let path = self.0.join("chatgpt-auth.json");
        write_record(&path, expires);
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn write_record(path: &Path, expires: f64) {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    write!(file, "{}", serde_json::json!({"access_token": ACCESS, "refresh_token": REFRESH, "account_id": ACCOUNT, "expires_at": expires})).unwrap();
}
fn renewed() -> TokenRecord {
    TokenRecord::new(ACCESS.into(), REFRESH.into(), ACCOUNT.into(), now() + 3600.0).unwrap()
}
fn never(_: &TokenRecord) -> Result<TokenRecord, &'static str> {
    panic!("fresh tokens must not refresh")
}

#[test]
fn python_record_margin_and_redacted_debug() {
    let temp = Temp::new();
    let path = temp.tokens(now() + 3600.0);
    let record = chatgpt_tokens::credentials(&path, never).unwrap();
    assert_eq!(record.access_token(), ACCESS);
    assert_eq!(record.refresh_token(), REFRESH);
    assert_eq!(record.account_id(), ACCOUNT);
    assert!(!record.is_fresh_at(record.expires_at() - 120.0));
    assert!(record.is_fresh_at(record.expires_at() - 120.001));
    for secret in [ACCESS, REFRESH, ACCOUNT] {
        assert!(!format!("{record:?}").contains(secret));
    }
}

#[test]
fn threads_refresh_once_and_replace_atomically() {
    let temp = Temp::new();
    let path = temp.tokens(now() + 119.0);
    let original = fs::File::open(&path).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let (path, calls, barrier) = (&path, &calls, &barrier);
            scope.spawn(move || {
                barrier.wait();
                let record = chatgpt_tokens::credentials(path, |old| {
                    assert_eq!(old.refresh_token(), REFRESH);
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                    Ok::<_, ()>(renewed())
                })
                .unwrap();
                assert!(record.is_fresh_at(now()));
            });
        }
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_ne!(original.metadata().unwrap().ino(), meta.ino());
    assert_eq!(
        fs::metadata(temp.0.join("chatgpt-auth.lock")).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 2);
}

#[test]
fn rejects_links_permissions_oversize_and_nonregular_files() {
    let temp = Temp::new();
    let path = temp.tokens(now() + 3600.0);
    let alias = temp.0.join("alias.json");
    symlink(&path, &alias).unwrap();
    assert!(chatgpt_tokens::read(&alias).is_err());
    fs::remove_file(&alias).unwrap();
    fs::hard_link(&path, &alias).unwrap();
    assert!(chatgpt_tokens::read(&path).is_err());
    fs::remove_file(alias).unwrap();
    for mode in [0o640, 0o604, 0o660] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(chatgpt_tokens::read(&path).is_err());
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, vec![b' '; 65537]).unwrap();
    assert!(chatgpt_tokens::read(&path).is_err());
    assert!(chatgpt_tokens::read(&temp.0).is_err());
    let fifo = temp.0.join("fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
    assert!(chatgpt_tokens::read(&fifo).is_err());
}

#[test]
fn lock_link_is_refused_without_refreshing() {
    let temp = Temp::new();
    let path = temp.tokens(0.0);
    symlink(&path, temp.0.join("chatgpt-auth.lock")).unwrap();
    assert!(chatgpt_tokens::credentials(&path, never).is_err());
    assert_eq!(chatgpt_tokens::read(&path).unwrap().expires_at(), 0.0);
}

#[test]
fn errors_never_contain_tokens_and_failure_keeps_original() {
    let temp = Temp::new();
    let path = temp.tokens(0.0);
    let before = fs::read(&path).unwrap();
    let error = chatgpt_tokens::credentials(&path, |_| {
        Err::<TokenRecord, _>(format!("{ACCESS} {REFRESH} {ACCOUNT}"))
    })
    .unwrap_err();
    assert_eq!(error, Error::Refresh);
    assert_eq!(fs::read(&path).unwrap(), before);
    let invalid = chatgpt_tokens::credentials(&path, |_| {
        Ok::<_, ()>(TokenRecord::new(ACCESS.into(), REFRESH.into(), ACCOUNT.into(), 0.0).unwrap())
    })
    .unwrap_err();
    assert_eq!(invalid, Error::InvalidRecord);
    fs::write(
        &path,
        format!("{{\"access_token\":\"{ACCESS}\",{REFRESH} {ACCOUNT}"),
    )
    .unwrap();
    let parse = chatgpt_tokens::read(&path).unwrap_err();
    for secret in [ACCESS, REFRESH, ACCOUNT] {
        assert!(!format!("{error} {error:?} {parse} {parse:?} {invalid:?}").contains(secret));
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "child synchronization timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn process_refresh_worker() {
    let Some(dir) = std::env::var_os("CHATGPT_TOKEN_TEST_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let id = std::env::var("CHATGPT_TOKEN_TEST_WORKER").unwrap();
    fs::write(dir.join(format!("ready-{id}")), b"").unwrap();
    wait_for(&dir.join("start"));
    let record = chatgpt_tokens::credentials(&dir.join("chatgpt-auth.json"), |_| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(dir.join("refresh-called"))
            .unwrap();
        file.write_all(b"once").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        Ok::<_, ()>(renewed())
    })
    .unwrap();
    assert!(record.is_fresh_at(now()));
}

#[test]
fn processes_refresh_once() {
    let temp = Temp::new();
    temp.tokens(0.0);
    let mut children = (0..2)
        .map(|id| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process_refresh_worker", "--nocapture"])
                .env("CHATGPT_TOKEN_TEST_DIR", &temp.0)
                .env("CHATGPT_TOKEN_TEST_WORKER", id.to_string())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for id in 0..2 {
        wait_for(&temp.0.join(format!("ready-{id}")));
    }
    fs::write(temp.0.join("start"), b"").unwrap();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(fs::read(temp.0.join("refresh-called")).unwrap(), b"once");
}

#[test]
fn nonblocking_refresh_reports_only_busy_and_preserves_checks() {
    use nix::fcntl::{Flock, FlockArg};
    let temp = Temp::new();
    let path = temp.tokens(1.0);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp.0.join("chatgpt-auth.lock"))
        .unwrap();
    let held = Flock::lock(file, FlockArg::LockExclusive).unwrap();
    let before = fs::read(&path).unwrap();
    let busy = chatgpt_tokens::try_credentials(&path, |_| -> Result<TokenRecord, Error> {
        panic!("busy lock must not call refresh")
    })
    .unwrap();
    assert!(busy.is_none());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(held);
    let record = chatgpt_tokens::try_credentials(&path, |_| Ok::<_, Error>(renewed()))
        .unwrap()
        .unwrap();
    assert_eq!(record.access_token(), ACCESS);
    let fresh = chatgpt_tokens::try_credentials(&path, never).unwrap().unwrap();
    assert_eq!(fresh.access_token(), ACCESS);
    let linked = temp.0.join("linked.json");
    symlink(&path, &linked).unwrap();
    assert!(matches!(
        chatgpt_tokens::try_credentials(&linked, never),
        Err(Error::Unavailable)
    ));
}
