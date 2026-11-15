#![cfg(feature = "secrets")]
use oxc_gateway::secrets::{self, Error, KeyFile};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
struct Home(PathBuf);
static NEXT_HOME: AtomicU64 = AtomicU64::new(0);
impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "oxc-key-path-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn key(&self, name: &str, value: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::json!({"openrouter":{"api_key":value}}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
#[test]
fn prepared_locations_cannot_be_redirected_into_a_repository() {
    let home = Home::new();
    let allowed = home.key("outside/provider.json", "synthetic-allowed");
    let inside = home.key("repository/provider.json", "synthetic-repository");
    fs::create_dir(inside.parent().unwrap().join(".git")).unwrap();
    let link = home.0.join("selected");
    symlink(allowed.parent().unwrap(), &link).unwrap();
    let path = link.join("provider.json");
    let prepared = KeyFile::prepare(&path).unwrap();
    fs::remove_file(&link).unwrap();
    symlink(inside.parent().unwrap(), &link).unwrap();
    assert_eq!(
        secrets::read(&path, "openrouter").unwrap_err(),
        Error::InsideRepository
    );
    assert_eq!(
        prepared.read("openrouter").unwrap().expose(),
        "synthetic-allowed"
    );
    assert!(!format!("{prepared:?}").contains(&home.0.display().to_string()));
}
#[test]
fn preparation_caches_no_key_and_rechecks_each_opened_file() {
    let home = Home::new();
    let path = home.key("provider.json", "synthetic-first");
    let prepared = KeyFile::prepare(&path).unwrap();
    assert_eq!(
        prepared.read("openrouter").unwrap().expose(),
        "synthetic-first"
    );
    let replacement = home.key("replacement.json", "synthetic-second");
    fs::rename(replacement, &path).unwrap();
    assert_eq!(
        prepared.read("openrouter").unwrap().expose(),
        "synthetic-second"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(prepared.read("openrouter").unwrap_err(), Error::Permissions);
    fs::remove_file(&path).unwrap();
    let other = home.key("other.json", "synthetic-other");
    symlink(other, path).unwrap();
    assert_eq!(
        prepared.read("openrouter").unwrap_err(),
        Error::SymbolicLink
    );
}
#[test]
fn preparation_requires_a_real_parent_before_it_can_bind_the_location() {
    let home = Home::new();
    let path = home.0.join("not-created/provider.json");
    // An unresolved parent must not be approved and then created as a link
    // into a repository after the single location validation.
    assert!(KeyFile::prepare(&path).is_err());
    fs::create_dir(path.parent().unwrap()).unwrap();
    assert!(KeyFile::prepare(&path).is_ok());
}
