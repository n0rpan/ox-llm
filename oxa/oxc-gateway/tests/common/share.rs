use oxc_gateway::share::keys::{Limits, Store};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

pub struct Directory(pub PathBuf);
impl Directory {
    pub fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("guest-key-test-{name}"));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    pub fn store(&self) -> Store {
        Store::open(&self.0).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub fn limits() -> Limits {
    Limits {
        daily_requests: 10,
        daily_tokens: 1000,
    }
}
