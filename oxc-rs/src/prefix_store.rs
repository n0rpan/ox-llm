//! Maintenance of the legacy private llama.cpp prefix store.
use nix::{
    libc,
    unistd::{unlinkat, Uid, UnlinkatFlags},
};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

fn name(value: &str) -> Option<(&str, &str)> {
    let (key, suffix) = value.split_once('.')?;
    (key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && ["bin", "json", "pending"].contains(&suffix))
    .then_some((key, suffix))
}
fn anchor(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/.", dir.as_raw_fd()))
}
fn names(dir: &File) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(anchor(dir))? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    Ok(names)
}
fn remove(dir: &File, name: &str) -> io::Result<()> {
    match unlinkat(dir, name, UnlinkatFlags::NoRemoveDir) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ENOENT) => Ok(()),
        Err(error) => Err(io::Error::from(error)),
    }
}
fn discard(dir: &File, key: &str) -> io::Result<()> {
    for suffix in ["bin", "json", "pending"] {
        remove(dir, &format!("{key}.{suffix}"))?;
    }
    Ok(())
}
fn budget(value: Option<&Value>) -> Option<f64> {
    let number = match value {
        None => 3.0 * 1024.0 * 1024.0 * 1024.0,
        Some(Value::Bool(v)) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::String(s)) => s.trim().parse::<i128>().ok()? as f64,
        Some(value) => value.as_f64()?.trunc(),
    };
    (number.is_finite() && number >= 1.0).then_some(number)
}
pub fn clear(config: &Value) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "Invalid cache budget");
    let maximum = budget(config.get("max_bytes")).ok_or_else(invalid)?;
    let root = Path::new(config["directory"].as_str().ok_or_else(invalid)?);
    if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    if dir.metadata()?.uid() != Uid::current().as_raw() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    dir.set_permissions(fs::Permissions::from_mode(0o700))?;
    for entry in names(&dir)? {
        if name(&entry).is_some_and(|(_, s)| s == "pending") {
            remove(&dir, &entry)?;
        }
    }
    let mut entries = Vec::new();
    for entry in names(&dir)? {
        let Some((key, "bin")) = name(&entry) else {
            continue;
        };
        let Ok(info) = fs::symlink_metadata(anchor(&dir).join(&entry)) else {
            continue;
        };
        if !info.is_file() || info.uid() != Uid::current().as_raw() || info.mode() & 0o077 != 0 {
            continue;
        }
        let metadata = anchor(&dir).join(format!("{key}.json"));
        let age = if metadata.exists() {
            let Ok(info) = fs::symlink_metadata(metadata) else {
                continue;
            };
            info.mtime() as f64 + info.mtime_nsec() as f64 / 1e9
        } else {
            0.0
        };
        entries.push((age, key.to_owned(), info.len()));
    }
    let mut total = entries
        .iter()
        .map(|(_, _, size)| *size as u128)
        .sum::<u128>();
    entries.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, key, size) in entries {
        if total as f64 <= maximum {
            break;
        }
        discard(&dir, &key)?;
        total -= size as u128;
    }
    for entry in names(&dir)? {
        if name(&entry).is_some() {
            remove(&dir, &entry)?;
        }
    }
    dir.sync_all()
}
