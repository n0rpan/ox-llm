//! Private synthetic inputs for the production Runtime constructor.
use super::anthropic::{MockAnthropic, CREDENTIAL};
use serde_json::{json, Value};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub const MODEL: &str = "synthetic-cloud";
pub const UPSTREAM: &str = "synthetic-anthropic";

pub struct Fixture {
    root: PathBuf,
    pub key_path: PathBuf,
    pub ledger_dir: PathBuf,
    pub config: Value,
}
impl Fixture {
    pub fn new(mock: &MockAnthropic) -> Self {
        Self::for_endpoint(&mock.base_url())
    }
    pub fn for_endpoint(endpoint: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "oxc-skeleton-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let root = create_root(root);
        let key_path = root.join("credentials.json");
        let mut key = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
            .unwrap();
        write!(key, "{}", json!({"anthropic":{"api_key":CREDENTIAL}})).unwrap();
        let ledger_dir = root.join("ledger");
        DirBuilder::new().mode(0o700).create(&ledger_dir).unwrap();
        let config = json!({
            "cloud_only":true,
            "cloud":{
                "enabled":true,
                "daily_budget_usd":1,
                "models":{
                    MODEL:{
                        "kind":"remote/anthropic",
                        "upstream_model":UPSTREAM,
                        "base_url":endpoint,
                        "context_window":1024,
                        "max_output":16,
                        "pricing":{
                            "input_per_million":2,
                            "output_per_million":3,
                            "cache_read_per_million":0.2,
                            "cache_write_per_million":2.5
                        }
                    }
                }
            }
        });
        Self {
            root,
            key_path,
            ledger_dir,
            config,
        }
    }
}
fn create_root(root: PathBuf) -> PathBuf {
    let mut candidate = root.clone();
    let mut suffix = 0u64;
    loop {
        match DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return candidate,
            // Clocks can repeat across threads. Never reuse another fixture's directory.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix += 1;
                candidate = root.with_extension(suffix.to_string());
            }
            Err(error) => panic!("cannot create private fixture directory: {error}"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn colliding_fixture_roots_remain_private_and_independent() {
        let first = Fixture::for_endpoint("http://127.0.0.1:1");
        let marker = first.root.join("existing-marker");
        fs::write(&marker, b"preserve existing fixture").unwrap();
        let second = create_root(first.root.clone());
        assert_ne!(first.root, second);
        assert_eq!(fs::metadata(&second).unwrap().permissions().mode() & 0o777, 0o700);
        fs::remove_dir_all(second).unwrap();
        assert_eq!(fs::read(marker).unwrap(), b"preserve existing fixture");
        assert!(first.key_path.is_file());
    }
}
