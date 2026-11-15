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
        let root = std::env::temp_dir().join(format!(
            "oxc-skeleton-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
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
                        "base_url":mock.base_url(),
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
