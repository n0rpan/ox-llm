use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::Uid::current())
                .ok()
                .flatten()
                .map(|user| user.dir)
        })
        .expect("Could not determine home directory.")
}

pub fn xdg(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

pub fn config() -> PathBuf {
    std::env::var_os("LOCAL_LLM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("local-ai/llm/runtime.json"))
}

/// Resolve existing symlinks while retaining missing trailing components, as
/// Python's non-strict realpath does for a not-yet-created conversation store.
pub fn realpath(path: &std::path::Path) -> std::io::Result<PathBuf> {
    fn resolve(path: &std::path::Path, links: &mut usize) -> std::io::Result<PathBuf> {
        use std::path::Component;
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut resolved = PathBuf::from("/");
        for part in absolute.components() {
            match part {
                Component::RootDir => resolved = PathBuf::from("/"),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => {
                    let candidate = resolved.join(name);
                    match std::fs::symlink_metadata(&candidate) {
                        Ok(info) if info.file_type().is_symlink() => {
                            *links += 1;
                            if *links > 40 {
                                return Err(std::io::Error::from_raw_os_error(nix::libc::ELOOP));
                            }
                            let target = std::fs::read_link(&candidate)?;
                            resolved = resolve(
                                &if target.is_absolute() {
                                    target
                                } else {
                                    resolved.join(target)
                                },
                                links,
                            )?;
                        }
                        Ok(_) => resolved = candidate,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) =>
                        {
                            resolved = candidate
                        }
                        Err(error) => return Err(error),
                    }
                }
                Component::Prefix(_) => unreachable!("Unix path"),
            }
        }
        Ok(resolved)
    }
    resolve(path, &mut 0)
}

pub fn read_config() -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(config()).ok()?).ok()?;
    value.is_object().then_some(value)
}

pub fn write_config(value: &serde_json::Value) -> std::io::Result<()> {
    use std::{
        fs::{self, File, OpenOptions},
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let requested = config();
    // Match the existing writer: follow the config symlink, keep the target's
    // permissions, and replace it using a private same-directory temporary.
    let path = match fs::canonicalize(&requested) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => requested,
        Err(error) => return Err(error),
    };
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let mode = fs::metadata(&path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let mut suffix = [0_u8; 4];
    getrandom::fill(&mut suffix).map_err(|error| std::io::Error::other(error.to_string()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        suffix
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(ascii_json(&serde_json::to_string_pretty(value)?).as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        if mode != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&temporary, &path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn python_json(value: &serde_json::Value) -> String {
    format_json(value, true, true)
}
pub fn unicode_json(value: &serde_json::Value) -> String {
    format_json(value, false, true)
}
pub fn compact_ascii_json(value: &serde_json::Value) -> String {
    format_json(value, true, false)
}
fn ascii_json(raw: &str) -> String {
    let mut result = String::new();
    for ch in raw.chars() {
        if (ch as u32) < 0x7f {
            result.push(ch);
        } else {
            let mut units = [0_u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                result.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    result
}
fn format_json(value: &serde_json::Value, ascii: bool, spaced: bool) -> String {
    use serde_json::Value;
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| format_json(value, ascii, spaced))
                .collect::<Vec<_>>()
                .join(if spaced { ", " } else { "," })
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}{}",
                    format_json(&Value::String(key.clone()), ascii, spaced),
                    if spaced { " " } else { "" },
                    format_json(value, ascii, spaced)
                ))
                .collect::<Vec<_>>()
                .join(if spaced { ", " } else { "," })
        ),
        Value::String(_) => {
            let raw = value.to_string();
            if ascii {
                ascii_json(&raw)
            } else {
                raw
            }
        }
        _ => value.to_string(),
    }
}

/// Python's default `:g` presentation (six significant digits).
pub fn general_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("finite numeric exponent");
    let exponent: i32 = exponent.parse().expect("numeric exponent");
    if (-4..6).contains(&exponent) {
        let decimals = (5 - exponent).max(0) as usize;
        let formatted = format!("{value:.decimals$}");
        if decimals == 0 {
            formatted
        } else {
            formatted
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        }
    } else {
        format!(
            "{}e{exponent:+03}",
            mantissa.trim_end_matches('0').trim_end_matches('.')
        )
    }
}
