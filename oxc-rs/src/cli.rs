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
    let url = format!("http://127.0.0.1:8092{path}");
    let client = http_client();
    let request = if let Some(body) = body {
        client.post(url).json(&body)
    } else {
        client.get(url)
    };
    match request.send().await {
        Ok(reply) => (
            reply.status().as_u16(),
            reply.json().await.unwrap_or_else(|_| json!({})),
        ),
        Err(_) => (0, json!({})),
    }
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
fn plain_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub async fn run(args: Args) -> i32 {
    match args.action.as_str() {
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
                request("/_local/harness/prepare", Some(json!({"harness":name})))
                    .await
                    .0
                    != 200,
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
            let (code, body) = request(&format!("/_local/{}", args.action), Some(json!({}))).await;
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
            if request("/_local/cache/clear", Some(json!({}))).await.0 == 200 {
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
            if request("/_local/storage/delete", Some(json!({"id":id})))
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
                request("/_local/cache/budget", Some(json!({"bytes":budget}))).await;
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
            let (code, value) = request("/_local/cache/clear", Some(payload)).await;
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
    let (code, value) = request("/_local/cache", None).await;
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
    let (code, chat) = request("/_local/status", None).await;
    let unavailable = || json!({"storage_available":false,"entries":[]});
    if code != 200
        || !chat["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|v| v == "storage"))
    {
        return unavailable();
    }
    let (code, value) = request("/_local/storage", None).await;
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
        let (code, value) = request("/_local/share", None).await;
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
