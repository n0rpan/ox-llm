//! Exact-record removal. Owner edits and unrecorded files are retained.
use crate::paths;
use nix::{
    fcntl::{open, openat, AtFlags, OFlag},
    sys::stat::{fstatat, Mode, SFlag},
    unistd::{unlinkat, Uid, UnlinkatFlags},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const NAMED: [&str; 2] = ["oxc-chat.service", "prompt-studio-app.service"];
const PLACES: [&str; 9] = [
    ".local/bin/oxc",
    ".local/bin/oxa",
    ".local/bin/launch_local_plan.py",
    ".config/oxc",
    ".local/state/oxc",
    ".local/share/ai-stack/runtime",
    ".local/share/ai-stack/runtime-releases",
    ".local/share/ai-stack/releases",
    ".config/omarchy/plugins/snapdragon.local-ai",
];
const CLIENT_NOTE:&str="Client configurations outside ~/.config/oxc are yours and are left in place; a client set up for http://127.0.0.1:8092/v1 stops working until ox-llm is installed again.";
type Reasons = Vec<(String, String)>;
type Owned = BTreeMap<PathBuf, BTreeSet<String>>;
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn norm(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                if out.file_name().is_some_and(|name| name != "..") {
                    out.pop();
                } else if !path.is_absolute() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            _ => out.push(part.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}
fn read_json(path: &Path) -> Option<Value> {
    let info = fs::symlink_metadata(path).ok()?;
    if !info.is_file() || info.len() > 8 * 1024 * 1024 {
        return None;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 8 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}
fn sha(mut file: File) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn owned_now(path: &Path, wants: &BTreeSet<String>) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        && File::open(path)
            .and_then(sha)
            .is_ok_and(|h| wants.contains(&h))
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(_) => v.as_f64().is_some_and(|n| n != 0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
fn record_files(doc: &Value) -> (Vec<(PathBuf, String)>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    if doc["kind"] == "sandbox-source-install" {
        if !truthy(&doc["installed"]) {
            return (files, dirs);
        }
        let Some(home) = doc["target_home"].as_str() else {
            return (files, dirs);
        };
        for row in doc["files"].as_array().into_iter().flatten() {
            if let (Some(path), Some(want)) = (row["path"].as_str(), row["sha256"].as_str()) {
                let path = Path::new(path);
                if !path.is_absolute() && !path.components().any(|c| c == Component::ParentDir) {
                    files.push((Path::new(home).join(path), want.into()));
                }
            }
        }
    } else {
        if let Some(paths) = doc["paths"].as_object() {
            if let Some(hashes) = doc["installed_sha256"].as_object() {
                for (key, value) in paths {
                    if let (Some(path), Some(want)) =
                        (value.as_str(), hashes.get(key).and_then(Value::as_str))
                    {
                        if Path::new(path).is_absolute() && want != "absent" {
                            files.push((PathBuf::from(path), want.into()));
                        }
                    }
                }
            }
        }
        if let Some(release) = doc["release_dir"]
            .as_str()
            .filter(|s| Path::new(s).is_absolute())
        {
            if let Some(hashes) = doc["runtime_source_sha256"].as_object() {
                for (name, want) in hashes {
                    if !name.contains('/') && !["", ".", ".."].contains(&name.as_str()) {
                        if let Some(want) = want.as_str() {
                            files.push((Path::new(release).join(name), want.into()));
                        }
                    }
                }
                dirs.push(PathBuf::from(release));
            }
        }
        if let Some(engine) = doc["engine_dir"]
            .as_str()
            .filter(|s| Path::new(s).is_absolute())
        {
            for (name, key) in [("oxe-server", "server_sha256"), ("oxe", "oxe_sha256")] {
                if let Some(want) = doc[key].as_str() {
                    files.push((Path::new(engine).join(name), want.into()));
                }
            }
            dirs.push(PathBuf::from(engine));
        }
    }
    (files, dirs)
}
fn records(home: &Path, extra: &[PathBuf]) -> (Owned, Vec<PathBuf>, Vec<String>) {
    let state = read_json(&home.join(".local/state/oxc/update/state.json")).unwrap_or(Value::Null);
    let mut sources = Vec::new();
    for entry in
        std::iter::once(&state["current"]).chain(state["history"].as_array().into_iter().flatten())
    {
        if let Some(stage) = entry["stage"].as_str() {
            if entry.get("action").is_none_or(|a| a == "update")
                && entry.get("result").is_none_or(|r| r == "installed")
            {
                let source = Path::new(stage).join("manifest.json");
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
        }
    }
    sources.extend(extra.iter().cloned());
    let (mut owned, mut dirs, mut used) = (Owned::new(), Vec::new(), Vec::new());
    for source in sources {
        if let Some(doc) = read_json(&source) {
            let (files, more) = record_files(&doc);
            if !files.is_empty() || !more.is_empty() {
                used.push(source.to_string_lossy().into_owned());
            }
            for (path, want) in files {
                owned.entry(norm(&path)).or_default().insert(want);
            }
            for dir in more {
                let dir = norm(&dir);
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
    }
    (owned, dirs, used)
}
fn check_home() -> Result<PathBuf, String> {
    let home = paths::home();
    if !home.is_absolute() {
        return Err(format!("home must be an absolute path: {}", home.display()));
    }
    let home = norm(&home);
    if home == Path::new("/") || paths::realpath(&home).is_ok_and(|p| p == Path::new("/")) {
        return Err("refusing to work with home = /".into());
    }
    if !home.is_dir() {
        return Err(format!("home is not a directory: {}", home.display()));
    }
    Ok(home)
}
fn vet(home: &Path, path: &Path) -> Option<&'static str> {
    if !path.starts_with(home) {
        return Some("outside home");
    }
    if !paths::realpath(path.parent().unwrap_or_else(|| Path::new(".")))
        .ok()?
        .starts_with(paths::realpath(home).ok()?)
    {
        return Some("a parent directory is a symlink that leaves home");
    }
    if let Ok(info) = fs::symlink_metadata(path) {
        if info.uid() != Uid::current().as_raw() {
            return Some("not owned by the current user");
        }
        if !info.file_type().is_symlink() && !info.is_file() {
            return Some("not a regular file");
        }
    }
    None
}
fn vet_dir(home: &Path, path: &Path) -> Option<&'static str> {
    if !path.starts_with(home) {
        return Some("outside home");
    }
    let Ok(info) = fs::symlink_metadata(path) else {
        return Some("gone");
    };
    if !info.is_dir() {
        return Some("not a real directory");
    }
    let real_home = paths::realpath(home).ok()?;
    if !paths::realpath(path.parent().unwrap_or_else(|| Path::new(".")))
        .ok()?
        .starts_with(&real_home)
        || !paths::realpath(path).ok()?.starts_with(&real_home)
    {
        return Some("resolves outside home (a parent is a symlink that leaves home)");
    }
    if info.uid() != Uid::current().as_raw() {
        return Some("not owned by the current user");
    }
    None
}
fn models(home: &Path) -> Vec<PathBuf> {
    fn walk(value: &Value, out: &mut Vec<PathBuf>) {
        match value {
            Value::Object(v) => {
                for (key, child) in v {
                    if ["model_path", "sidecar_path", "draft_model_path", "mtp_path"]
                        .contains(&key.as_str())
                        && child.as_str().is_some_and(|s| Path::new(s).is_absolute())
                    {
                        let path = norm(Path::new(child.as_str().unwrap_or("")));
                        if !out.contains(&path) {
                            out.push(path);
                        }
                    } else {
                        walk(child, out);
                    }
                }
            }
            Value::Array(v) => {
                for child in v {
                    walk(child, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(config) = read_json(&home.join(".config/oxc/llm/runtime.json")) {
        walk(&config, &mut out);
    }
    out
}
fn quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
#[derive(Default)]
struct Plan {
    units: Vec<String>,
    disable: Vec<String>,
    remove: Vec<PathBuf>,
    cleanup: Vec<PathBuf>,
    kept: Reasons,
    refused: Reasons,
    notes: Vec<String>,
    commands: Vec<String>,
    records: Vec<String>,
    extra: Vec<PathBuf>,
}
fn plan(home: &Path, remove_models: bool, extra: Vec<PathBuf>) -> Plan {
    let (owned, dirs, used) = records(home, &extra);
    let mut plan = Plan {
        records: used,
        extra,
        ..Plan::default()
    };
    let unit_root = home.join(".config/systemd/user");
    for (path, wants) in owned {
        if !exists(&path) {
            continue;
        }
        if !owned_now(&path, &wants) {
            plan.kept.push((
                path.display().to_string(),
                "changed since ox-llm installed it (sha256 differs); kept".into(),
            ));
            continue;
        }
        if let Some(reason) = vet(home, &path) {
            plan.refused
                .push((path.display().to_string(), reason.into()));
            continue;
        }
        plan.remove.push(path);
    }
    for place in PLACES {
        let path = home.join(place);
        if exists(&path) && !plan.remove.contains(&path) {
            let dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            plan.kept.push((
                path.display().to_string(),
                if dir {
                    "directory left in place; only its recorded files are removed"
                } else {
                    "not in an ox-llm install record; kept"
                }
                .into(),
            ));
        }
    }
    for path in models(home) {
        if !exists(&path) {
            continue;
        }
        if !remove_models {
            plan.kept.push((
                path.display().to_string(),
                "model weights (pass --remove-models to delete this file)".into(),
            ));
        } else if path.starts_with(home) {
            if let Some(reason) = vet(home, &path) {
                plan.refused
                    .push((path.display().to_string(), reason.into()));
            } else {
                plan.remove.push(path);
            }
        } else {
            plan.kept.push((
                path.display().to_string(),
                "model file outside home; never removed by this tool".into(),
            ));
            plan.commands
                .push(format!("rm -- {}", quote(&path.display().to_string())));
        }
    }
    plan.disable = plan
        .remove
        .iter()
        .filter(|p| p.parent() == Some(unit_root.as_path()))
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .filter(|s| s.ends_with(".service") || s.ends_with(".timer"))
        .map(str::to_owned)
        .collect();
    plan.disable.sort();
    if !plan.remove.is_empty() {
        let mut units: BTreeSet<_> = plan.disable.iter().cloned().collect();
        units.extend(
            NAMED
                .iter()
                .filter(|name| exists(&unit_root.join(name)))
                .map(|s| s.to_string()),
        );
        plan.units = units.into_iter().collect();
    }
    plan.cleanup = dirs
        .into_iter()
        .filter(|p| p.starts_with(home))
        .chain(
            plan.disable
                .iter()
                .map(|name| unit_root.join(format!("{name}.d"))),
        )
        .collect();
    for folder in &plan.cleanup {
        if exists(folder) {
            if let Some(reason) = vet_dir(home, folder) {
                plan.refused
                    .push((folder.display().to_string(), reason.into()));
            }
        }
    }
    if plan.records.is_empty() {
        plan.notes.push("no ox-llm install record was found (update state, or pass --record); no program file is removed".into());
    }
    plan.notes.push(CLIENT_NOTE.into());
    let shell = home.join(".config/omarchy/shell.json");
    if fs::read_to_string(&shell).is_ok_and(|s| s.contains("snapdragon.local-ai")) {
        plan.notes.push(format!("{} still lists the snapdragon.local-ai widget in the bar layout; it is not edited. Remove that entry yourself, then restart the shell.",shell.display()));
    }
    plan
}
async fn systemctl(operation: &str, units: &[String]) -> (Option<i32>, String, String) {
    let mut command = tokio::process::Command::new("systemctl");
    command
        .args(["--user", operation])
        .args(units)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(60), command.output()).await {
        Ok(Ok(out)) => (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim().into(),
            String::from_utf8_lossy(&out.stderr).trim().into(),
        ),
        Ok(Err(e)) => (None, String::new(), e.to_string()),
        Err(_) => (
            None,
            String::new(),
            "systemctl timed out after 60 seconds".into(),
        ),
    }
}
async fn states(units: &[String]) -> Option<BTreeMap<String, String>> {
    let (code, out, _) = systemctl("is-active", units).await;
    let lines: Vec<_> = out.lines().collect();
    if code.is_none() || lines.len() != units.len() {
        return None;
    }
    Some(
        units
            .iter()
            .cloned()
            .zip(lines.iter().map(|s| s.trim().to_string()))
            .collect(),
    )
}
fn stopped(value: &str) -> bool {
    ["inactive", "failed"].contains(&value)
}
async fn stop_all(units: &[String]) -> Result<(), String> {
    let before = states(units)
        .await
        .ok_or("cannot ask systemctl whether the ox-llm services run; nothing removed")?;
    let running: Vec<_> = units
        .iter()
        .filter(|u| !stopped(&before[*u]))
        .cloned()
        .collect();
    if !running.is_empty() {
        systemctl("stop", &running).await;
    }
    let after = states(units).await;
    let bad: Vec<_> = units
        .iter()
        .filter(|u| {
            after
                .as_ref()
                .and_then(|a| a.get(*u))
                .is_none_or(|s| !stopped(s))
        })
        .cloned()
        .collect();
    if !bad.is_empty() {
        let active: Vec<_> = running
            .iter()
            .filter(|u| before[*u] == "active")
            .cloned()
            .collect();
        if !active.is_empty() {
            systemctl("start", &active).await;
        }
        let shown = bad
            .iter()
            .map(|u| {
                format!(
                    "{u} ({})",
                    after
                        .as_ref()
                        .and_then(|a| a.get(u))
                        .filter(|s| !s.is_empty())
                        .map(String::as_str)
                        .unwrap_or("unknown")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "could not prove these services stopped: {shown}; nothing removed{}",
            if active.is_empty() {
                String::new()
            } else {
                format!("; restarted {}", active.join(", "))
            }
        ));
    }
    Ok(())
}
fn open_directory(path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut fd = open(Path::new("/"), flags, Mode::empty())?;
    for part in path.components() {
        if let Component::Normal(name) = part {
            fd = openat(&fd, name, flags, Mode::empty())?;
        }
    }
    Ok(fd)
}
fn unlink_owned(
    home: &Path,
    path: &Path,
    dir: bool,
    wants: Option<&BTreeSet<String>>,
) -> io::Result<bool> {
    let real_home = paths::realpath(home)?;
    let parent = paths::realpath(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    if !parent.starts_with(&real_home) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a parent directory is a symlink that leaves home",
        ));
    }
    let fd = open_directory(&parent)?;
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let info = fstatat(&fd, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if info.st_uid != Uid::current().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not owned by the current user",
        ));
    }
    if let Some(wants) = wants {
        let file = File::from(openat(
            &fd,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?);
        if !wants.contains(&sha(file)?) {
            return Ok(false);
        }
    }
    let kind = SFlag::from_bits_truncate(info.st_mode) & SFlag::S_IFMT;
    if dir {
        if kind != SFlag::S_IFDIR {
            return Err(io::ErrorKind::NotADirectory.into());
        }
    } else if kind != SFlag::S_IFREG && kind != SFlag::S_IFLNK {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    unlinkat(
        &fd,
        name,
        if dir {
            UnlinkatFlags::RemoveDir
        } else {
            UnlinkatFlags::NoRemoveDir
        },
    )?;
    Ok(true)
}
async fn apply(home: &Path, plan: &Plan) -> Result<(i32, Vec<String>, Reasons), String> {
    if !plan.units.is_empty() {
        stop_all(&plan.units).await?;
    }
    let (mut removed, mut failed) = (Vec::new(), Vec::new());
    if !plan.disable.is_empty() {
        let (code, _, err) = systemctl("disable", &plan.disable).await;
        if code != Some(0) {
            failed.push((
                "systemctl disable".into(),
                if err.is_empty() {
                    format!(
                        "exit {}",
                        code.map(|n| n.to_string()).unwrap_or_else(|| "None".into())
                    )
                } else {
                    err
                },
            ));
        }
    }
    let (owned, _, _) = records(home, &plan.extra);
    for path in &plan.remove {
        if !exists(path) {
            continue;
        }
        let wants = owned.get(&norm(path));
        if wants.is_some_and(|w| !owned_now(path, w)) {
            failed.push((
                path.display().to_string(),
                "changed since the plan; kept".into(),
            ));
            continue;
        }
        if let Some(reason) = vet(home, path) {
            failed.push((path.display().to_string(), reason.into()));
            continue;
        }
        match unlink_owned(home, path, false, wants) {
            Ok(true) => removed.push(path.display().to_string()),
            Ok(false) => failed.push((
                path.display().to_string(),
                "changed since the plan; kept".into(),
            )),
            Err(e) => {
                let reason = e
                    .raw_os_error()
                    .map(|n| {
                        format!(
                            "[Errno {n}] {}: {}",
                            nix::errno::Errno::from_raw(n).desc(),
                            crate::paths::python_json(&json!(path.display().to_string()))
                        )
                    })
                    .unwrap_or_else(|| e.to_string());
                failed.push((path.display().to_string(), reason));
            }
        }
    }
    for folder in &plan.cleanup {
        if !exists(folder) {
            continue;
        }
        if let Some(reason) = vet_dir(home, folder) {
            if reason != "gone" {
                failed.push((
                    folder.display().to_string(),
                    format!("{reason}; directory kept"),
                ));
            }
            continue;
        }
        if unlink_owned(home, folder, true, None).is_ok() {
            removed.push(folder.display().to_string());
        }
    }
    if !plan.units.is_empty() {
        let (code, _, err) = systemctl("daemon-reload", &[]).await;
        if code != Some(0) {
            failed.push((
                "systemctl daemon-reload".into(),
                if err.is_empty() {
                    format!(
                        "exit {}",
                        code.map(|n| n.to_string()).unwrap_or_else(|| "None".into())
                    )
                } else {
                    err
                },
            ));
        }
    }
    Ok((i32::from(!failed.is_empty()), removed, failed))
}
fn pretty(value: &Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let next = "  ".repeat(depth + 1);
    match value {
        Value::Object(v) if !v.is_empty() => format!(
            "{{\n{}\n{indent}}}",
            v.iter()
                .map(|(k, v)| format!(
                    "{next}{}: {}",
                    paths::python_json(&json!(k)),
                    pretty(v, depth + 1)
                ))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        Value::Array(v) if !v.is_empty() => format!(
            "[\n{}\n{indent}]",
            v.iter()
                .map(|v| format!("{next}{}", pretty(v, depth + 1)))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        _ => paths::python_json(value),
    }
}
fn report(
    plan: &Plan,
    as_json: bool,
    applied: bool,
    removed: &[String],
    failed: &Reasons,
    code: i32,
    message: Option<&str>,
) {
    let reasons = |v: &Reasons| {
        v.iter()
            .map(|(p, r)| json!({"path":p,"reason":r}))
            .collect::<Vec<_>>()
    };
    if as_json {
        let mut body = json!({"applied":applied,"exit":code,"units":plan.units,(if applied {"removed"} else {"remove"}):removed,"records":plan.records,"failed":reasons(failed),"kept":reasons(&plan.kept),"refused":reasons(&plan.refused),"notes":plan.notes,"commands":plan.commands});
        if let Some(message) = message {
            body["error"] = json!(message);
        }
        println!("{}", pretty(&body, 0));
        return;
    }
    if let Some(message) = message {
        println!("oxc uninstall: refused: {message}");
    }
    println!(
        "{}",
        if applied {
            "Removed:"
        } else if message.is_some() {
            "Planned (not removed):"
        } else {
            "Dry run (nothing changed; pass --apply to remove):"
        }
    );
    for item in removed {
        println!("  - {item}");
    }
    if removed.is_empty() {
        println!("  (nothing)");
    }
    if !plan.units.is_empty() && !applied {
        println!("Services that must stop first: {}", plan.units.join(", "));
    }
    for (title, rows) in [
        ("Refused:", &plan.refused),
        ("Failed:", failed),
        ("Kept:", &plan.kept),
    ] {
        if !rows.is_empty() {
            println!("{title}");
            for (path, reason) in rows {
                println!("  - {path}: {reason}");
            }
        }
    }
    if !plan.commands.is_empty() {
        println!("Run yourself to finish (this tool never deletes outside home or uses sudo):");
        for command in &plan.commands {
            println!("  {command}");
        }
    }
    if !plan.notes.is_empty() {
        println!("Notes:");
        for note in &plan.notes {
            println!("  - {note}");
        }
    }
}
pub async fn run(do_apply: bool, remove_models: bool, as_json: bool, extra: Vec<String>) -> i32 {
    let home = match check_home() {
        Ok(home) => home,
        Err(message) => {
            report(
                &Plan::default(),
                as_json,
                false,
                &[],
                &Vec::new(),
                2,
                Some(&message),
            );
            return 2;
        }
    };
    let plan = plan(
        &home,
        remove_models,
        extra.into_iter().map(PathBuf::from).collect(),
    );
    let planned: Vec<_> = plan
        .remove
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if !plan.refused.is_empty() {
        report(
            &plan,
            as_json,
            false,
            &planned,
            &Vec::new(),
            2,
            Some("unsafe paths in the plan (see Refused); nothing removed"),
        );
        return 2;
    }
    if !do_apply {
        report(&plan, as_json, false, &planned, &Vec::new(), 0, None);
        return 0;
    }
    match apply(&home, &plan).await {
        Ok((code, removed, failed)) => {
            report(&plan, as_json, true, &removed, &failed, code, None);
            code
        }
        Err(message) => {
            report(
                &plan,
                as_json,
                false,
                &planned,
                &Vec::new(),
                2,
                Some(&message),
            );
            2
        }
    }
}
