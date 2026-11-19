//! `oxa local add <model.gguf>`: wire one local GGUF into the user's native local engine settings
//! from the engine's own admission report.
//!
//! The sibling `oxe` (found beside this executable, never through PATH) prints the allocation
//! bounds for the model and launch profile (`oxe admission --json`); this command writes
//! `oxa.local` (binary, model, arguments, limits including `allocation_budget`) into the USER
//! settings file only, after a backup of the existing file. Every value comes from the report or
//! from an explicit option. A value that is missing is an explained error and nothing is written:
//! oxa never guesses a limit. `oxa.engine.inProcessLocal` is switched on only with `--enable`.
use oxa_config::{Dirs, Settings};
use oxa_proto::{codes, OxaError};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "oxa local add <model.gguf> [--name <alias>] [--ctx <positions>] [--max-output <tokens>] [--margin-bytes <n>] [--enable] [--replace] [--engine-dir <dir>] [-- <oxe-server launch arguments>]";
const SCHEMA: &str = "oxe-admission/1";
/// A reasonable bound on what an engine may print; the report is a few kilobytes.
const REPORT_LIMIT: usize = 1 << 20;

fn usage(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::USAGE, message).with_hint(USAGE)
}

fn invalid(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, message)
}

#[derive(Debug, Default, PartialEq)]
struct AddArgs {
    gguf: PathBuf,
    name: Option<String>,
    ctx: Option<u64>,
    max_output: Option<u64>,
    margin: Option<u64>,
    enable: bool,
    replace: bool,
    engine_dir: Option<PathBuf>,
    launch: Vec<String>,
}

fn number(flag: &str, value: &str) -> Result<u64, OxaError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| usage(format!("{flag} expects a positive whole number, got `{value}`")))
}

fn take<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, OxaError> {
    it.next()
        .cloned()
        .ok_or_else(|| usage(format!("{flag} needs a value")))
}

fn parse_add(words: &[String]) -> Result<AddArgs, OxaError> {
    let mut args = AddArgs::default();
    let mut gguf = None;
    let mut it = words.iter();
    while let Some(word) = it.next() {
        match word.as_str() {
            "--" => {
                args.launch = it.by_ref().cloned().collect();
            }
            "--name" => args.name = Some(take(&mut it, "--name")?),
            "--ctx" => args.ctx = Some(number("--ctx", &take(&mut it, "--ctx")?)?),
            "--max-output" => {
                args.max_output = Some(number("--max-output", &take(&mut it, "--max-output")?)?)
            }
            "--margin-bytes" => {
                args.margin = Some(number("--margin-bytes", &take(&mut it, "--margin-bytes")?)?)
            }
            "--engine-dir" => args.engine_dir = Some(PathBuf::from(take(&mut it, "--engine-dir")?)),
            "--enable" => args.enable = true,
            "--replace" => args.replace = true,
            flag if flag.starts_with('-') => return Err(usage(format!("unknown option: {flag}"))),
            path => {
                if gguf.replace(PathBuf::from(path)).is_some() {
                    return Err(usage("only one model file can be added at a time"));
                }
            }
        }
    }
    args.gguf = gguf.ok_or_else(|| usage("the model file is required"))?;
    if args.ctx.is_some()
        && args
            .launch
            .iter()
            .any(|a| matches!(a.as_str(), "--ctx" | "--ctx-size" | "-c"))
    {
        return Err(usage(
            "the context is given twice: --ctx and a launch argument after `--`",
        ));
    }
    Ok(args)
}

/// `oxa local ..`. Returns the exit code.
pub fn run_local(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    match words.split_first() {
        Some((verb, rest)) if verb == "add" => add(&parse_add(rest)?, dirs, cwd, out),
        _ => Err(usage("usage: oxa local add <model.gguf> ...")),
    }
}

fn alias_for(args: &AddArgs, gguf: &Path) -> Result<String, OxaError> {
    let name = match &args.name {
        Some(n) => n.clone(),
        None => gguf
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default(),
    };
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(name)
    } else {
        Err(usage(format!(
            "`{name}` is not a usable model name (letters, digits, `.`, `_`, `-`; at most 64): pass --name"
        )))
    }
}

/// The directory holding `oxe` and `oxe-server`: `--engine-dir`, else the directory of this
/// executable. There is no PATH or current-directory search.
fn engine_dir(given: Option<&Path>) -> Result<PathBuf, OxaError> {
    let dir = match given {
        Some(d) => d.to_path_buf(),
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .ok_or_else(|| invalid("The directory of this oxa executable is unknown; pass --engine-dir."))?,
    };
    std::fs::canonicalize(&dir)
        .ok()
        .filter(|d| d.is_dir())
        .ok_or_else(|| invalid(format!("The engine directory {} does not exist.", dir.display())))
}

