use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
};

const HELP: &str = include_str!("help.txt");
fn main_help() -> &'static str {
    if std::env::args_os().next().is_some_and(|name| {
        std::path::Path::new(&name)
            .file_name()
            .is_some_and(|name| name == "local-ai")
    }) {
        include_str!("legacy-help.txt")
    } else {
        HELP
    }
}
const UNINSTALL_HELP: &str = include_str!("uninstall-help.txt");
const ACTIONS: &[&str] = &[
    "panel",
    "status",
    "ledger",
    "storage",
    "chat",
    "code",
    "plan",
    "images",
    "load",
    "unload",
    "moe",
    "ornith",
    "flash",
    "clear-cache",
    "storage-delete",
    "harness-prepare",
    "harness-forget",
    "flash-cache",
    "cache",
    "mode",
    "share",
    "digest",
    "update",
    "rollback",
];
const FLAGS: &[&str] = &[
    "--version",
    "--rotate",
    "--json",
    "--all",
    "--write",
    "--status",
    "--soak",
    "--check",
    "--no-chat",
];
const VALUES: &[&str] = &[
    "--id",
    "--pref",
    "--harness",
    "--mode",
    "--limit",
    "--session",
    "--day",
];
const CACHE_USAGE: &str = "Usage:\n  local-ai cache [status] [--json]          conversation cache: used, budget, conversations\n  local-ai cache budget GB|off              decimal GB, 4 to 4096 (default 64); off keeps no saved state\n  local-ai cache clear [--all | --session ID]  --all (default) also clears stored prompts";

pub struct Exit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Default)]
pub struct Args {
    pub action: String,
    pub rest: Vec<String>,
    flags: HashSet<String>,
    values: HashMap<String, String>,
}

