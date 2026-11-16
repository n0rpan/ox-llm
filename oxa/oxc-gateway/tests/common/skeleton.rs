//! Private synthetic inputs for the production Runtime constructor.
use super::anthropic::{MockAnthropic, CREDENTIAL};
use serde_json::{json, Value};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = loop {
            let path = std::env::temp_dir().join(format!(
                "oxc-skeleton-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create private synthetic fixture: {error}"),
            }
        };
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
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
