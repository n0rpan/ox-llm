use nix::unistd::{Uid, User};
use std::path::PathBuf;

/// Read the real user's account home as a native path, independently of `HOME`.
pub fn account_home() -> Option<PathBuf> {
    let home = User::from_uid(Uid::current()).ok()??.dir;
    (!home.as_os_str().is_empty()).then_some(home)
}
