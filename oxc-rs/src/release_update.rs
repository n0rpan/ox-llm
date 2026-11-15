//! Offline staged updates and exact rollback; services and readiness are proven explicitly.
use crate::{paths, release_files as files};
use chrono::{Local, SecondsFormat};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

const CHAT: &str = "local-ai-chat.service";
const STUDIO: &str = "prompt-studio-app.service";
const SERVICES: [&str; 2] = [CHAT, STUDIO];
fn stage_roots() -> Vec<PathBuf> {
    if let Some(value) = std::env::var_os("LOCAL_AI_STAGE_ROOTS").filter(|s| !s.is_empty()) {
        std::env::split_paths(&value)
            .filter(|p| !p.as_os_str().is_empty())
            .collect()
    } else {
        vec![paths::xdg("XDG_DATA_HOME", ".local/share").join("ai-stack/staged")]
    }
}
fn stages() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in stage_roots() {
        if let Ok(entries) = fs::read_dir(root) {
            let mut paths: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
                .collect();
            paths.sort();
            out.extend(paths);
        }
    }
    out
}
fn state_path() -> PathBuf {
    std::env::var_os("LOCAL_AI_UPDATE_STATE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/update/state.json")
        })
}
fn state() -> Value {
    let mut value = files::json_file(&state_path())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"current":null,"history":[]}));
    if value.get("current").is_none() {
        value["current"] = Value::Null;
    }
    let history: Vec<_> = value["history"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v.is_object())
        .cloned()
        .collect();
    value["history"] = json!(history
        .into_iter()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>());
    value
}
fn record(action: &str, stage: Option<&Path>, result: &str, extra: Value) {
    let mut value = state();
    let mut entry = json!({"time":Local::now().to_rfc3339_opts(SecondsFormat::Secs,false),"action":action,"stage":stage.map(|p|p.display().to_string()),"result":result});
    if let Some(extra) = extra.as_object() {
        entry.as_object_mut().expect("record").extend(extra.clone());
    }
    let mut history = value["history"].as_array().cloned().unwrap_or_default();
    history.push(entry.clone());
    if history.len() > 20 {
        history.remove(0);
    }
    value["history"] = json!(history);
    if action == "update" && result == "installed" {
        value["current"] =
            json!({"stage":stage.map(|p|p.display().to_string()),"since":entry["time"]});
    } else if ["rolled_back", "auto_rolled_back"].contains(&result) {
        value["current"] = Value::Null;
    }
    let _ = (|| -> io::Result<()> {
        let path = state_path();
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        files::atomic(&path, (pretty(&value, 0) + "\n").as_bytes(), 0o600, "state")
    })();
}
fn pretty(value: &Value, depth: usize) -> String {
    let indent = " ".repeat(depth);
    let next = " ".repeat(depth + 1);
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
fn candidate() -> Result<PathBuf, String> {
    let mut found = Vec::new();
    for stage in stages() {
        if let Ok(m) = files::manifest(&stage) {
            if stage.join("retired.json").exists() || files::truthy(&m["retired"]) {
                continue;
            }
            if files::live_matches(&m, "preimage_sha256") {
                if let Ok(info) = fs::metadata(&stage) {
                    found.push(((info.mtime(), info.mtime_nsec()), stage));
                }
            }
        }
    }
    found.sort();
    found.pop().map(|(_,p)|p).ok_or_else(||"No staged release is ready to install. Stage one first, then run `local-ai update <dir>`.".into())
}
fn current() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(stage) = state()["current"]["stage"].as_str() {
        candidates.push(PathBuf::from(stage));
    }
    candidates.extend(stages());
    candidates.into_iter().find(|stage| {
        stage.join("rollback/rollback.py").is_file()
            && files::manifest(stage).is_ok_and(|m| files::live_matches(&m, "installed_sha256"))
    })
}
fn systemctl(m: &Value) -> String {
    m["rollback"]["systemctl"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("LOCAL_AI_SYSTEMCTL")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "systemctl".into())
}
async fn command(
    tool: &str,
    args: &[String],
    timeout: f64,
    env: &[(&str, &str)],
) -> (i32, String, String) {
    let mut command = tokio::process::Command::new(tool);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    command.envs(env.iter().copied());
    let result = tokio::time::timeout(Duration::from_secs_f64(timeout), command.output()).await;
    match result {
        Ok(Ok(out)) => (
            out.status.code().unwrap_or(127),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Ok(Err(e)) => (
            127,
            String::new(),
            match e.kind() {
                io::ErrorKind::NotFound => "FileNotFoundError",
                io::ErrorKind::PermissionDenied => "PermissionError",
                _ => "OSError",
            }
            .into(),
        ),
        Err(_) => (127, String::new(), "TimeoutExpired".into()),
    }
}
async fn ctl(tool: &str, verb: &str, units: &[String], timeout: f64) -> (i32, String, String) {
    let mut args = vec!["--user".into(), verb.into()];
    args.extend_from_slice(units);
    command(tool, &args, timeout, &[]).await
}
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}
async fn controller_release(tool: &str) -> Option<String> {
    let (code, out, _) = command(
        tool,
        &strings(&["--user", "show", CHAT, "-p", "MainPID"]),
        15.0,
        &[],
    )
    .await;
    if code != 0 {
        return None;
    }
    let pid = out
        .lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| *k == "MainPID")?
        .1
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)?;
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}
async fn record_services(stage: &Path, tool: &str) -> Result<(), String> {
    let path = stage.join("service-state.json");
    if path.exists() {
        return Ok(());
    }
    let (_, out, _) = ctl(tool, "is-active", &strings(&SERVICES), 60.0).await;
    let states: Vec<_> = out.split_whitespace().collect();
    if states.len() != 2 {
        return Err("Could not read which local AI services are running.".into());
    }
    let active: Vec<_> = states
        .iter()
        .map(|s| ["active", "activating", "reloading"].contains(s))
        .collect();
    let release = if active[0] {
        controller_release(tool).await
    } else {
        None
    };
    let value = json!({"time":Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),"states":{CHAT:states[0],STUDIO:states[1]},"active":{CHAT:active[0],STUDIO:active[1]},"chat_release":release});
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(
        file,
        "{}",
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
    )
    .map_err(|e| e.to_string())
}
fn service_record(stage: &Path) -> Value {
    files::json_file(&stage.join("service-state.json"))
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}
async fn stop_services(tool: &str) -> Result<(), String> {
    if ctl(tool, "stop", &strings(&SERVICES), 180.0).await.0 != 0 {
        return Err("the local AI service could not be stopped".into());
    }
    let (_, out, _) = ctl(tool, "is-active", &strings(&SERVICES), 60.0).await;
    let states: Vec<_> = out.split_whitespace().collect();
    if states.len() != 2
        || states
            .iter()
            .any(|v| ["active", "activating", "reloading", "deactivating"].contains(v))
    {
        return Err("the local AI service did not stop".into());
    }
    Ok(())
}
async fn restore_services(stage: &Path, tool: &str) -> bool {
    let record = service_record(stage);
    let start: Vec<_> = SERVICES
        .iter()
        .filter(|s| record["active"][**s] == true)
        .map(|s| s.to_string())
        .collect();
    start.is_empty() || ctl(tool, "start", &start, 180.0).await.0 == 0
}
async fn http(
    url: &str,
    payload: Option<Value>,
    timeout: f64,
    lab: bool,
) -> (Option<u16>, Option<Value>) {
    if !timeout.is_finite() || timeout < 0.0 {
        return (None, None);
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout))
        .build()
    else {
        return (None, None);
    };
    let mut request = if let Some(payload) = payload {
        client.post(url).json(&payload)
    } else {
        client.get(url)
    };
    if lab {
        request = request.header("X-Local-AI-Origin", "lab");
    }
    let Ok(response) = request.send().await else {
        return (None, None);
    };
    let code = response.status().as_u16();
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while bytes.len() < 1 << 20 {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let n = chunk.len().min((1 << 20) - bytes.len());
                bytes.extend_from_slice(&chunk[..n]);
            }
            Some(Err(_)) => return (Some(code), None),
            None => break,
        }
    }
    (Some(code), serde_json::from_slice(&bytes).ok())
}
async fn ready(
    chat: &str,
    seconds: f64,
    tool: &str,
    expect: Option<&str>,
    avoid: Option<&str>,
) -> Result<Value, String> {
    let start = Instant::now();
    let mut last = "the local AI service did not answer".to_owned();
    loop {
        let (code, body) = http(&format!("{chat}/_local/status"), None, 3.0, false).await;
        if let Some(body) = body.filter(|v| code == Some(200) && v.is_object()) {
            let expect = expect.filter(|s| !s.is_empty());
            let avoid = avoid.filter(|s| !s.is_empty());
            let proven = if expect.is_none() && avoid.is_none() {
                Some(true)
            } else {
                controller_release(tool).await.map(|cwd| {
                    if let Some(expect) = expect {
                        cwd == expect
                    } else {
                        Some(cwd.as_str()) != avoid
                    }
                })
            };
            match proven {
                Some(true) => return Ok(body),
                Some(false) => last = "the service runs another version than expected".into(),
                None => last = "which version the service runs could not be confirmed".into(),
            }
        } else if let Some(code) = code {
            last = format!("the local AI service answered {code}");
        }
        if start.elapsed().as_secs_f64() >= seconds {
            return Err(format!("{last} within {} s", seconds as i64));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
async fn chat_check(chat: &str, status: &Value, timeout: f64) -> Result<f64, String> {
    let loaded = status["state"]
        .as_str()
        .is_some_and(|s| ["ready", "loaded"].contains(&s))
        || files::truthy(&status["loaded_model"]);
    let mut body = json!({"temperature":0,"max_tokens":8,"stream":false,"messages":[{"role":"user","content":"Reply with one word: ready"}]});
    if let Some(model) = status["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| status["selected_alias"].as_str().filter(|s| !s.is_empty()))
    {
        body["model"] = json!(model);
    }
    let started = Instant::now();
    let (code, reply) = http(
        &format!("{chat}/v1/chat/completions"),
        Some(body),
        timeout,
        true,
    )
    .await;
    let wall = started.elapsed().as_secs_f64();
    if !loaded {
        http(
            &format!("{chat}/_local/unload"),
            Some(json!({})),
            120.0,
            true,
        )
        .await;
    }
    let answered = reply.as_ref().is_some_and(|v| {
        v["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
            || files::truthy(&v["choices"][0]["finish_reason"])
    });
    if code != Some(200) || !answered {
        return Err(format!(
            "the test chat did not answer{}",
            code.filter(|n| *n != 0)
                .map(|n| format!(" (HTTP {n})"))
                .unwrap_or_default()
        ));
    }
    Ok(format!("{wall:.1}").parse().unwrap_or(wall))
}
async fn refresh_identity(m: &Value, chat: &str) -> String {
    let path = m["paths"]["oxa"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            m["home"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(paths::home)
                .join(".local/bin/oxa")
        });
    if !path.is_file() {
        return "absent".into();
    }
    for offline in [false, true] {
        let mut args = strings(&["identity", "--refresh"]);
        if offline {
            args.push("--offline".into());
        }
        let mut env = vec![("OXA_ENGINE_URL", chat)];
        if let Some(home) = m["home"].as_str() {
            env.push(("HOME", home));
        }
        if command(&path.to_string_lossy(), &args, 30.0, &env).await.0 == 0 {
            return if offline {
                "refreshed-offline"
            } else {
                "refreshed"
            }
            .into();
        }
    }
    "failed".into()
}
fn bound_services(binding: &Value) -> Vec<String> {
    binding["services"]
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec![binding["service"].as_str().unwrap_or(CHAT).into()])
}
async fn rollback_start(stage: &Path, binding: &Value) -> Result<Vec<String>, i32> {
    let services = bound_services(binding);
    let Some(state) = binding["service_state"].as_str().filter(|s| !s.is_empty()) else {
        return Ok(services);
    };
    let path = stage.join(state);
    if !path.exists() {
        let tool = binding["systemctl"].as_str().unwrap_or("systemctl");
        let (_, out, _) = ctl(tool, "is-active", &services, 600.0).await;
        let states: Vec<_> = out.split_whitespace().collect();
        return Ok(if states.len() != services.len() {
            services
        } else {
            services
                .iter()
                .zip(states)
                .filter(|(_, s)| ["active", "activating", "reloading"].contains(s))
                .map(|(s, _)| s.clone())
                .collect()
        });
    }
    let value = files::json_file(&path).ok_or(5)?;
    let active = value["active"]
        .as_object()
        .filter(|v| {
            v.len() == services.len()
                && services
                    .iter()
                    .all(|s| v.get(s).is_some_and(Value::is_boolean))
        })
        .ok_or(5)?;
    Ok(services.into_iter().filter(|s| active[s] == true).collect())
}
async fn rollback_guard(stage: &Path, m: &Value, check: bool) -> Result<(), i32> {
    let binding = files::binding(stage).map_err(|_| 5)?;
    let todo = files::rollback_checks(stage, m, &binding)?;
    if check {
        rollback_start(stage, &binding).await?;
        return Ok(());
    }
    if todo.is_empty() {
        return Ok(());
    }
    let start = rollback_start(stage, &binding).await?;
    let services = bound_services(&binding);
    let tool = binding["systemctl"].as_str().unwrap_or("systemctl");
    if ctl(tool, "stop", &services, 600.0).await.0 != 0 {
        return Err(1);
    }
    let same = binding["files"].as_object().is_some_and(|v| {
        v.iter().all(|(key, item)| {
            files::observed(Path::new(item["path"].as_str().unwrap_or(""))).as_deref()
                == item[if todo.contains(key) {
                    "installed_sha256"
                } else {
                    "preimage_sha256"
                }]
                .as_str()
        })
    });
    if !same {
        if !start.is_empty() {
            ctl(tool, "start", &start, 600.0).await;
        }
        return Err(3);
    }
    files::restore(stage, &binding, &todo).map_err(|_| 1)?;
    if ctl(tool, "daemon-reload", &[], 600.0).await.0 != 0 {
        return Err(1);
    }
    if !start.is_empty() && ctl(tool, "start", &start, 600.0).await.0 != 0 {
        return Err(1);
    }
    let basis = if let Some(name) = binding["service_state"].as_str().filter(|s| !s.is_empty()) {
        if stage.join(name).exists() {
            format!("active before install ({name})")
        } else {
            "active now (no install-time record)".into()
        }
    } else {
        "all bound services".into()
    };
    let hashes = todo
        .iter()
        .map(|k| (k.clone(), binding["files"][k]["preimage_sha256"].clone()))
        .collect::<serde_json::Map<_, _>>();
    files::receipt(&stage.join("rollback/receipts"),"rollback",json!({"action":"rollback","stage":stage.file_name().unwrap_or_default().to_string_lossy(),"restored":todo,"started":start,"start_basis":basis,"sha256":hashes})).map_err(|_|1)?;
    Ok(())
}
fn error(code: i32) -> String {
    files::rollback_error(code)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("exit {code}"))
}
async fn precheck(stage: &Path, m: &Value) -> Result<(), String> {
    println!("Checking the staged release {} ...", files::name(stage, m));
    files::seal(stage)?;
    if files::live_matches(m, "installed_sha256") {
        return Err("This release is already installed.".into());
    }
    for name in ["tools/install_staged_release.py", "rollback/rollback.py"] {
        if !stage.join(name).is_file() {
            return Err(format!("The staged release has no {name}; stage it again."));
        }
    }
    rollback_guard(stage, m, true)
        .await
        .map_err(|code| format!("Its rollback would not work ({}).", error(code)))?;
    // Generated runtime materialization still needs its native bootstrap adapter.
    // Refuse it until those extra generator checks are ported.
    if fs::read_dir(stage.join("tools")).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("stage_") && name.ends_with("_final.py"))
        })
    }) {
        return Err(
            "The staged release must be materialized and checked before it can be installed."
                .into(),
        );
    }
    files::precheck(stage, m)
}
async fn ready_after_rollback(
    stage: &Path,
    m: &Value,
    chat: &str,
    seconds: f64,
) -> Result<(), String> {
    let recorded = service_record(stage);
    if recorded["active"].is_object() && recorded["active"][CHAT] != true {
        return Ok(());
    }
    ready(
        chat,
        seconds,
        &systemctl(m),
        recorded["chat_release"].as_str(),
        m["release_dir"].as_str(),
    )
    .await?;
    Ok(())
}
async fn auto_rollback(stage: &Path, m: &Value, chat: &str, why: &str, seconds: f64) -> i32 {
    println!(
        "The new version did not pass its check ({why}). Putting the previous version back ..."
    );
    if let Err(code) = rollback_guard(stage, m, false).await {
        record(
            "update",
            Some(stage),
            "rollback_failed",
            json!({"reason":why.chars().take(300).collect::<String>()}),
        );
        println!("The automatic rollback did not finish ({}). Run `local-ai rollback`; if that also stops, run `{}/rollback/rollback.py --check` and share its output.",error(code),stage.display());
        return 3;
    }
    if let Err(error) = ready_after_rollback(stage, m, chat, seconds).await {
        record(
            "update",
            Some(stage),
            "auto_rolled_back",
            json!({"reason":why.chars().take(300).collect::<String>(),"ready":false}),
        );
        println!("The previous version is back, but the service is not answering yet ({error}). Give it a minute, then run `local-ai status`.");
        return 3;
    }
    record(
        "update",
        Some(stage),
        "auto_rolled_back",
        json!({"reason":why.chars().take(300).collect::<String>()}),
    );
    println!("Your previous version is back and working. Nothing else changed.");
    1
}
async fn rollback(chat: &str, check: bool, seconds: f64) -> i32 {
    let Some(stage) = current() else {
        println!(
            "Nothing to roll back: no installed release with a saved previous version was found."
        );
        return 2;
    };
    let m = match files::manifest(&stage) {
        Ok(v) => v,
        Err(e) => {
            println!("Nothing was changed: {e}.");
            return 2;
        }
    };
    if let Err(code) = rollback_guard(&stage, &m, true).await {
        println!("Nothing was changed: {}.", error(code));
        return 2;
    }
    let name = files::name(&stage, &m);
    if check {
        println!("Rollback is ready: it would put back the version before {name}.");
        return 0;
    }
    println!("Putting back the version before {name} ...");
    if let Err(code) = rollback_guard(&stage, &m, false).await {
        record(
            "rollback",
            Some(&stage),
            "failed",
            json!({"reason":error(code).chars().take(300).collect::<String>()}),
        );
        println!("The rollback stopped ({}). Run `local-ai rollback` again; if it stops again, run `{}/rollback/rollback.py --check` and share its output.",error(code),stage.display());
        return 3;
    }
    if let Err(error) = ready_after_rollback(&stage, &m, chat, seconds).await {
        record(
            "rollback",
            Some(&stage),
            "rolled_back",
            json!({"ready":false}),
        );
        println!("The previous version is back, but the service is not answering yet ({error}). Give it a minute, then run `local-ai status`.");
        return 3;
    }
    record("rollback", Some(&stage), "rolled_back", json!({}));
    println!("The previous version is back and running. To install the newer one again: local-ai update {}",stage.display());
    0
}
async fn update(
    stage: Option<PathBuf>,
    chat: &str,
    chat_check_enabled: bool,
    seconds: f64,
    chat_seconds: f64,
) -> i32 {
    let stage = match stage.map(Ok).unwrap_or_else(candidate) {
        Ok(stage) => stage,
        Err(error) => {
            println!("Nothing was changed: {error}");
            record(
                "update",
                None,
                "refused",
                json!({"reason":error.chars().take(300).collect::<String>()}),
            );
            return 2;
        }
    };
    let m = match files::manifest(&stage) {
        Ok(m) => m,
        Err(error) => {
            println!("Nothing was changed: {error}");
            record(
                "update",
                Some(&stage),
                "refused",
                json!({"reason":error.chars().take(300).collect::<String>()}),
            );
            return 2;
        }
    };
    if let Err(error) = precheck(&stage, &m).await {
        println!("Nothing was changed: {error}");
        record(
            "update",
            Some(&stage),
            "refused",
            json!({"reason":error.chars().take(300).collect::<String>()}),
        );
        return 2;
    }
    let tool = systemctl(&m);
    let name = files::name(&stage, &m);
    if let Err(error) = record_services(&stage, &tool).await {
        println!("Nothing was changed: {error}");
        record(
            "update",
            Some(&stage),
            "refused",
            json!({"reason":error.chars().take(300).collect::<String>()}),
        );
        return 2;
    }
    println!("Stopping the local AI service ...");
    let stopped = stop_services(&tool).await;
    let installed = if stopped.is_ok() {
        println!("Installing {name} ...");
        files::install(&stage, &m)
    } else {
        stopped
    };
    if let Err(error) = installed {
        if files::live_matches(&m, "preimage_sha256") {
            let back = restore_services(&stage, &tool).await;
            let _ = fs::remove_file(stage.join("service-state.json"));
            record(
                "update",
                Some(&stage),
                "refused",
                json!({"reason":error.chars().take(300).collect::<String>(),"services_restored":back}),
            );
            if !back {
                println!("No file was changed ({error}), but the local AI service did not start again. Run `systemctl --user start local-ai-chat.service`, then `local-ai status`.");
                return 3;
            }
            println!("Nothing was changed: {error}");
            return 2;
        }
        return auto_rollback(&stage, &m, chat, "the installer stopped part-way", seconds).await;
    }
    println!("Starting the local AI service ...");
    let mut start = vec![CHAT.to_owned()];
    if service_record(&stage)["active"][STUDIO] == true {
        start.push(STUDIO.into());
    }
    for (verb, args) in [("daemon-reload", Vec::new()), ("start", start)] {
        if ctl(&tool, verb, &args, 120.0).await.0 != 0 {
            return auto_rollback(&stage, &m, chat, "the service did not start", seconds).await;
        }
    }
    println!("Waiting for it to be ready ...");
    let status = match ready(chat, seconds, &tool, m["release_dir"].as_str(), None).await {
        Ok(status) => status,
        Err(error) => return auto_rollback(&stage, &m, chat, &error, seconds).await,
    };
    let wall = if chat_check_enabled {
        println!("Checking that chat answers (this may load the model) ...");
        match chat_check(chat, &status, chat_seconds).await {
            Ok(wall) => Some(wall),
            Err(error) => return auto_rollback(&stage, &m, chat, &error, seconds).await,
        }
    } else {
        None
    };
    let identity = refresh_identity(&m, chat).await;
    record(
        "update",
        Some(&stage),
        "installed",
        json!({"release":name,"chat_seconds":wall,"agent_identity":identity}),
    );
    println!(
        "Updated to {name}.{} If anything feels wrong: local-ai rollback",
        wall.map(|n| format!(" Chat answered in {n:.1} s."))
            .unwrap_or_default()
    );
    0
}
fn seconds(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
pub async fn run(action: &str, words: &[String], check: bool, no_chat: bool) -> i32 {
    let chat = std::env::var("LOCAL_AI_CHAT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8092".into())
        .trim_end_matches('/')
        .to_string();
    let ready_seconds = seconds("LOCAL_AI_UPDATE_READY_SECONDS", 120.0);
    if action == "rollback" {
        return rollback(&chat, check, ready_seconds).await;
    }
    if words.len() > 1 {
        eprintln!("Usage: local-ai update [STAGED-RELEASE-DIR] [--no-chat]");
        return 2;
    }
    let stage = words
        .first()
        .map(|s| paths::realpath(Path::new(s)).unwrap_or_else(|_| PathBuf::from(s)));
    if check {
        let stage = match stage.map(Ok).unwrap_or_else(candidate) {
            Ok(stage) => stage,
            Err(error) => {
                println!("Not ready to install: {error}");
                return 2;
            }
        };
        let result = match files::manifest(&stage) {
            Ok(m) => precheck(&stage, &m).await,
            Err(e) => Err(e),
        };
        if let Err(error) = result {
            println!("Not ready to install: {error}");
            return 2;
        }
        println!(
            "Ready to install. Run `local-ai update {}`.",
            stage.display()
        );
        return 0;
    }
    update(
        stage,
        &chat,
        !no_chat,
        ready_seconds,
        seconds("LOCAL_AI_UPDATE_CHAT_SECONDS", 900.0),
    )
    .await
}