fn sibling(dir: &Path, name: &str) -> Result<PathBuf, OxaError> {
    let path = dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
        Ok(path)
    } else {
        Err(invalid(format!(
            "`{name}` is not an executable file in {}. oxa looks only beside its own executable (no PATH search); install the release that ships oxe and oxe-server together, or pass --engine-dir.",
            dir.display()
        )))
    }
}

/// Run `oxe admission` and parse its JSON. The engine gets an empty environment.
fn admission_report(oxe: &Path, args: &AddArgs, gguf: &Path) -> Result<Value, OxaError> {
    let mut cmd = Command::new(oxe);
    cmd.arg("admission").arg("--model").arg(gguf);
    if let Some(ctx) = args.ctx {
        cmd.arg("--ctx").arg(ctx.to_string());
    }
    if let Some(margin) = args.margin {
        cmd.arg("--margin-bytes").arg(margin.to_string());
    }
    cmd.args(&args.launch)
        .arg("--json")
        .env_clear()
        .stdin(Stdio::null());
    let output = cmd
        .output()
        .map_err(|e| OxaError::io("cannot run the engine's admission report", &e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason: String = stderr.trim().chars().take(1500).collect();
        return Err(invalid(format!(
            "The engine refused to report admission bounds for this model: {reason}"
        ))
        .with_hint("Nothing was written."));
    }
    if output.stdout.len() > REPORT_LIMIT {
        return Err(invalid(
            "The engine's admission report is unexpectedly large; nothing was written.",
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| invalid("The engine's admission report is not valid JSON; nothing was written."))
}

fn missing(path: &str) -> OxaError {
    invalid(format!(
        "The engine's admission report has no usable {path}; nothing was written. oxa never guesses a limit."
    ))
}

fn positive(report: &Value, pointer: &str) -> Result<u64, OxaError> {
    report
        .pointer(pointer)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| missing(pointer))
}

/// The limits and arguments of a report, each checked present. `scratch_bytes` and
/// `mtp_state_bytes` may legitimately be zero; the others must be positive.
fn from_report(report: &Value, max_output: Option<u64>) -> Result<(Value, Vec<String>), OxaError> {
    if report.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Err(invalid(format!(
            "The engine's admission report is not `{SCHEMA}` (the oxe next to oxa is a different version); nothing was written."
        )));
    }
    let context = positive(report, "/limits/context_window")?;
    let context_u32 = u32::try_from(context).map_err(|_| missing("/limits/context_window"))?;
    let reported_max = positive(report, "/limits/max_output")?;
    let max_output = max_output.unwrap_or(reported_max);
    if max_output > context {
        return Err(usage(format!(
            "--max-output {max_output} exceeds the context window {context}"
        )));
    }
    let budget = |field: &str, zero_ok: bool| -> Result<u64, OxaError> {
        let pointer = format!("/limits/allocation_budget/{field}");
        let value = report
            .pointer(&pointer)
            .and_then(Value::as_u64)
            .ok_or_else(|| missing(&pointer))?;
        if value == 0 && !zero_ok {
            return Err(missing(&pointer));
        }
        Ok(value)
    };
    let allocation = json!({
        "kv_bytes_per_position": budget("kv_bytes_per_position", false)?,
        "scratch_bytes": budget("scratch_bytes", true)?,
        "mtp_state_bytes": budget("mtp_state_bytes", true)?,
        "margin_bytes": budget("margin_bytes", false)?,
    });
    let arguments: Vec<String> = report
        .get("arguments")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_owned)).collect())
        .ok_or_else(|| missing("/arguments"))?;
    // oxa binds the budget to the launch context: the reported arguments must carry exactly it
    if !arguments
        .windows(2)
        .any(|w| w[0] == "--ctx-size" && w[1] == context_u32.to_string())
    {
        return Err(missing("/arguments (a `--ctx-size` equal to the context window)"));
    }
    Ok((
        json!({"context_window": context_u32, "max_output": max_output, "allocation_budget": allocation}),
        arguments,
    ))
}

/// Copy the existing user settings file next to itself (0600, never over an older backup).
fn backup(path: &Path) -> Result<Option<PathBuf>, OxaError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(OxaError::io("cannot read the user settings file", &e)),
    };
    if meta.file_type().is_symlink() {
        return Err(invalid(format!(
            "{} is a symbolic link; it is not changed.",
            path.display()
        ))
        .with_hint("Edit the file the link points to."));
    }
    let bytes = std::fs::read(path).map_err(|e| OxaError::io("cannot read the user settings file", &e))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    for attempt in 0..100u32 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let target = path.with_file_name(format!("{name}.bak-{stamp}{suffix}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)
        {
            Ok(mut file) => {
                file.write_all(&bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(|e| OxaError::io("cannot write the settings backup", &e))?;
                return Ok(Some(target));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(OxaError::io("cannot write the settings backup", &e)),
        }
    }
    Err(invalid("Too many settings backups with the same timestamp."))
}

fn object<'a>(value: &'a mut Value, what: &str) -> Result<&'a mut Map<String, Value>, OxaError> {
    value.as_object_mut().ok_or_else(|| {
        invalid(format!(
            "{what} in the user settings is not an object; fix or remove it first."
        ))
    })
}

