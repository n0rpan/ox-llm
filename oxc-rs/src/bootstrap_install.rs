//! Create-only, hash-bound sandbox installation; no service or cloud activation.
use crate::{
    bootstrap::{self, io, relative, Result},
    paths,
};
use nix::{
    errno::Errno,
    fcntl::{open, openat, renameat, AtFlags, Flock, FlockArg, OFlag},
    sys::stat::{fchmod, fstat, fstatat, mkdirat, FileStat, Mode, SFlag},
    unistd::{dup, fsync, linkat, unlinkat, UnlinkatFlags},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const LEASE: &str = ".local/state/local-ai/inference.lock";
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn sandbox_path(raw: &Path) -> Result<PathBuf> {
    let path = if raw.is_absolute() {
        raw.to_owned()
    } else {
        std::env::current_dir().map_err(io)?.join(raw)
    };
    let home = paths::home();
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir().map_err(io)?.join(home)
    };
    if path.components().any(|p| p == Component::ParentDir)
        || path == home
        || path == Path::new("/")
        || [".config", ".local", "Pictures"]
            .iter()
            .any(|name| path.starts_with(home.join(name)))
    {
        return Err("explicit_sandbox_home_or_journal_required".into());
    }
    if let Ok(tail) = path.strip_prefix(&home) {
        if tail
            .components()
            .next()
            .is_some_and(|p| p.as_os_str().to_string_lossy().starts_with('.'))
        {
            return Err("live_hidden_home_tree_refused".into());
        }
    }
    if path
        .ancestors()
        .any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_symlink()))
    {
        return Err("symlinked_target_path_refused".into());
    }
    Ok(path)
}
fn owned_directory(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path).map_err(io)?;
    if !m.is_dir() || m.uid() != nix::unistd::getuid().as_raw() || m.mode() & 0o022 != 0 {
        return Err("unsafe_existing_directory".into());
    }
    Ok(())
}
fn absolute_dir(path: &Path) -> Result<OwnedFd> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_err(io)?.join(path)
    };
    let mut fd = open(Path::new("/"), DIR_FLAGS, Mode::empty()).map_err(io)?;
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                fd = openat(&fd, name, DIR_FLAGS, Mode::empty()).map_err(io)?
            }
            Component::ParentDir => return Err("unsafe path".into()),
            _ => {}
        }
    }
    Ok(fd)
}
fn open_dir(root: &impl AsFd, name: &Path) -> Result<OwnedFd> {
    let mut fd = dup(root).map_err(io)?;
    for part in name.components() {
        if let Component::Normal(part) = part {
            fd = openat(&fd, part, DIR_FLAGS, Mode::empty()).map_err(io)?;
            let info = fstat(&fd).map_err(io)?;
            if info.st_uid != nix::unistd::getuid().as_raw() || info.st_mode & 0o022 != 0 {
                return Err("unsafe_target_directory".into());
            }
        } else if part != Component::CurDir {
            return Err("safe relative path required".into());
        }
    }
    Ok(fd)
}
fn parent(root: &impl AsFd, name: &str) -> Result<(OwnedFd, std::ffi::OsString)> {
    let path = relative(name)?;
    let leaf = path
        .file_name()
        .ok_or("safe relative path required")?
        .to_owned();
    Ok((
        open_dir(root, path.parent().unwrap_or(Path::new("")))?,
        leaf,
    ))
}
fn identity(info: &FileStat) -> Value {
    json!({"dev":info.st_dev,"ino":info.st_ino,"mode":info.st_mode&0o7777,"mtime_ns":info.st_mtime as i128*1_000_000_000+info.st_mtime_nsec as i128})
}
fn allowed_file(name: &str) -> bool {
    [
        LEASE,
        ".local/bin/local-ai",
        ".local/bin/oxa",
        ".config/systemd/user/local-ai-chat.service",
        ".config/systemd/user/prompt-studio-app.service",
    ]
    .contains(&name)
        || [
            ".config/local-ai/",
            ".config/omarchy/plugins/snapdragon.local-ai/",
            ".local/share/ai-stack/runtime/",
            ".local/share/ai-stack/notices/",
        ]
        .iter()
        .any(|p| name.starts_with(p))
}
fn strings(value: &Value) -> Result<Vec<String>> {
    value
        .as_array()
        .ok_or("invalid plan list")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "invalid plan path".into())
        })
        .collect()
}
fn allowed_directories(files: &[String]) -> Result<BTreeSet<String>> {
    let leaves = [
        "Work",
        "Pictures/sd/_uploads",
        ".local/state/local-ai/prefix-cache",
        ".local/share/ai-stack/releases/llama-cpu",
        ".local/share/ai-stack/releases/sd-cpu-repack",
        ".local/share/ai-stack/models/llm",
        ".local/share/ai-stack/models/images/vae",
    ];
    let mut out = BTreeSet::new();
    for name in files.iter().map(String::as_str).chain(leaves) {
        let path = relative(name)?;
        let first = if leaves.contains(&name) {
            path.as_path()
        } else {
            path.parent().unwrap_or(Path::new(""))
        };
        for part in first.ancestors().filter(|p| !p.as_os_str().is_empty()) {
            out.insert(part.to_string_lossy().into_owned());
        }
    }
    Ok(out)
}
fn token() -> Result<String> {
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).map_err(io)?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}
struct Journal {
    parent: OwnedFd,
    name: std::ffi::OsString,
}
impl Journal {
    fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            parent: absolute_dir(path.parent().ok_or("journal parent required")?)?,
            name: path.file_name().ok_or("journal name required")?.into(),
        })
    }
    fn create(&self, value: &Value) -> Result<()> {
        let mut file = File::from(
            openat(
                &self.parent,
                self.name.as_os_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(io)?,
        );
        file.write_all(&bootstrap::json_bytes(value)?).map_err(io)?;
        file.sync_all().map_err(io)
    }
    fn save(&self, value: &Value) -> Result<()> {
        let temporary = format!(".{}.{}", self.name.to_string_lossy(), token()?);
        let result = (|| {
            let mut file = File::from(
                openat(
                    &self.parent,
                    temporary.as_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    Mode::from_bits_truncate(0o600),
                )
                .map_err(io)?,
            );
            file.write_all(&bootstrap::json_bytes(value)?).map_err(io)?;
            file.sync_all().map_err(io)?;
            renameat(
                &self.parent,
                temporary.as_str(),
                &self.parent,
                self.name.as_os_str(),
            )
            .map_err(io)?;
            fsync(&self.parent).map_err(io)
        })();
        if result.is_err() {
            let _ = unlinkat(&self.parent, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
        }
        result
    }
    fn sha(&self) -> Result<String> {
        bootstrap::hash(File::from(
            openat(
                &self.parent,
                self.name.as_os_str(),
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(io)?,
        ))
    }
}
fn same_inode(info: &FileStat, value: &Value) -> bool {
    json!(info.st_dev) == value["dev"] && json!(info.st_ino) == value["ino"]
}
fn matches(parent: &impl AsFd, name: &std::ffi::OsStr, row: &Value) -> bool {
    (|| -> Result<bool> {
        let mut file = File::from(
            openat(
                parent,
                name,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(io)?,
        );
        let before = fstat(&file).map_err(io)?;
        let record = identity(&before);
        if SFlag::from_bits_truncate(before.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG
            || before.st_uid != nix::unistd::getuid().as_raw()
            || json!(before.st_size) != row["bytes"]
            || ["dev", "ino", "mode", "mtime_ns"]
                .iter()
                .any(|k| record[*k] != row[*k])
        {
            return Ok(false);
        }
        let size = row["bytes"].as_u64().ok_or("invalid byte count")?;
        if size > MAX_SOURCE_BYTES {
            return Ok(false);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(size + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        Ok(bytes.len() as u64 == size
            && identity(&fstat(&file).map_err(io)?) == record
            && json!(format!("{:x}", Sha256::digest(&bytes))) == row["sha256"])
    })()
    .unwrap_or(false)
}
fn rollback_state(state: &Value) -> Result<Value> {
    let mut removed = Vec::new();
    let mut preserved = Vec::new();
    if state["root_identity"].is_null() {
        return Ok(json!({"removed":removed,"preserved":preserved}));
    }
    let target = Path::new(state["target_home"].as_str().ok_or("invalid target")?);
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"removed":[],"preserved":[],"target_already_absent":true}))
        }
        Err(e) => return Err(io(e)),
        Ok(_) => {}
    }
    owned_directory(target)?;
    let planned = strings(&state["planned_files"])?;
    let dirs = strings(&state["planned_directories"])?;
    if !planned.iter().all(|n| allowed_file(n))
        || !dirs
            .iter()
            .all(|d| allowed_directories(&planned).is_ok_and(|allowed| allowed.contains(d)))
    {
        return Err("unsafe_journal_plan".into());
    }
    let root = absolute_dir(target)?;
    if !same_inode(&fstat(&root).map_err(io)?, &state["root_identity"]) {
        return Err("target_root_identity_changed".into());
    }
    for row in state["files"]
        .as_array()
        .ok_or("invalid journal files")?
        .iter()
        .rev()
    {
        let name = row["path"].as_str().ok_or("unsafe_journal_file")?;
        if !planned.iter().any(|n| n == name) || name == LEASE {
            return Err("unsafe_journal_file".into());
        }
        if let Some(temp) = row["temporary"].as_str() {
            if temp.len() != 35
                || !temp.starts_with(".bootstrap-")
                || !temp[11..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("unsafe_journal_temporary".into());
            }
        }
        let (parent, leaf) = match parent(&root, name) {
            Ok(p) => p,
            Err(_) => {
                let path = target.join(relative(name)?);
                if path.parent().is_some_and(|p| p.exists()) {
                    preserved.push(name.to_owned());
                }
                continue;
            }
        };
        for (file, label) in [
            (Some(leaf), name.to_owned()),
            (
                row["temporary"].as_str().map(Into::into),
                format!("{name} (temporary)"),
            ),
        ] {
            let Some(file) = file else {
                continue;
            };
            match fstatat(&parent, file.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Err(Errno::ENOENT) => continue,
                Err(e) => return Err(io(e)),
                Ok(_) => {}
            }
            if matches(&parent, file.as_os_str(), row) {
                unlinkat(&parent, file.as_os_str(), UnlinkatFlags::NoRemoveDir).map_err(io)?;
                fsync(&parent).map_err(io)?;
                removed.push(label);
            } else {
                preserved.push(label);
            }
        }
    }
    for row in state["directories"]
        .as_array()
        .ok_or("invalid journal directories")?
        .iter()
        .rev()
    {
        let name = row["path"].as_str().ok_or("unsafe_journal_directory")?;
        if !dirs.iter().any(|d| d == name) {
            return Err("unsafe_journal_directory".into());
        }
        let (parent, leaf) = match parent(&root, name) {
            Ok(p) => p,
            Err(_) => {
                if target
                    .join(relative(name)?)
                    .parent()
                    .is_some_and(|p| p.exists())
                {
                    preserved.push(name.into());
                }
                continue;
            }
        };
        let info = match fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(i) => i,
            Err(Errno::ENOENT) => continue,
            Err(e) => return Err(io(e)),
        };
        if SFlag::from_bits_truncate(info.st_mode) & SFlag::S_IFMT != SFlag::S_IFDIR
            || !same_inode(&info, row)
            || json!(info.st_mode & 0o7777) != row["mode"]
        {
            preserved.push(name.into());
            continue;
        }
        match unlinkat(&parent, leaf.as_os_str(), UnlinkatFlags::RemoveDir) {
            Ok(()) => {
                fsync(&parent).map_err(io)?;
                removed.push(format!("{name}/"));
            }
            Err(Errno::ENOTEMPTY | Errno::EEXIST) => {}
            Err(e) => return Err(io(e)),
        }
    }
    if state["root_created"] == true && state["canonical"].is_null() {
        match fs::remove_dir(target) {
            Ok(()) => removed.push("(new target home)".into()),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(nix::libc::ENOTEMPTY | nix::libc::EEXIST | nix::libc::ENOENT)
                ) => {}
            Err(e) => return Err(io(e)),
        }
    }
    Ok(
        json!({"removed":removed,"preserved":preserved,"canonical_lease_preserved":!state["canonical"].is_null()}),
    )
}
fn check_disabled(stage: &Path, manifest: &Value) -> Result<()> {
    let files = manifest["files"]
        .as_object()
        .ok_or("invalid stage file inventory")?;
    let lease = &manifest["files"][LEASE];
    if lease["bytes"] != 0
        || lease["mode"] != 0o600
        || lease["sha256"] != format!("{:x}", Sha256::digest(b""))
    {
        return Err("empty_private_staged_lease_required".into());
    }
    for name in files.keys() {
        relative(name)?;
        if !allowed_file(name) {
            return Err(format!("unsupported_payload_path_{name}"));
        }
    }
    let allowed = allowed_directories(&files.keys().cloned().collect::<Vec<_>>())?;
    if !strings(&manifest["directories"])?
        .iter()
        .all(|d| allowed.contains(d))
    {
        return Err("unsupported_payload_directory".into());
    }
    let config = bootstrap::read_json(&stage.join("payload/.config/local-ai/llm/runtime.json"))?;
    if config["ready"] != false
        || config["profiles"]
            .as_object()
            .is_some_and(|p| p.values().any(|v| v["ready"] != false))
    {
        return Err("source_install_cannot_enable_profiles".into());
    }
    for name in ["cpu-profile.json", "gpu-profile.json"] {
        let path = stage.join("payload/.config/local-ai/images").join(name);
        if path.exists() {
            let value = bootstrap::read_json(&path)?;
            if value["enabled"] != false || value["validated"] != false {
                return Err("source_install_cannot_validate_images".into());
            }
        }
    }
    Ok(())
}
struct Outcome {
    value: Value,
    lease: Option<Flock<File>>,
    target: Option<PathBuf>,
}
fn install(stage: &Path, target: &Path, journal: &Path, wanted: &str) -> Result<Outcome> {
    let target = sandbox_path(target)?;
    let journal_path = sandbox_path(journal)?;
    if exists(&journal_path) {
        return Err("fresh_journal_required".into());
    }
    owned_directory(journal_path.parent().ok_or("journal parent required")?)?;
    owned_directory(target.parent().ok_or("target parent required")?)?;
    if journal_path.starts_with(&target) {
        return Err("journal_must_live_outside_target_home".into());
    }
    if bootstrap::sha(&stage.join("stage-manifest.json"))? != wanted {
        return Err("stage_manifest_identity_mismatch".into());
    }
    let manifest = bootstrap::read_json(&stage.join("stage-manifest.json"))?;
    let plan = bootstrap::read_json(&stage.join("bootstrap-plan.json"))?;
    let files = manifest["files"]
        .as_object()
        .ok_or("invalid stage file inventory")?;
    let size = files
        .values()
        .try_fold(0u64, |sum, row| {
            sum.checked_add(row["bytes"].as_u64().unwrap_or(u64::MAX))
        })
        .ok_or("source_payload_only_size_limit")?;
    if size > MAX_SOURCE_BYTES {
        return Err("source_payload_only_size_limit".into());
    }
    bootstrap::verify_stage(stage)?;
    let planned_target = Path::new(
        plan["target_home"]
            .as_str()
            .ok_or("stage_target_home_mismatch")?,
    );
    if sandbox_path(planned_target)? != target {
        return Err("stage_target_home_mismatch".into());
    }
    if plan["end_to_end_parity_passed"] != false || manifest["installed"] != false {
        return Err("unqualified_fresh_stage_required".into());
    }
    check_disabled(stage, &manifest)?;
    if target.exists() {
        owned_directory(&target)?;
    }
    for name in strings(&manifest["directories"])? {
        let path = target.join(relative(&name)?);
        if exists(&path) {
            owned_directory(&path)?;
        }
    }
    for name in files.keys() {
        let path = target.join(relative(name)?);
        if exists(&path) {
            if name == LEASE {
                let m = fs::symlink_metadata(path).map_err(io)?;
                if !m.is_file()
                    || m.uid() != nix::unistd::getuid().as_raw()
                    || m.mode() & 0o7777 != 0o600
                {
                    return Err("unsafe_existing_canonical_lease".into());
                }
            } else {
                return Err(format!("existing_managed_file_preserved_{name}"));
            }
        }
    }
    let mut state = json!({"schema":1,"kind":"sandbox-source-install","status":"installing","target_home":target,"stage_manifest_sha256":wanted,"installed":false,"qualified":false,"services_started":false,"panel_layout_changed":false,"files":[],"directories":[],"canonical":null,"root_identity":null,"root_created":false,"planned_files":files.keys().collect::<Vec<_>>(),"planned_directories":manifest["directories"]});
    let target_parent = absolute_dir(target.parent().ok_or("target parent required")?)?;
    let target_name = target.file_name().ok_or("target name required")?;
    let journal = Journal::new(&journal_path)?;
    journal.create(&state)?;
    let mut held = None;
    let result = (|| {
        if !target.exists() {
            mkdirat(&target_parent, target_name, Mode::from_bits_truncate(0o700)).map_err(io)?;
            state["root_created"] = json!(true);
        }
        let root = openat(&target_parent, target_name, DIR_FLAGS, Mode::empty()).map_err(io)?;
        state["root_identity"] = identity(&fstat(&root).map_err(io)?);
        journal.save(&state)?;
        let source = absolute_dir(&stage.join("payload"))?;
        let mut directories = strings(&manifest["directories"])?;
        directories.sort_by_key(|s| (Path::new(s).components().count(), s.clone()));
        for name in directories {
            let (parent, leaf) = parent(&root, &name)?;
            match mkdirat(&parent, leaf.as_os_str(), Mode::from_bits_truncate(0o700)) {
                Err(Errno::EEXIST) => continue,
                Err(e) => return Err(io(e)),
                Ok(()) => {}
            }
            let mut row = identity(
                &fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io)?,
            );
            row["path"] = json!(name);
            state["directories"]
                .as_array_mut()
                .expect("directory rows")
                .push(row);
            fsync(&parent).map_err(io)?;
            journal.save(&state)?;
        }
        let (lease_parent, lease_name) = parent(&root, LEASE)?;
        let (lease, created) = match openat(
            &lease_parent,
            lease_name.as_os_str(),
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        ) {
            Ok(fd) => (fd, true),
            Err(Errno::EEXIST) => (
                openat(
                    &lease_parent,
                    lease_name.as_os_str(),
                    OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io)?,
                false,
            ),
            Err(e) => return Err(io(e)),
        };
        let info = fstat(&lease).map_err(io)?;
        if SFlag::from_bits_truncate(info.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG
            || info.st_uid != nix::unistd::getuid().as_raw()
            || info.st_mode & 0o7777 != 0o600
        {
            return Err("unsafe_canonical_lease".into());
        }
        state["canonical"] = identity(&info);
        state["canonical"]["created"] = json!(created);
        let locked = Flock::lock(File::from(lease), FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| io(e))?;
        locked.sync_all().map_err(io)?;
        fsync(&lease_parent).map_err(io)?;
        held = Some(locked);
        journal.save(&state)?;
        for (name, row) in files {
            if name == LEASE {
                continue;
            }
            let (parent, leaf) = parent(&root, name)?;
            let (source_parent, source_leaf) = self::parent(&source, name)?;
            let mut incoming = File::from(
                openat(
                    &source_parent,
                    source_leaf.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io)?,
            );
            let mut data = Vec::new();
            Read::by_ref(&mut incoming)
                .take(MAX_SOURCE_BYTES + 1)
                .read_to_end(&mut data)
                .map_err(io)?;
            if json!(data.len()) != row["bytes"]
                || json!(format!("{:x}", Sha256::digest(&data))) != row["sha256"]
            {
                return Err("source_payload_changed".into());
            }
            let temporary = format!(".bootstrap-{}", token()?);
            let mode = Mode::from_bits_truncate(
                row["mode"].as_u64().ok_or("invalid staged file mode")? as u32,
            );
            let mut out = File::from(
                openat(
                    &parent,
                    temporary.as_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    mode,
                )
                .map_err(io)?,
            );
            if let Err(error) = (|| {
                out.write_all(&data).map_err(io)?;
                fchmod(&out, mode).map_err(io)?;
                out.sync_all().map_err(io)
            })() {
                let _ = unlinkat(&parent, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
                return Err(error);
            }
            let info = match fstat(&out) {
                Ok(info) => info,
                Err(error) => {
                    let _ = unlinkat(&parent, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
                    return Err(io(error));
                }
            };
            let mut entry = identity(&info);
            entry["path"] = json!(name);
            entry["temporary"] = json!(temporary);
            entry["bytes"] = row["bytes"].clone();
            entry["sha256"] = row["sha256"].clone();
            state["files"]
                .as_array_mut()
                .expect("file rows")
                .push(entry);
            journal.save(&state)?;
            linkat(
                &parent,
                temporary.as_str(),
                &parent,
                leaf.as_os_str(),
                AtFlags::empty(),
            )
            .map_err(io)?;
            fsync(&parent).map_err(io)?;
            journal.save(&state)?;
            unlinkat(&parent, temporary.as_str(), UnlinkatFlags::NoRemoveDir).map_err(io)?;
            fsync(&parent).map_err(io)?;
            state["files"]
                .as_array_mut()
                .expect("file rows")
                .last_mut()
                .expect("published row")["temporary"] = Value::Null;
            journal.save(&state)?;
        }
        state["status"] = json!("installed_unqualified");
        state["installed"] = json!(true);
        journal.save(&state)?;
        Ok(
            json!({"installed_source_payload":true,"agent_identity":if files.contains_key(".local/bin/oxa"){"pending"}else{"absent"},"qualified":false,"services_started":false,"panel_layout_changed":false,"journal_sha256":journal.sha()?,"canonical_inode":state["canonical"]["ino"]}),
        )
    })();
    if let Err(error) = result {
        state["error"] = json!(if error.starts_with("EAGAIN") {
            "BlockingIOError"
        } else {
            "InstallError"
        });
        state["rollback"] = rollback_state(&state)?;
        state["status"] = json!("rolled_back_after_failure");
        state["installed"] = json!(false);
        journal.save(&state)?;
        return Err(error);
    }
    result.map(|value| Outcome {
        value,
        lease: held,
        target: Some(target),
    })
}
fn rollback(journal: &Path, wanted: &str) -> Result<Value> {
    let path = sandbox_path(journal)?;
    if bootstrap::sha(&path)? != wanted {
        return Err("journal_identity_mismatch".into());
    }
    let mut state = bootstrap::read_json(&path)?;
    if state["kind"] != "sandbox-source-install" || state["schema"] != 1 {
        return Err("unsupported_journal".into());
    }
    let target = sandbox_path(Path::new(
        state["target_home"].as_str().ok_or("invalid target")?,
    ))?;
    let _lease = if target.exists() && !state["canonical"].is_null() {
        let root = absolute_dir(&target)?;
        let (parent, name) = parent(&root, LEASE)?;
        let fd = openat(
            &parent,
            name.as_os_str(),
            OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        if !same_inode(&fstat(&fd).map_err(io)?, &state["canonical"]) {
            return Err("canonical_inode_changed".into());
        }
        Some(Flock::lock(File::from(fd), FlockArg::LockExclusiveNonblock).map_err(|(_, e)| io(e))?)
    } else {
        None
    };
    let mut result = rollback_state(&state)?;
    state["status"] = json!("rolled_back");
    state["installed"] = json!(false);
    state["rollback"] = result.clone();
    let journal = Journal::new(&path)?;
    journal.save(&state)?;
    result["qualified"] = json!(false);
    result["services_started"] = json!(false);
    result["journal_sha256"] = json!(journal.sha()?);
    Ok(result)
}
pub async fn cli(words: &[String]) -> i32 {
    if words
        .iter()
        .take_while(|w| w.as_str() != "--")
        .any(|w| matches!(w.as_str(), "--help" | "-h"))
    {
        print!(
            "{}",
            match words.first().map(String::as_str) {
                Some("apply") => include_str!("bootstrap-install-apply-help.txt"),
                Some("rollback") => include_str!("bootstrap-install-rollback-help.txt"),
                _ => include_str!("bootstrap-install-help.txt"),
            }
        );
        return 0;
    }
    let result = (|| {
        let action = words.first().map(String::as_str).ok_or("action required")?;
        let mut args = std::collections::HashMap::new();
        let mut index = 1;
        while index < words.len() {
            let (name, inline) = words[index]
                .split_once('=')
                .map_or((words[index].as_str(), None), |(k, v)| (k, Some(v)));
            if ![
                "--stage",
                "--target-home",
                "--journal",
                "--stage-manifest-sha256",
                "--journal-sha256",
            ]
            .contains(&name)
            {
                return Err("unknown install option".into());
            }
            let value = if let Some(value) = inline {
                value
            } else {
                index += 1;
                words
                    .get(index)
                    .ok_or("install option needs a value")?
                    .as_str()
            };
            args.insert(name, value);
            index += 1;
        }
        let get = |name| {
            args.get(name)
                .copied()
                .ok_or_else(|| format!("required option {name}"))
        };
        match action {
            "apply" => install(
                Path::new(get("--stage")?),
                Path::new(get("--target-home")?),
                Path::new(get("--journal")?),
                get("--stage-manifest-sha256")?,
            ),
            "rollback" => {
                rollback(Path::new(get("--journal")?), get("--journal-sha256")?).map(|value| {
                    Outcome {
                        value,
                        lease: None,
                        target: None,
                    }
                })
            }
            _ => Err("unknown install action".into()),
        }
    })();
    match result {
        Ok(Outcome {
            mut value,
            lease: _lease,
            target,
        }) => {
            if value["agent_identity"] == "pending" {
                let target = target.as_deref().expect("validated installed target");
                let mut command = tokio::process::Command::new(target.join(".local/bin/oxa"));
                command
                    .args(["identity", "--refresh", "--offline"])
                    .env("HOME", target)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                let good = match command.spawn() {
                    Ok(mut child) => {
                        match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
                            Ok(Ok(status)) => status.success(),
                            _ => {
                                let _ = child.kill().await;
                                let _ = child.wait().await;
                                false
                            }
                        }
                    }
                    Err(_) => false,
                };
                value["agent_identity"] = json!(if good { "refreshed-offline" } else { "failed" });
            }
            drop(_lease);
            println!(
                "{}",
                serde_json::to_string_pretty(&value).expect("install result")
            );
            0
        }
        Err(error) => {
            eprintln!("bootstrap-install: {error}");
            1
        }
    }
}