impl Args {
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

fn usage(help: &str) -> &str {
    help.split("\n\n").next().unwrap_or(help)
}
fn error(message: String, uninstall: bool) -> Exit {
    let help = if uninstall {
        UNINSTALL_HELP
    } else {
        main_help()
    };
    let program = if uninstall {
        "local-ai uninstall"
    } else if main_help() == HELP {
        "oxc"
    } else {
        "local-ai"
    };
    Exit {
        code: 2,
        stdout: String::new(),
        stderr: format!("{}\n{program}: error: {message}\n", usage(help)),
    }
}
fn help(uninstall: bool) -> Exit {
    Exit {
        code: 0,
        stdout: if uninstall {
            UNINSTALL_HELP
        } else {
            main_help()
        }
        .to_owned(),
        stderr: String::new(),
    }
}
fn choices(values: &[&str]) -> String {
    values.join(", ")
}

pub fn parse(words: &[String]) -> Result<Args, Exit> {
    if words.first().map(String::as_str) == Some("dense") {
        return Err(Exit {code:2, stdout:String::new(), stderr:"The dense Qwen3.8 27B model was retired in v0.1.2. Use `local-ai ornith` or `local-ai moe`.\n".into()});
    }
    let uninstall = words.first().map(String::as_str) == Some("uninstall");
    let input = if uninstall { &words[1..] } else { words };
    let mut result = Args {
        action: if uninstall { "uninstall" } else { "panel" }.into(),
        ..Args::default()
    };
    let mut action_seen = uninstall;
    let mut positional_only = false;
    let mut unknown = Vec::new();
    let mut index = 0;
    while index < input.len() {
        let word = &input[index];
        index += 1;
        if !positional_only && word == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && (word == "-h" || word == "--help") {
            return Err(help(uninstall));
        }
        if !positional_only && word.starts_with('-') && word != "-" {
            let (requested, explicit) = word
                .split_once('=')
                .map_or((word.as_str(), None), |(k, v)| (k, Some(v)));
            let uninstall_flags = ["--apply", "--remove-models", "--json"];
            let uninstall_values = ["--record"];
            let flags = if uninstall {
                uninstall_flags.as_slice()
            } else {
                FLAGS
            };
            let values = if uninstall {
                uninstall_values.as_slice()
            } else {
                VALUES
            };
            let matches: Vec<&str> = flags
                .iter()
                .chain(values.iter())
                .copied()
                .chain(std::iter::once("--help"))
                .filter(|name| {
                    **name == *requested
                        || (requested.starts_with("--") && name.starts_with(requested))
                })
                .collect();
            let name = if matches.contains(&requested) {
                requested
            } else if matches.len() == 1 {
                matches[0]
            } else if matches.len() > 1 {
                return Err(error(
                    format!(
                        "ambiguous option: {requested} could match {}",
                        matches.join(", ")
                    ),
                    uninstall,
                ));
            } else {
                unknown.push(word.clone());
                continue;
            };
            if name == "--help" {
                return Err(help(uninstall));
            }
            if flags.contains(&name) {
                if let Some(value) = explicit {
                    return Err(error(
                        format!("argument {name}: ignored explicit argument '{value}'"),
                        uninstall,
                    ));
                }
                if name == "--version" {
                    return Err(Exit {
                        code: 0,
                        stdout: concat!("local-ai ", env!("CARGO_PKG_VERSION"), "\n").into(),
                        stderr: String::new(),
                    });
                }
                result.flags.insert(name.into());
                continue;
            }
            let value = if let Some(value) = explicit {
                value.to_owned()
            } else {
                let Some(value) = input.get(index) else {
                    return Err(error(
                        format!("argument {name}: expected one argument"),
                        uninstall,
                    ));
                };
                if value.starts_with('-') && value.parse::<i64>().is_err() {
                    return Err(error(
                        format!("argument {name}: expected one argument"),
                        uninstall,
                    ));
                }
                index += 1;
                value.clone()
            };
            let allowed = match name {
                "--mode" => Some(["fast", "deep", "max"].as_slice()),
                "--pref" => Some(["auto", "20", "18", "16", "14"].as_slice()),
                _ => None,
            };
            if let Some(allowed) = allowed {
                if !allowed.contains(&value.as_str()) {
                    return Err(error(
                        format!(
                            "argument {name}: invalid choice: '{value}' (choose from {})",
                            choices(allowed)
                        ),
                        uninstall,
                    ));
                }
            }
            if name == "--limit" && value.replace('_', "").parse::<i64>().is_err() {
                return Err(error(
                    format!("argument --limit: invalid int value: '{value}'"),
                    uninstall,
                ));
            }
            result.values.insert(name.into(), value);
        } else if !action_seen {
            if !ACTIONS.contains(&word.as_str()) {
                return Err(error(
                    format!(
                        "argument action: invalid choice: '{word}' (choose from {})",
                        choices(ACTIONS)
                    ),
                    uninstall,
                ));
            }
            result.action = word.clone();
            action_seen = true;
        } else if uninstall {
            unknown.push(word.clone());
        } else {
            result.rest.push(word.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(error(
            format!("unrecognized arguments: {}", unknown.join(" ")),
            uninstall,
        ));
    }
    if !uninstall
        && !result.rest.is_empty()
        && !["cache", "share", "update"].contains(&result.action.as_str())
    {
        return Err(error("unexpected arguments".into(), false));
    }
    Ok(result)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("HTTP client")
}
async fn request(path: &str, body: Option<Value>) -> (u16, Value) {
    request_timeout(path, body, 15).await
}
pub(crate) async fn request_timeout(path: &str, body: Option<Value>, seconds: u64) -> (u16, Value) {
    let url = format!("http://127.0.0.1:8092{path}");
    request_url(&url, body, seconds).await
}
pub(crate) async fn request_url(url: &str, body: Option<Value>, seconds: u64) -> (u16, Value) {
    let client = http_client();
    let request = if let Some(body) = body {
        client.post(url).json(&body)
    } else {
        client.get(url)
    }
    .timeout(std::time::Duration::from_secs(seconds));
    match request.send().await {
        Ok(mut reply) => {
            let code = reply.status().as_u16();
            let mut bytes = Vec::new();
            loop {
                match reply.chunk().await {
                    Ok(Some(chunk)) => {
                        bytes.extend_from_slice(&chunk[..chunk.len().min(65537 - bytes.len())]);
                        if bytes.len() == 65537 {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return (if code >= 400 { code } else { 0 }, json!({})),
                }
            }
            if code < 400 && bytes.len() > 65536 {
                return (0, json!({}));
            }
            bytes.truncate(65536);
            let value = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .filter(Value::is_object);
            (
                if code < 400 && value.is_none() {
                    0
                } else {
                    code
                },
                value.unwrap_or_else(|| json!({})),
            )
        }
        Err(_) => (0, json!({})),
    }
}
async fn launch(words: &[String]) -> i32 {
    use tokio::io::AsyncReadExt;
    let child = tokio::process::Command::new("/proc/self/exe")
        .arg("__desktop_exec")
        .args(words)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    if let Ok(mut child) = child {
        let mut error = [0];
        if matches!(
            child
                .stderr
                .take()
                .expect("desktop exec pipe")
                .read(&mut error)
                .await,
            Ok(0)
        ) {
            return 0;
        }
        let _ = child.wait().await;
    }
    notify(&format!(
        "This action needs the Omarchy desktop (`{}` was not found).",
        words[0]
    ))
    .await;
    1
}
pub(crate) async fn offered(alias: &str) -> bool {
    let (code, value) = request_timeout("/v1/models", None, 1).await;
    code == 200
        && value["object"] == "list"
        && value["data"].as_array().is_some_and(|rows| {
            rows.len() <= 32
                && rows.iter().all(Value::is_object)
                && rows
                    .iter()
                    .filter(|row| row["id"] == alias && row["object"] == "model")
                    .count()
                    == 1
        })
}
fn code_agent() -> Option<std::path::PathBuf> {
    use nix::unistd::{access, AccessFlags};
    let executable =
        |path: &std::path::Path| path.is_file() && access(path, AccessFlags::X_OK).is_ok();
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("oxa")))
        .filter(|p| executable(p))
    {
        return Some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into()))
        .map(|p| p.join("oxa"))
        .find(|p| executable(p))
}
fn code_command(agent: std::path::PathBuf, alias: &str, read_only: bool) -> Vec<String> {
    let directory = std::env::var_os("LOCAL_AI_CODE_DIRECTORY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::paths::home().join("Work"));
    let mut words = vec![
        "omarchy".into(),
        "launch".into(),
        "terminal".into(),
        "env".into(),
        "-C".into(),
        directory.to_string_lossy().into_owned(),
        "OXA_ENGINE_URL=http://127.0.0.1:8092".into(),
        agent.to_string_lossy().into_owned(),
        "--model".into(),
        alias.into(),
    ];
    if read_only {
        words.extend(["--tools".into(), "read,grep,find,ls".into()]);
    }
    words
}
async fn desktop_action(name: &str, mode: Option<&str>) -> i32 {
    let service = if name == "images" {
        "prompt-studio-app.service"
    } else {
        "local-ai-chat.service"
    };
    if command("systemctl", &["--user", "start", service]).await != Some(0) {
        notify(if name == "images" { "The local image service could not start. Details: journalctl --user -u prompt-studio-app." }
            else { "The local chat service could not start. Details: journalctl --user -u local-ai-chat." }).await;
        return 1;
    }
    if name == "ornith" && !offered("ornith-35b-a3b-local").await {
        notify("Ornith is not set up on this machine. Pick another model in Local AI.").await;
        return 1;
    }
    if ["flash", "plan"].contains(&name) && !offered("qwen3.8-flash-next-local").await {
        notify("Flash is not ready on this machine. Run `local-ai status` to see why.").await;
        return 1;
    }
    if name == "mode" {
        let Some(mode) = mode.filter(|m| ["fast", "deep", "max"].contains(m)) else {
            return 2;
        };
        let current = crate::cli_status::project().await.unwrap_or(json!({}));
        let alias = match current["selected_model"].as_str() {
            Some("moe") => "qwen3.6-35b-a3b-local",
            Some("ornith") => "ornith-35b-a3b-local",
            _ => {
                notify("Context modes are for Ornith and Qwen 3.6 MoE.").await;
                return 1;
            }
        };
        let (code, body) = request("/_local/mode", Some(json!({"model":alias,"mode":mode}))).await;
        if code == 200 {
            return 0;
        }
        notify(&reason(&body, "That context mode is unavailable.")).await;
        return 1;
    }
    if name == "code" {
        let Some(agent) = code_agent() else {
            notify("The coding agent (oxa) is not installed. Reinstall ox-llm or run `local-ai doctor`.").await;
            return 1;
        };
        let current = crate::cli_status::project().await.unwrap_or(json!({}));
        let alias = match current["selected_model"].as_str() {
            Some("moe") => "qwen3.6-35b-a3b-local",
            Some("ornith") => "ornith-35b-a3b-local",
            Some("flash") if current["flash_available"] == true => "qwen3.8-flash-next-local",
            _ => {
                notify("The selected local model is unavailable. Choose a model in Local AI and try again.").await;
                return 1;
            }
        };
        return launch(&code_command(agent, alias, false)).await;
    }
    if ["moe", "ornith", "flash"].contains(&name) {
        let alias = match name {
            "moe" => "qwen3.6-35b-a3b-local",
            "ornith" => "ornith-35b-a3b-local",
            _ => "qwen3.8-flash-next-local",
        };
        let (code, value) = request("/_local/select", Some(json!({"model":alias}))).await;
        if code == 200 {
            return 0;
        }
        notify(&reason(
            &value,
            "Model selection is unavailable while a request is running.",
        ))
        .await;
        return 1;
    }
    if name == "plan" {
        let Some(agent) = code_agent() else {
            notify("The coding agent (oxa) is not installed. Reinstall ox-llm or run `local-ai doctor`.").await;
            return 1;
        };
        return launch(&code_command(agent, "qwen3.8-flash-next-local", true)).await;
    }
    if name == "images" && request("/_local/unload", Some(json!({}))).await.0 == 409 {
        notify("Chat is still answering. Image generation can start when it finishes.").await;
    }
    launch(&[
        "omarchy".into(),
        "launch".into(),
        "webapp".into(),
        if name == "images" {
            "http://127.0.0.1:8090".into()
        } else {
            "http://127.0.0.1:8092".into()
        },
    ])
    .await
}
async fn command(name: &str, args: &[&str]) -> Option<i32> {
    tokio::process::Command::new(name)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .ok()
        .map(|s| s.code().unwrap_or(1))
}
async fn notify(message: &str) {
    if command("notify-send", &["Local AI", message])
        .await
        .is_none()
    {
        eprintln!("local-ai: {message}");
    }
}
fn reason(body: &Value, fallback: &str) -> String {
    body.get("error")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("message").and_then(Value::as_str))
        })
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().chars().take(300).collect())
        .unwrap_or_else(|| fallback.to_owned())
}
pub(crate) fn plain_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub async fn run(args: Args) -> i32 {
    match args.action.as_str() {
        "chat" | "images" | "moe" | "ornith" | "flash" | "plan" | "code" | "mode" => {
            desktop_action(&args.action, args.value("--mode")).await
        }
        "status" => crate::cli_status::run(args.flag("--json")).await,
        "panel" => {
            use std::os::unix::process::ExitStatusExt;
            match tokio::process::Command::new("omarchy-shell")
                .args([
                    std::env::var("LOCAL_AI_PANEL_ID")
                        .unwrap_or_else(|_| "snapdragon.local-ai".into()),
                    "toggle".into(),
                ])
                .status()
                .await
            {
                Ok(status) => status
                    .code()
                    .unwrap_or_else(|| -status.signal().unwrap_or(1)),
                Err(_) => {
                    eprintln!("local-ai: the panel needs the Omarchy shell (omarchy-shell was not found). Try `local-ai status`.");
                    1
                }
            }
        }
        "harness-forget" => {
            if args
                .value("--harness")
                .is_some_and(|name| !plain_name(name))
            {
                return 2;
            }
            let body = args
                .value("--harness")
                .map_or(json!({}), |name| json!({"harness":name}));
            i32::from(request("/_local/harness/forget", Some(body)).await.0 != 200)
        }
        "harness-prepare" => {
            let Some(name) = args.value("--harness").filter(|name| plain_name(name)) else {
                return 2;
            };
            if command("systemctl", &["--user", "start", "local-ai-chat.service"]).await != Some(0)
            {
                return 1;
            }
            i32::from(
                request_timeout(
                    "/_local/harness/prepare",
                    Some(json!({"harness":name})),
                    300,
                )
                .await
                .0 != 200,
            )
        }
        "flash-cache" => {
            let Some(pref) = args.value("--pref") else {
                return 2;
            };
            if request("/_local/flash/cache", Some(json!({"pref":pref})))
                .await
                .0
                == 200
            {
                return 0;
            }
            notify("The Flash tier was not changed: a request is running, or this model has no tiers. Try again when it is idle.").await;
            1
        }
        "load" | "unload" => {
            if args.action == "load"
                && command("systemctl", &["--user", "start", "local-ai-chat.service"]).await
                    != Some(0)
            {
                notify("The local chat service could not start. Details: journalctl --user -u local-ai-chat.").await;
                return 1;
            }
            let (code, body) = request_timeout(
                &format!("/_local/{}", args.action),
                Some(json!({})),
                if args.action == "load" { 200 } else { 15 },
            )
            .await;
            if code == 200 {
                return 0;
            }
            let message = reason(
                &body,
                if code == 409 || code == 503 {
                    "A model is working. Try again when it finishes."
                } else {
                    "The local model service is not answering. Run `local-ai status` to see why."
                },
            );
            notify(&message).await;
            1
        }
        "clear-cache" => {
            if request_timeout("/_local/cache/clear", Some(json!({})), 30)
                .await
                .0
                == 200
            {
                return 0;
            }
            notify("Prefix cache is busy or unavailable. Try again when the request finishes.")
                .await;
            1
        }
        "ledger" => {
            let limit = args
                .value("--limit")
                .and_then(|v| v.replace('_', "").parse::<i64>().ok())
                .unwrap_or(50)
                .clamp(1, 200);
            let value = crate::ledger::recent(limit as usize);
            if args.flag("--json") {
                println!("{}", crate::paths::python_json(&value));
            } else if value["ledger_available"] == true {
                println!(
                    "{} requests recorded",
                    value["rows"].as_array().map_or(0, Vec::len)
                );
            } else {
                println!("No request ledger");
            }
            0
        }
        "storage" => {
            let value = storage_status().await;
            if args.flag("--json") {
                println!("{}", crate::paths::python_json(&value));
            } else if let Some(rows) = value["entries"].as_array() {
                println!(
                    "{}",
                    rows.iter()
                        .map(|row| format!(
                            "{}\t{}\t{:.2} GiB\t{}\t{}",
                            row["kind"].as_str().unwrap_or(""),
                            row["state"].as_str().unwrap_or(""),
                            row["bytes"].as_f64().unwrap_or(0.0) / (1_u64 << 30) as f64,
                            row["name"].as_str().unwrap_or(""),
                            row["id"].as_str().unwrap_or("")
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            i32::from(value["storage_available"] != true)
        }
        "storage-delete" => {
            if command("systemctl", &["--user", "start", "local-ai-chat.service"]).await != Some(0)
            {
                notify("The local chat service could not start. Details: journalctl --user -u local-ai-chat.").await;
                return 1;
            }
            let Some(id) = args.value("--id").filter(|id| storage_id(id)) else {
                return 2;
            };
            if request_timeout("/_local/storage/delete", Some(json!({"id":id})), 120)
                .await
                .0
                == 200
            {
                return 0;
            }
            notify("Nothing was deleted: that cache is in use or a job is running. Try again when it finishes.").await;
            1
        }
        "share" => share(&args).await,
        "cache" => cache(&args).await,
        _ => {
            eprintln!(
                "local-ai: {} is not implemented by the Rust controller yet.",
                args.action
            );
            1
        }
    }
}

async fn cache(args: &Args) -> i32 {
    let bad = || {
        eprintln!("{CACHE_USAGE}");
        2
    };
    if args.value("--session").is_some() && args.flag("--all") {
        return bad();
    }
    if args.rest.is_empty() {
        if args.value("--session").is_some() || args.flag("--all") {
            return bad();
        }
        if !args.flag("--json") {
            println!("{CACHE_USAGE}");
            return 0;
        }
        println!("{}", crate::paths::python_json(&cache_status().await));
        return 0;
    }
    let body = match args.rest[0].as_str() {
        "status"
            if args.rest.len() == 1 && args.value("--session").is_none() && !args.flag("--all") =>
        {
            let value = cache_status().await;
            if args.flag("--json") {
                println!("{}", crate::paths::python_json(&value));
            } else {
                println!("{}", cache_text(&value));
            }
            return i32::from(value["cache_available"] != true);
        }
        "budget"
            if args.rest.len() == 2 && args.value("--session").is_none() && !args.flag("--all") =>
        {
            let text = &args.rest[1];
            let budget = if text == "off" || text == "0" {
                Some(0)
            } else {
                text.parse::<f64>()
                    .ok()
                    .filter(|n| (4.0..=4096.0).contains(n))
                    .map(|n| (n * 1_000_000_000.0).round_ties_even() as u64)
            };
            let Some(budget) = budget else {
                return bad();
            };
            let (code, value) =
                request_timeout("/_local/cache/budget", Some(json!({"bytes":budget})), 30).await;
            if code == 200 {
                if args.flag("--json") {
                    println!("{}", crate::paths::python_json(&value));
                } else if budget == 0 {
                    println!("Cache budget: off.");
                } else {
                    println!(
                        "Cache budget: {} GB.",
                        crate::paths::general_number(budget as f64 / 1_000_000_000.0)
                    );
                }
                return 0;
            }
            code
        }
        "clear" if args.rest.len() == 1 => {
            let payload = if let Some(session) = args.value("--session") {
                if session.is_empty()
                    || session.len() > 128
                    || !session
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                {
                    return bad();
                }
                json!({"scope":"session","session":session})
            } else {
                json!({"scope":"all"})
            };
            let (code, value) = request_timeout("/_local/cache/clear", Some(payload), 30).await;
            if code == 200 {
                if args.flag("--json") {
                    println!("{}", crate::paths::python_json(&value));
                } else if let Some(freed) = value["state_cache"]["freed_bytes"].as_f64() {
                    println!("Cleared {:.2} GB of saved conversations.", freed / 1e9);
                } else {
                    println!("Cache cleared.");
                }
                return 0;
            }
            code
        }
        _ => return bad(),
    };
    let message = match body {
        409 => "A request is running or a model is loading. Try again when it finishes.",
        400 => "That value was refused. Use off or a size from 4 to 4096 GB.",
        _ => "The local model service is not answering. Run `local-ai status` to see why.",
    };
    if args.flag("--json") {
        println!("{}", crate::paths::python_json(&json!({"error":message})));
    } else {
        eprintln!("{message}");
    }
    1
}

fn valid_number(value: &Value, low: f64, high: f64) -> Value {
    if value.as_f64().is_some_and(|v| (low..=high).contains(&v)) {
        value.clone()
    } else {
        Value::Null
    }
}
fn valid_count(value: &Value) -> Value {
    value
        .as_u64()
        .filter(|v| *v <= 1 << 31)
        .map_or(json!(0), |v| json!(v))
}
async fn cache_status() -> Value {
    let (code, value) = request_timeout("/_local/cache", None, 2).await;
    if code != 200 {
        return json!({"cache_available":false});
    }
    let oldest = valid_number(&value["sessions"]["oldest_atime"], 0.0, 1e12)
        .as_f64()
        .map(|atime| {
            (chrono::Utc::now().timestamp_millis() as f64 / 1000.0 - atime).max(0.0) as u64
        });
    json!({"cache_available":true,"budget_bytes":valid_number(&value["budget_bytes"],0.0,(1_u64<<50) as f64),
        "used_bytes":valid_number(&value["used_bytes"],0.0,(1_u64<<50) as f64),
        "ttl_secs":valid_number(&value["ttl_secs"],1.0,1e9),"default_budget_bytes":64_000_000_000_u64,
        "invalid_settings":value["invalid_settings"]==true || value["invalid_settings"].as_array().is_some_and(|v|!v.is_empty()),
        "sessions":{"count":valid_count(&value["sessions"]["count"]),"oldest_age_secs":oldest}})
}
fn cache_text(value: &Value) -> String {
    if value["cache_available"] != true {
        return "Conversation cache: the local model service does not answer.".into();
    }
    let budget = match value["budget_bytes"].as_f64() {
        None => "unknown budget".into(),
        Some(0.0) => "off".into(),
        Some(v) => format!("{} GB", crate::paths::general_number(v / 1e9)),
    };
    let count = value["sessions"]["count"].as_u64().unwrap_or(0);
    let mut line = format!(
        "Conversation cache: {:.2} GB used of {budget}, {count} conversation{}",
        value["used_bytes"].as_f64().unwrap_or(0.0) / 1e9,
        if count == 1 { "" } else { "s" }
    );
    if let Some(age) = value["sessions"]["oldest_age_secs"].as_u64() {
        let age = if age < 3600 {
            format!("{} min", (age / 60).max(1))
        } else if age < 172800 {
            format!("{} h", age / 3600)
        } else {
            format!("{} d", age / 86400)
        };
        line.push_str(&format!(", oldest {age} old"));
    }
    if value["invalid_settings"] == true {
        line.push_str(" (saved budget not valid: default in use)");
    }
    line
}
fn storage_id(value: &str) -> bool {
    value.rsplit_once('-').is_some_and(|(kind, hash)| {
        hash.len() == 16
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && kind
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase()))
    })
}
async fn storage_status() -> Value {
    let (code, chat) = request_timeout("/_local/status", None, 1).await;
    let unavailable = || json!({"storage_available":false,"entries":[]});
    if code != 200
        || !chat["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|v| v == "storage"))
    {
        return unavailable();
    }
    let (code, value) = request_timeout("/_local/storage", None, 10).await;
    let Some(rows) = value["data"]
        .as_array()
        .filter(|v| v.len() <= 128 && code == 200)
    else {
        return unavailable();
    };
    let mut entries = Vec::new();
    for row in rows {
        if !row["kind"].as_str().is_some_and(|v| {
            [
                "rust-weights",
                "rust-prefix",
                "flash-sidecar",
                "llama-prefix",
                "llama-repack",
            ]
            .contains(&v)
        }) || !row["state"]
            .as_str()
            .is_some_and(|v| ["in-use", "referenced", "protected", "unreferenced"].contains(&v))
            || !row["id"].as_str().is_some_and(storage_id)
        {
            continue;
        }
        let name = row["name"]
            .as_str()
            .filter(|v| {
                !v.is_empty()
                    && v.len() <= 96
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
            .unwrap_or("");
        let model = row["model"]
            .as_str()
            .filter(|v| ["ornith", "moe", "flash", "other"].contains(v))
            .unwrap_or("");
        entries.push(json!({"id":row["id"],"kind":row["kind"],"state":row["state"],"name":name,"model":model,
            "bytes":row["bytes"].as_u64().filter(|v|*v<=1<<50).unwrap_or(0),"deletable":row["deletable"]==true && row["state"]=="unreferenced"}));
    }
    json!({"storage_available":true,"entries":entries})
}
async fn share(args: &Args) -> i32 {
    let bad = || {
        eprintln!("{}", include_str!("share-usage.txt"));
        2
    };
    let command = args.rest.first().map(String::as_str).unwrap_or("status");
    let rest = if args.rest.is_empty() {
        &[][..]
    } else {
        &args.rest[1..]
    };
    if args.flag("--rotate") && command != "key" {
        return bad();
    }
    if command == "status" && rest.is_empty() {
        let (code, value) = request_timeout("/_local/share", None, 5).await;
        if code != 200 {
            eprintln!(
                "local-ai: offline share status is not implemented by the Rust controller yet."
            );
            return 1;
        }
        if args.flag("--json") {
            println!("{}", crate::paths::python_json(&value));
        } else {
            println!("{}", share_text(&value));
        }
        return 0;
    }
    if ["on", "off"].contains(&command) && rest.len() <= usize::from(command == "on") {
        let mode = if command == "off" {
            "off"
        } else {
            rest.first().map(String::as_str).unwrap_or("tailscale")
        };
        if !["off", "tailscale", "lan"].contains(&mode) {
            return bad();
        }
        let (code, value) = request("/_local/share", Some(json!({"mode":mode}))).await;
        if code != 200 {
            eprintln!(
                "{}",
                value["error"]
                    .as_str()
                    .unwrap_or("The local model service refused the change.")
            );
            return 1;
        }
        if args.flag("--json") {
            println!("{}", crate::paths::python_json(&value));
        } else {
            println!("{}", share_text(&value));
        }
        if mode != "off" && value["key_shown"] != true {
            eprintln!("Run `local-ai share key` to see the API key (shown once).");
        }
        return i32::from(mode != "off" && value["active"] != true);
    }
    bad()
}
fn share_text(value: &Value) -> String {
    if value["enabled"] != true {
        let mut text = "LAN share: off (loopback only).".to_owned();
        if let Some(paused) = value["paused"].as_str().filter(|v| !v.is_empty()) {
            text.push_str(&format!("\n  {paused}"));
        }
        return text;
    }
    let mut lines = vec![format!(
        "LAN share: {} {}",
        value["mode"].as_str().unwrap_or("None"),
        if value["active"] == true {
            "ON"
        } else {
            "requested, NOT active"
        }
    )];
    if let Some(urls) = value["urls"].as_array() {
        for url in urls {
            if let Some(url) = url.as_str() {
                lines.push(format!("  {url}"));
            }
        }
    }
    lines.push(format!(
        "  key: {}{}",
        value["key_fingerprint"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or("none"),
        if value["key_shown"] == true {
            ""
        } else {
            " (not shown yet: local-ai share key)"
        }
    ));
    for key in ["error", "warning"] {
        if let Some(text) = value[key].as_str().filter(|v| !v.is_empty()) {
            lines.push(format!("  {key}: {text}"));
        }
    }
    lines.join("\n")
}