fn add(args: &AddArgs, dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    let gguf = std::fs::canonicalize(&args.gguf)
        .ok()
        .filter(|p| p.is_file())
        .ok_or_else(|| invalid(format!("{} is not a model file.", args.gguf.display())))?;
    let name = alias_for(args, &gguf)?;
    let dir = engine_dir(args.engine_dir.as_deref())?;
    let (oxe, server) = (sibling(&dir, "oxe")?, sibling(&dir, "oxe-server")?);
    let report = admission_report(&oxe, args, &gguf)?;
    let (limits, arguments) = from_report(&report, args.max_output)?;

    // USER settings only: the project file is never read or written here.
    let mut settings = Settings::load(dirs, cwd, false);
    let mut local = settings
        .user_value("oxa.local")
        .cloned()
        .unwrap_or_else(|| json!({}));
    {
        let root = object(&mut local, "oxa.local")?;
        let server_path = server.to_string_lossy().into_owned();
        match root.get("binary").and_then(Value::as_str) {
            Some(existing) if existing != server_path => {
                return Err(invalid(format!(
                    "oxa.local.binary is already {existing}, not {server_path}; remove it from your user settings or pass --engine-dir for that installation."
                )))
            }
            _ => {
                root.insert("binary".into(), json!(server_path));
            }
        }
        for key in ["models", "limits"] {
            let map = root.entry(key).or_insert_with(|| json!({}));
            let map = object(map, &format!("oxa.local.{key}"))?;
            if map.contains_key(&name) && !args.replace {
                return Err(invalid(format!(
                    "oxa.local.{key}.{name} already exists; pass --replace to overwrite it."
                )));
            }
        }
        root["models"][&name] = json!({"model": gguf.to_string_lossy(), "arguments": arguments});
        root["limits"][&name] = limits.clone();
    }
    // The real consumer must accept what is written (no process, file or socket is opened).
    oxc_local::configured_local_engine(
        Some(&local),
        dirs.agent_dir.join("local-engine"),
        oxa_engine::StreamOpts::default(),
    )
    .map(drop)
    .map_err(|e| {
        invalid(format!(
            "The generated oxa.local was rejected: {}; nothing was written.",
            e.message
        ))
    })?;

    let settings_path = dirs.settings_file();
    let saved = backup(&settings_path)?;
    settings.set(dirs, cwd, "oxa.local", local, false)?;
    if args.enable {
        settings.set(dirs, cwd, "oxa.engine.inProcessLocal", Value::Bool(true), false)?;
    }

    let write_err = |e: std::io::Error| OxaError::io("cannot write the result", &e);
    let budget = &limits["allocation_budget"];
    writeln!(out, "Added local model `{name}` to {}", settings_path.display()).map_err(write_err)?;
    match &saved {
        Some(path) => writeln!(out, "Backup of the previous file: {}", path.display()),
        None => writeln!(out, "There was no user settings file to back up."),
    }
    .map_err(write_err)?;
    writeln!(
        out,
        "  model {}\n  context_window {}  max_output {}\n  allocation_budget {}",
        gguf.display(),
        limits["context_window"],
        limits["max_output"],
        serde_json::to_string(budget).unwrap_or_default()
    )
    .map_err(write_err)?;
    writeln!(out, "  arguments {}", arguments.join(" ")).map_err(write_err)?;
    if args.enable {
        writeln!(out, "oxa.engine.inProcessLocal is now true.").map_err(write_err)?;
    } else {
        writeln!(
            out,
            "The local engine is not enabled yet: run again with --enable, or set oxa.engine.inProcessLocal to true in your user settings."
        )
        .map_err(write_err)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_options_and_launch_arguments_after_the_separator() {
        let a = parse_add(&words(&[
            "m.gguf",
            "--name",
            "x",
            "--ctx",
            "4096",
            "--enable",
            "--",
            "--oxe-cpus",
            "0-3",
        ]))
        .unwrap();
        assert_eq!(a.gguf, PathBuf::from("m.gguf"));
        assert_eq!(
            (a.name.as_deref(), a.ctx, a.enable),
            (Some("x"), Some(4096), true)
        );
        assert_eq!(a.launch, words(&["--oxe-cpus", "0-3"]));
        assert!(parse_add(&words(&["--ctx", "0", "m.gguf"])).is_err());
        assert!(parse_add(&words(&["a.gguf", "b.gguf"])).is_err());
        assert!(parse_add(&words(&["--ctx", "4096", "m.gguf", "--", "-c", "8"])).is_err());
        assert!(parse_add(&words(&["--frob", "m.gguf"])).is_err());
    }
}
