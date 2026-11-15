use std::collections::{HashMap, HashSet};

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

pub struct Exit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Default)]
pub struct Args {
    pub action: String,
    pub rest: Vec<String>,
    pub(crate) flags: HashSet<String>,
    pub(crate) values: HashMap<String, String>,
    pub(crate) records: Vec<String>,
}

impl Args {
    pub(crate) fn value(&self, name: &str) -> Option<&str> {
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

fn repr(value: &str) -> String {
    const PRINTABLE: &[[u32; 2]] = &include!("cli-printable-ranges.rs");
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = String::new();
    result.push(quote);
    for c in value.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            _ if c == quote => {
                result.push('\\');
                result.push(c);
            }
            _ => {
                let point = c as u32;
                if (' '..='~').contains(&c)
                    || PRINTABLE
                        .iter()
                        .any(|span| (span[0]..=span[1]).contains(&point))
                {
                    result.push(c);
                } else if point < 256 {
                    result.push_str(&format!("\\x{point:02x}"));
                } else if point < 65536 {
                    result.push_str(&format!("\\u{point:04x}"));
                } else {
                    result.push_str(&format!("\\U{point:08x}"));
                }
            }
        }
    }
    result.push(quote);
    result
}

fn decimal(c: char) -> Option<u8> {
    const ZEROES: &[u32] = &include!("cli-decimal-zeroes.rs");
    let point = c as u32;
    ZEROES
        .iter()
        .find_map(|zero| (point >= *zero && point < *zero + 10).then(|| (point - zero) as u8))
}
// argparse's int conversion accepts Unicode decimal digits and single
// underscores, with Python's default 4300-digit conversion limit. Saturating
// the value is sufficient for the CLI's later 1..=200 ledger clamp.
pub(crate) fn integer(value: &str) -> Option<i64> {
    let text = value.trim();
    let negative = text.starts_with('-');
    let text = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mut previous_digit, mut digits, mut number) = (false, 0, 0_i64);
    for c in text.chars() {
        if let Some(digit) = decimal(c) {
            number = number.saturating_mul(10).saturating_add(digit.into());
            digits += 1;
            previous_digit = true;
        } else if c == '_' && previous_digit {
            previous_digit = false;
        } else {
            return None;
        }
    }
    (previous_digit && digits <= 4300).then_some(if negative { -number } else { number })
}
fn negative_number(value: &str) -> bool {
    let Some(text) = value.strip_prefix('-') else {
        return false;
    };
    if let Some((whole, fractional)) = text.split_once('.') {
        whole.chars().all(|c| decimal(c).is_some())
            && !fractional.is_empty()
            && fractional.chars().all(|c| decimal(c).is_some())
    } else {
        !text.is_empty() && text.chars().all(|c| decimal(c).is_some())
    }
}

fn option_matches<'a>(
    requested: &str,
    flags: &'a [&'a str],
    values: &'a [&'a str],
) -> Vec<&'a str> {
    let mut matches: Vec<&str> = flags
        .iter()
        .chain(values.iter())
        .copied()
        .chain(std::iter::once("--help"))
        .filter(|name| {
            **name == *requested || (requested.starts_with("--") && name.starts_with(requested))
        })
        .collect();
    // argparse preserves option declaration order in ambiguity errors.
    let order = [
        "--help",
        "--version",
        "--rotate",
        "--json",
        "--id",
        "--pref",
        "--harness",
        "--mode",
        "--limit",
        "--session",
        "--all",
        "--day",
        "--write",
        "--status",
        "--soak",
        "--check",
        "--no-chat",
        "--apply",
        "--remove-models",
        "--record",
    ];
    matches.sort_by_key(|name| order.iter().position(|item| item == name));
    matches
}

fn option_like(word: &str, flags: &[&str], values: &[&str]) -> bool {
    if !word.starts_with('-') || word == "-" {
        return false;
    }
    let requested = word.split_once('=').map_or(word, |(name, _)| name);
    if (word.starts_with("-h") && !word.starts_with("--"))
        || !option_matches(requested, flags, values).is_empty()
    {
        return true;
    }
    !negative_number(word) && !word.contains(' ')
}

pub fn parse(words: &[String]) -> Result<Args, Exit> {
    if words.first().map(String::as_str) == Some("dense") {
        return Err(Exit {code:2, stdout:String::new(), stderr:"The dense Qwen3.8 27B model was retired in v0.1.2. Use `local-ai ornith` or `local-ai moe`.\n".into()});
    }
    let uninstall = words.first().map(String::as_str) == Some("uninstall");
    let input = if uninstall { &words[1..] } else { words };
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
        if !positional_only && word.starts_with("-h") && !word.starts_with("--") {
            if let Some(value) = word.strip_prefix("-h=") {
                return Err(error(
                    format!(
                        "argument -h/--help: ignored explicit argument {}",
                        repr(value)
                    ),
                    uninstall,
                ));
            }
            return Err(help(uninstall));
        }
        if !positional_only && option_like(word, flags, values) {
            let (requested, explicit) = word
                .split_once('=')
                .map_or((word.as_str(), None), |(k, v)| (k, Some(v)));
            let matches = option_matches(requested, flags, values);
            let name = if matches.contains(&requested) {
                requested
            } else if matches.len() == 1 {
                matches[0]
            } else if matches.len() > 1 {
                return Err(error(
                    format!(
                        "ambiguous option: {word} could match {}",
                        matches.join(", ")
                    ),
                    uninstall,
                ));
            } else {
                unknown.push(word.clone());
                continue;
            };
            if name == "--help" {
                if let Some(value) = explicit {
                    return Err(error(
                        format!(
                            "argument -h/--help: ignored explicit argument {}",
                            repr(value)
                        ),
                        uninstall,
                    ));
                }
                return Err(help(uninstall));
            }
            if flags.contains(&name) {
                if let Some(value) = explicit {
                    return Err(error(
                        format!("argument {name}: ignored explicit argument {}", repr(value)),
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
                if option_like(value, flags, values) {
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
                            "argument {name}: invalid choice: {} (choose from {})",
                            repr(&value),
                            choices(allowed)
                        ),
                        uninstall,
                    ));
                }
            }
            if name == "--limit" && integer(&value).is_none() {
                return Err(error(
                    format!("argument --limit: invalid int value: {}", repr(&value)),
                    uninstall,
                ));
            }
            if uninstall && name == "--record" {
                result.records.push(value.clone());
            }
            result.values.insert(name.into(), value);
        } else if !action_seen {
            if !ACTIONS.contains(&word.as_str()) {
                return Err(error(
                    format!(
                        "argument action: invalid choice: {} (choose from {})",
                        repr(word),
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
