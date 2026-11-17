//! The Brave Search API key: the secret type and where it is read from.
use crate::private_file::{read_private, PrivateFileError};
use std::path::{Path, PathBuf};

/// The largest key file read; a raw API key is far smaller.
const KEY_FILE_LIMIT: u64 = 64 * 1024;

/// The API key. A secret: `Debug` and `Display` print a redaction, it is not `Serialize`, and
/// only the provider's request builder reads the bytes ([`ApiKey::expose`] is crate-private).
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

/// What `Debug` / `Display` print instead of the key.
const REDACTED: &str = "<redacted>";

impl ApiKey {
    /// A key from its text (leading/trailing whitespace trimmed). `None` when empty.
    pub fn new(text: &str) -> Option<ApiKey> {
        let t = text.trim();
        (!t.is_empty()).then(|| ApiKey(t.to_string()))
    }
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiKey({REDACTED})")
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Where the key may come from.
#[derive(Debug, Clone)]
pub struct KeyInputs {
    /// The value of `BRAVE_SEARCH_API_KEY` as read once at startup (wins over the file).
    pub env_key: Option<ApiKey>,
    /// The oxa agent directory (`<agent dir>/brave-search.key`).
    pub agent_dir: PathBuf,
    /// `OXA_TEST_ISOLATION=1`: a key file under [`KeyInputs::real_home`] is refused unread.
    pub isolation: bool,
    /// The home directory of the user running the process (passwd), see [`real_home`].
    pub real_home: Option<PathBuf>,
}

/// Where a usable key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrigin {
    Env,
    File,
}

/// The outcome of [`load_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    /// Neither the variable nor the file: the tool is not declared.
    Missing,
    Found(ApiKey, KeyOrigin),
    /// The file exists but is refused; the text is a user notice in the problem shape (it
    /// names the file, never its content).
    Refused(String),
}

impl KeyStatus {
    pub fn key(&self) -> Option<&ApiKey> {
        match self {
            KeyStatus::Found(k, _) => Some(k),
            _ => None,
        }
    }
}

fn refused(path: &Path, why: &str, fix: &str) -> KeyStatus {
    KeyStatus::Refused(oxa_proto::problem_text(
        &format!("Brave Search key file {} refused", path.display()),
        why,
        fix,
    ))
}

/// The key: `env_key` when set, else the file `<agent dir>/brave-search.key`. The file must be
/// a regular file (not a symlink) owned by this user, mode 0600 or stricter (no group or
/// other bits), at most 64 KiB and not empty after trimming; it is opened once, then checked
/// and read through that one handle. Under isolation an existing file below the real home is
/// refused before it is opened; a missing one is simply [`KeyStatus::Missing`].
pub fn load_key(i: &KeyInputs) -> KeyStatus {
    load_key_with_metadata(i, |path| std::fs::symlink_metadata(path))
}

// Keep the metadata lookup injectable so tests can reproduce a file appearing during it.
fn load_key_with_metadata(
    i: &KeyInputs,
    mut metadata: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
) -> KeyStatus {
    if let Some(k) = &i.env_key {
        return KeyStatus::Found(k.clone(), KeyOrigin::Env);
    }
    let path = i.agent_dir.join(crate::KEY_FILE);
    // A missing file stays quiet. Do not retry the lookup: a key appearing between two
    // lookups must never bypass the isolation guard. This lookup only decides "missing" and
    // the isolation refusal; every trust check below is made on the one open handle.
    if metadata(&path).is_err() {
        return KeyStatus::Missing;
    }
    if i.isolation {
        match &i.real_home {
            Some(home) if !path.starts_with(home) => {}
            _ => {
                return refused(
                    &path,
                    "OXA_TEST_ISOLATION=1 forbids reading a key file under the real home directory",
                    "point HOME / OXA_AGENT_DIR at the test's temporary directory",
                )
            }
        }
    }
    let bytes = match read_private(&path, KEY_FILE_LIMIT) {
        Ok(b) => b,
        Err(PrivateFileError::Missing) => return KeyStatus::Missing,
        Err(PrivateFileError::SymbolicLink) => {
            return refused(
                &path,
                "it is a symbolic link",
                "replace the link with the key file itself (mode 0600)",
            )
        }
        Err(PrivateFileError::NotRegular) => {
            return refused(
                &path,
                "it is not a regular file",
                "put the raw key in a regular file (mode 0600)",
            )
        }
        Err(PrivateFileError::WrongOwner) => {
            return refused(
                &path,
                "it is owned by another user",
                "recreate the key file as the user running oxa (mode 0600)",
            )
        }
        Err(PrivateFileError::Permissions(mode)) => {
            return refused(
                &path,
                &format!("it is readable by group or others (mode {mode:04o})"),
                &format!("chmod 600 {}", path.display()),
            )
        }
        Err(PrivateFileError::TooLarge(limit)) => {
            return refused(
                &path,
                &format!("it is larger than {limit} bytes"),
                "put only the raw key in the file",
            )
        }
        Err(PrivateFileError::Io(e)) => {
            return refused(
                &path,
                &format!("it cannot be read ({})", e.kind()),
                "check the file's owner and mode 0600",
            )
        }
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match ApiKey::new(&text) {
        Some(k) => KeyStatus::Found(k, KeyOrigin::File),
        None => refused(
            &path,
            "it is empty",
            "write the raw key into the file, or delete it",
        ),
    }
}

/// The home directory of the user running this process, from the password database (not
/// `HOME`, which a test points at its temporary directory).
pub fn real_home() -> Option<PathBuf> {
    // SAFETY: getpwuid_r with a caller-owned buffer and record; the result pointer is either
    // null or points into `pw`, and `pw_dir` into `buf`, both alive while read here.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out);
        if rc != 0 || out.is_null() || pw.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned();
        (!dir.is_empty()).then(|| PathBuf::from(dir))
    }
}

/// Remove `BRAVE_SEARCH_API_KEY` from the process environment, so that no child process
/// (bash, `!cmd`, exec hooks, MCP stdio servers, editors, subagents) inherits the key. Called
/// by `oxa`'s `main` right after it took its `Env` snapshot, before any thread exists.
pub fn remove_key_from_process_env() {
    std::env::remove_var(crate::ENV_KEY);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn isolation_does_not_retry_when_a_key_appears_after_a_missing_lookup() {
        for known_home in [true, false] {
            let env = TestEnv::new();
            let inputs = KeyInputs {
                env_key: None,
                agent_dir: env.agent_dir(),
                isolation: true,
                real_home: known_home.then(|| env.root().to_path_buf()),
            };
            let mut lookups = 0;
            let status = load_key_with_metadata(&inputs, |path| {
                lookups += 1;
                if lookups == 1 {
                    // The lookup sees no file, but a concurrent writer creates one before
                    // the result is handled. A second lookup would now succeed.
                    assert_eq!(
                        std::fs::symlink_metadata(path).unwrap_err().kind(),
                        std::io::ErrorKind::NotFound
                    );
                    env.write("home/.oxa/agent/brave-search.key", "file-key-not-real");
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                    return Err(std::io::ErrorKind::NotFound.into());
                }
                std::fs::symlink_metadata(path)
            });
            assert_eq!(status, KeyStatus::Missing);
            assert_eq!(lookups, 1);
            assert!(matches!(load_key(&inputs), KeyStatus::Refused(_)));
        }
    }

    fn file_inputs(env: &TestEnv) -> KeyInputs {
        KeyInputs {
            env_key: None,
            agent_dir: env.agent_dir(),
            isolation: false,
            real_home: None,
        }
    }

    fn key_file(env: &TestEnv, rel: &str, mode: u32) -> PathBuf {
        let p = env.write(rel, "file-key-not-real\n");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn a_private_key_file_is_read_through_one_handle() {
        let env = TestEnv::new();
        key_file(&env, "home/.oxa/agent/brave-search.key", 0o600);
        assert_eq!(
            load_key(&file_inputs(&env)),
            KeyStatus::Found(ApiKey::new("file-key-not-real").unwrap(), KeyOrigin::File)
        );
    }

    #[test]
    fn a_link_to_a_private_key_file_is_refused() {
        let env = TestEnv::new();
        let target = key_file(&env, "elsewhere/real.key", 0o600);
        std::fs::create_dir_all(env.agent_dir()).unwrap();
        std::os::unix::fs::symlink(&target, env.agent_dir().join(crate::KEY_FILE)).unwrap();
        match load_key(&file_inputs(&env)) {
            KeyStatus::Refused(msg) => assert!(msg.contains("symbolic link"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_group_or_world_readable_key_file_is_refused() {
        for mode in [0o640, 0o604] {
            let env = TestEnv::new();
            key_file(&env, "home/.oxa/agent/brave-search.key", mode);
            match load_key(&file_inputs(&env)) {
                KeyStatus::Refused(msg) => assert!(msg.contains("group or others"), "{msg}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn an_oversized_key_file_is_refused() {
        let env = TestEnv::new();
        let p = env.write("home/.oxa/agent/brave-search.key", &"k".repeat(KEY_FILE_LIMIT as usize + 1));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        match load_key(&file_inputs(&env)) {
            KeyStatus::Refused(msg) => assert!(msg.contains("larger than"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }
}
