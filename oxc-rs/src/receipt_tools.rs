//! Native receipt tools. Rule order and expressions mirror tools/export_receipt.py.
//! Match failures refuse the operation; matched private values never reach diagnostics.
use fancy_regex::{Regex, RegexBuilder};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const SCANNER: &str = "export_receipt/1";
const SCHEMA: &str = "snapdragon-local-ai-lab/receipt/v1";
const RULES: &[(&str, &str)] = &[
    (
        "home-path",
        r#"(?:^|(?<![A-Za-z0-9_.<>-]))/(?:home|Users|root|var/home)/[A-Za-z0-9_.-]|[A-Za-z]:\\Users\\"#,
    ),
    (
        "tilde-path",
        r#"(?:^|(?<![A-Za-z0-9_.<>/-]))~/|\$\{?HOME\b|%USERPROFILE%"#,
    ),
    (
        "abs-path",
        r#"(?:^|(?<![A-Za-z0-9_.<>/-]))/(?:run|tmp|var|etc|opt|srv|dev|sys|proc|usr|boot|private|Library)/[A-Za-z0-9_.@-]"#,
    ),
    (
        "user-slice",
        r#"\buser-[0-9]+\.slice|\buser@[0-9]+\.service|/run/user/[0-9]+"#,
    ),
    (
        "mac-address",
        r#"(?<![0-9A-Fa-f:-])[0-9A-Fa-f]{2}([:-])(?:[0-9A-Fa-f]{2}\1){4}[0-9A-Fa-f]{2}(?![0-9A-Fa-f:-])"#,
    ),
    (
        "device-serial",
        r#"(?i)(?<![A-Za-z0-9])(?:id_serial(?:_short)?|(?:product|board|system|device|chassis|disk|hw|hardware)?[_ -]?serial(?:[_ -]?(?:number|no|num))?|service[_ -]?tag|imei|hwid)["']?\s*[:=]\s*["']?[A-Za-z0-9][A-Za-z0-9._-]{5,}"#,
    ),
    (
        "uuid",
        r#"(?<![0-9A-Fa-f-])[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}(?![0-9A-Fa-f-])"#,
    ),
    (
        "email",
        r#"[A-Za-z0-9._%+-]+@(?![0-9a-fA-F]+\.)[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.(?!(?:service|slice|scope|mount|socket|target|timer)\b)[A-Za-z]{2,}"#,
    ),
    (
        "mount-path",
        r#"(?:^|(?<![A-Za-z0-9_.<>/-]))/(?:mnt|media|run/media|Volumes)/[A-Za-z0-9_.@-]"#,
    ),
    (
        "private-ip",
        r#"(?<![0-9.])(?:10\.\d{1,3}|192\.168|172\.(?:1[6-9]|2\d|3[01])|169\.254|100\.(?:6[4-9]|[7-9]\d|1[01]\d|12[0-7]))\.\d{1,3}\.\d{1,3}(?![0-9.])"#,
    ),
    (
        "private-key",
        r#"-----BEGIN (?:[A-Z]+ )*PRIVATE KEY(?: BLOCK)?-----"#,
    ),
    (
        "api-key",
        r#"\bsk-[A-Za-z0-9_-]{20,}|\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}|\bgithub_pat_[A-Za-z0-9_]{30,}|\bxox[bpas]-[A-Za-z0-9-]{10,}|\b(?:AKIA|ASIA)[0-9A-Z]{16}\b|\bhf_[A-Za-z0-9]{30,}"#,
    ),
    (
        "secret-assignment",
        r#"(?i)\b[a-z0-9_-]*(?:api[_-]?key|secret|passw(?:or)?d|bearer|authorization|access[_-]?token|refresh[_-]?token|auth[_-]?token)["']?\s*[:=]\s*["']?(?!<redacted>|null|none|false|true)[A-Za-z0-9_/+=.-]{8,}"#,
    ),
    (
        "price",
        r#"[$€£]\s?\d|\b\d[\d.,]*\s?(?:USD|EUR|SEK|NOK|DKK|GBP)\b|\b\d[\d.,]*\s?kr\b"#,
    ),
];
const TRIVIAL_MAC: &str = r"^(?:00|ff|FF)(?:[:-](?:00|ff|FF)){5}$";
const TREE_SKIPS: &[&str] = &["abs-path", "tilde-path", "price", "binary-file"];
const HEX: &str = r"^(?:sha256:)?[0-9a-fA-F]{7,64}(?:…|\.\.\.)?$";
const NUM: &str = r"^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$";
const IDENT: &str = r"^[A-Za-z0-9][A-Za-z0-9_.+:=,x%-]{0,63}$";
const TIME: &str = r"^\d{4}-\d\d-\d\d(?:[T ]\d\d:\d\d(?::\d\d(?:\.\d+)?)?(?:Z|[+-]\d\d:?\d\d)?)?$";
const KEY: &str = r"^[A-Za-z0-9][A-Za-z0-9_.:+%-]{0,79}$";
const FILE_NAME: &str = r"^[A-Za-z0-9][A-Za-z0-9_.+=,-]{0,159}$";
const PROGRAM: &str = r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,47}$";
const PATH_KEY: &str = r"(?:^|[_. -])(?:path|paths|dir|dirs|directory|file|files|root|folder|location)$|[a-z](?:Path|Paths|Dir|File|Files|Root)$";
const ENV_ALLOW: &str =
    r"^(?:OMP_|GOMP_|KMP_|OXE_|GGML_|LLAMA_|OXC_|RAYON_|MALLOC_|OPENBLAS_|VECLIB_|MLX_)[A-Z0-9_]*$";
const UNIT_SLASH: &str = r"^[(\[]?(?:[A-Za-zµ%]{1,8}/(?:s|sec|ms|us|min|h|token|tokens|tok|run|step|core|thread|pass|layer|row|block|request|req|image|px|W|GB|GiB|MiB|K|1K)|\d{1,6}/\d{1,6})[)\].,;:]*$";
const DENY_KEYS: &[&str] = &[
    "home",
    "cwd",
    "pwd",
    "user",
    "login",
    "owner",
    "author",
    "email",
    "host",
    "node",
    "nodename",
    "machine",
    "bootid",
    "mac",
    "address",
    "ip",
    "url",
    "uri",
    "endpoint",
    "pid",
    "ppid",
    "tid",
    "uid",
    "gid",
    "evidence",
    "authorization",
    "bearer",
    "cookie",
    "sessionid",
    "sn",
    "wwid",
    "eui",
    "nguid",
    "imei",
    "hwid",
    "servicetag",
    "assettag",
    "devicename",
    "computername",
    "key",
];
const DENY_PARTS: &[&str] = &[
    "serial",
    "uuid",
    "hostname",
    "username",
    "passw",
    "secret",
    "apikey",
    "machineid",
    "macaddr",
    "wwn",
    "credential",
    "privatekey",
];
const TEXT_DENY: &[&str] = &[
    "prompt",
    "prompts",
    "systemprompt",
    "system",
    "userprompt",
    "input",
    "inputs",
    "text",
    "texts",
    "answer",
    "answers",
    "content",
    "contents",
    "message",
    "messages",
    "response",
    "responses",
    "completion",
    "completions",
    "output",
    "outputs",
    "stdout",
    "stderr",
    "log",
    "logs",
    "lines",
    "tail",
    "transcript",
    "window",
    "comment",
    "comments",
    "description",
    "reasoning",
    "thinking",
    "toolcalls",
    "tools",
    "choices",
    "body",
    "request",
    "scope",
    "scopecgroup",
    "cgroup",
    "session",
];
const TEXT_KEYS: &[&str] = &[
    "verdict",
    "kind",
    "classification",
    "title",
    "model",
    "modelname",
    "quant",
    "quantization",
    "name",
    "label",
    "workload",
    "status",
    "phase",
    "policy",
    "class",
    "qualityclass",
    "evidenceclass",
    "note",
    "notes",
    "caveat",
    "comparisoncaveat",
    "scopenote",
    "chip",
    "os",
    "osversion",
    "storage",
    "memory",
    "engine",
    "stack",
    "test",
    "metric",
    "unit",
    "arm",
    "variant",
    "step",
    "laptop",
    "powersource",
];
const PROMPT_FLAGS: &[&str] = &[
    "-p",
    "--prompt",
    "-sys",
    "--system-prompt",
    "--system",
    "--grammar",
    "--json-schema",
    "-j",
    "--chat-template",
    "--reverse-prompt",
    "-r",
    "--in-prefix",
    "--in-suffix",
    "--negative-prompt",
];
const SECRET_FLAGS: &[&str] = &[
    "--api-key",
    "--hf-token",
    "-hft",
    "--token",
    "--password",
    "--ssl-key-file",
    "--api-key-file",
];
type Result<T> = std::result::Result<T, String>;

// Cache only the fixed expressions. A deny word or private path must not be
// retained in the process-wide cache or included in a regex error message.
fn fixed(pattern: &'static str) -> Regex {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .expect("regex cache");
    cache
        .entry(pattern)
        .or_insert_with(|| compile(pattern).expect("reference expression"))
        .clone()
}
fn compile(pattern: &str) -> std::result::Result<Regex, fancy_regex::Error> {
    // The reference bounds each scanned line to 200,000 characters and has no
    // regex step limit. A lower library default rejects valid bounded lines.
    RegexBuilder::new(&python_pattern(pattern))
        .backtrack_limit(usize::MAX)
        .build()
}
fn py_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\x1c'..='\x1f')
}
fn python_pattern(pattern: &str) -> String {
    // Python's Unicode word class includes letters/numbers/underscore rather
    // than combining marks and join controls. Its ignore-case ASCII ranges also
    // include both dotted and dotless I. Preserve those privacy boundaries.
    let (insensitive, pattern) = pattern
        .strip_prefix("(?i)")
        .map_or((false, pattern), |p| (true, p));
    let chars: Vec<_> = pattern.chars().collect();
    let mut out = if insensitive {
        "(?i)".to_owned()
    } else {
        String::new()
    };
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            match chars[i + 1] {
                'b' => {
                    let word = include_str!("receipt-word-class.txt").trim_end();
                    out.push_str(&format!("(?:(?<!{word})(?={word})|(?<={word})(?!{word}))"));
                }
                'd' => out.push_str(include_str!("receipt-decimal-class.txt").trim_end()),
                's' => out.push_str(r"[\s\x1c-\x1f]"),
                ch => {
                    out.push('\\');
                    out.push(ch);
                }
            }
            i += 2;
            continue;
        }
        if chars[i] == '[' {
            let start = i;
            let mut j = i + 1;
            let mut has_i = false;
            if chars.get(j) == Some(&'^') {
                j += 1;
            }
            while j < chars.len() && chars[j] != ']' {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if matches!(chars[j], 'i' | 'I' | 'İ' | 'ı') {
                    has_i = true;
                }
                if chars.get(j + 1) == Some(&'-')
                    && chars.get(j + 2).is_some_and(|last| {
                        (chars[j] <= 'i' && *last >= 'i') || (chars[j] <= 'I' && *last >= 'I')
                    })
                {
                    has_i = true;
                }
                j += 1;
            }
            if j < chars.len() {
                out.extend(chars[start..j].iter());
                if insensitive && has_i {
                    out.push_str("İı");
                }
                out.push(']');
                i = j + 1;
                continue;
            }
        }
        if chars[i] == '$' {
            out.push_str(r"(?=\n?\z)");
            i += 1;
            continue;
        }
        if insensitive && matches!(chars[i], 'i' | 'I' | 'İ' | 'ı') {
            out.push_str("[iIİı]");
        } else {
            out.push(chars[i]);
        }
        i += 1;
    }
    out
}
fn is_match(pattern: &'static str, value: &str) -> Result<bool> {
    fixed(pattern)
        .is_match(value)
        .map_err(|_| "scanner match could not be completed".into())
}
fn py_lines(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = value.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if matches!(
            ch,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            out.push(&value[start..i]);
            start = i + ch.len_utf8();
            if ch == '\r' && chars.peek().is_some_and(|(_, ch)| *ch == '\n') {
                start = chars.next().expect("CRLF").0 + 1;
            }
        }
    }
    if start < value.len() {
        out.push(&value[start..]);
    }
    out
}
fn word_rx(word: &str, insensitive: bool) -> Result<Regex> {
    compile(&format!(
        "{}(?<![A-Za-z0-9]){}(?![A-Za-z0-9])",
        if insensitive { "(?i)" } else { "" },
        regex::escape(word)
    ))
    .map_err(|_| "scanner word could not be compiled".into())
}
struct Scanner {
    deny: Vec<(Regex, String)>,
}
impl Scanner {
    fn new(words: Vec<(String, String)>) -> Result<Self> {
        Ok(Self {
            deny: words
                .into_iter()
                .map(|(w, r)| Ok((word_rx(&w, true)?, r)))
                .collect::<Result<_>>()?,
        })
    }
    fn scan(&self, text: &str, exact: &HashSet<String>) -> Result<Vec<(String, usize)>> {
        let mut hits = Vec::new();
        for (index, line) in py_lines(text).into_iter().enumerate() {
            let end = line
                .char_indices()
                .nth(200_000)
                .map_or(line.len(), |(i, _)| i);
            let line = &line[..end];
            for &(name, pattern) in RULES {
                for matched in fixed(pattern).find_iter(line) {
                    let matched = matched.map_err(|_| "scanner match could not be completed")?;
                    if name == "mac-address" && is_match(TRIVIAL_MAC, matched.as_str())? {
                        continue;
                    }
                    hits.push((name.to_owned(), index + 1));
                    break;
                }
            }
            for (rx, rule) in &self.deny {
                for matched in rx.find_iter(line) {
                    if !exact.contains(
                        matched
                            .map_err(|_| "scanner word match could not be completed")?
                            .as_str(),
                    ) {
                        hits.push((rule.clone(), index + 1));
                        break;
                    }
                }
            }
        }
        Ok(hits)
    }
    fn hit(&self, text: &str) -> Result<bool> {
        Ok(!self.scan(text, &HashSet::new())?.is_empty())
    }
}

fn add_word(words: &mut Vec<(String, String)>, word: &str, rule: &str, minimum: usize) {
    let word = word.trim_matches(py_space);
    if word.chars().count() >= minimum
        && !fixed(include_str!("receipt-digit-pattern.txt").trim_end())
            .is_match(word)
            .expect("reference digit expression")
        && !words.iter().any(|(w, _)| w == word)
    {
        words.push((word.to_owned(), rule.to_owned()));
    }
}
fn system_words() -> Result<Vec<(String, String)>> {
    let mut words = Vec::new();
    for name in ["USER", "LOGNAME", "SUDO_USER"] {
        add_word(
            &mut words,
            &std::env::var(name).unwrap_or_default(),
            "user-name",
            3,
        );
    }
    if let Ok(Some(entry)) = nix::unistd::User::from_uid(nix::unistd::getuid()) {
        add_word(&mut words, &entry.name, "user-name", 3);
        for part in entry
            .gecos
            .to_string_lossy()
            .split(|ch: char| py_space(ch) || ch == ',')
        {
            add_word(&mut words, part, "real-name", 4);
        }
    }
    if let Ok(host) = nix::unistd::gethostname() {
        let host = host.to_string_lossy();
        add_word(&mut words, &host, "host-name", 3);
        add_word(
            &mut words,
            host.split('.').next().unwrap_or_default(),
            "host-name",
            3,
        );
    }
    let mut probes = vec![
        (PathBuf::from("/etc/machine-id"), "machine-id"),
        (PathBuf::from("/etc/hostname"), "host-name"),
    ];
    for (pattern, rule) in [
        ("/sys/class/dmi/id/*serial", "device-serial-value"),
        ("/sys/class/dmi/id/product_uuid", "device-serial-value"),
        ("/sys/class/nvme/*/serial", "disk-serial-value"),
        ("/sys/block/*/device/serial", "disk-serial-value"),
        ("/sys/class/net/*/address", "mac-address-value"),
        (
            "/sys/firmware/devicetree/base/serial-number",
            "device-serial-value",
        ),
    ] {
        if let Ok(paths) = glob::glob(pattern) {
            probes.extend(paths.flatten().map(|p| (p, rule)));
        }
    }
    for (path, rule) in probes {
        if let Ok(data) = fs::read(path) {
            let value = String::from_utf8_lossy(&data);
            let value = value.trim_matches(py_space).trim_matches('\0');
            if rule == "mac-address-value" && (value.is_empty() || is_match(TRIVIAL_MAC, value)?) {
                continue;
            }
            add_word(
                &mut words,
                value,
                rule,
                if rule == "host-name" { 3 } else { 6 },
            );
        }
    }
    for word in std::env::var("RECEIPT_DENY_WORDS")
        .unwrap_or_default()
        .split(',')
    {
        add_word(&mut words, word, "deny-word", 3);
    }
    Ok(words)
}

/// Path.rglob follows file links but does not descend through directory links.
fn files(root: &Path, skip_git: bool) -> Vec<PathBuf> {
    fn walk(path: &Path, skip_git: bool, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if skip_git && path.components().any(|p| p.as_os_str() == ".git") {
                    continue;
                }
                if path.is_file() {
                    out.push(path);
                } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    walk(&path, skip_git, out);
                }
            }
        }
    }
    if !root.is_dir() {
        return vec![root.to_owned()];
    }
    let mut out = Vec::new();
    walk(root, skip_git, &mut out);
    out.sort();
    out
}

#[derive(Default)]
struct Args {
    paths: Vec<String>,
    options: HashMap<String, Vec<String>>,
    flags: HashSet<String>,
}
impl Args {
    fn get(&self, name: &str) -> Option<&str> {
        self.options.get(name)?.last().map(String::as_str)
    }
    fn all(&self, name: &str) -> &[String] {
        self.options.get(name).map_or(&[], Vec::as_slice)
    }
    fn has(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
}
fn parse(words: &[String], values: &[&str], switches: &[&str]) -> Result<Args> {
    let mut args = Args::default();
    let mut index = 0;
    let mut positional = false;
    while index < words.len() {
        let word = &words[index];
        if !positional && word == "--" {
            positional = true;
            index += 1;
            continue;
        }
        if !positional && word.starts_with('-') {
            let (raw, inline) = word
                .split_once('=')
                .map_or((word.as_str(), None), |(k, v)| (k, Some(v)));
            let mut candidates = values
                .iter()
                .chain(switches)
                .copied()
                .filter(|name| name.starts_with(raw));
            let name = if values.contains(&raw) || switches.contains(&raw) {
                raw
            } else {
                let found = candidates.next().ok_or("unrecognized option")?;
                if candidates.next().is_some() {
                    return Err("ambiguous option".into());
                }
                found
            };
            if switches.contains(&name) {
                if inline.is_some() {
                    return Err("option does not accept a value".into());
                }
                args.flags.insert(name.into());
            } else {
                let value = if let Some(value) = inline {
                    value.to_owned()
                } else {
                    index += 1;
                    words.get(index).ok_or("option requires a value")?.clone()
                };
                args.options.entry(name.into()).or_default().push(value);
            }
        } else {
            args.paths.push(word.clone());
        }
        index += 1;
    }
    Ok(args)
}
fn deny(args: &Args) -> Result<Vec<(String, String)>> {
    let mut words = if args.has("--no-system-denylist") {
        Vec::new()
    } else {
        system_words()?
    };
    for word in args.all("--allow-word") {
        words.retain(|(w, _)| w.to_lowercase() != word.to_lowercase());
    }
    let mut add = |word: &str| {
        if word.chars().count() >= 3 {
            if let Some((_, rule)) = words.iter_mut().find(|(w, _)| w == word) {
                *rule = "deny-word".into();
            } else {
                words.push((word.into(), "deny-word".into()));
            }
        }
    };
    for word in args.all("--deny-word") {
        add(word);
    }
    for path in args.all("--deny-file") {
        for line in py_lines(&fs::read_to_string(path).map_err(|_| "cannot read deny file")?) {
            if !line.starts_with('#') {
                add(line.trim_matches(py_space));
            }
        }
    }
    Ok(words)
}

type AllowEntry = (String, String, String);
fn allow_file(path: &str) -> Result<(Vec<AllowEntry>, HashSet<String>, HashSet<String>)> {
    let mut entries = Vec::new();
    let mut exact = HashSet::new();
    let mut words = HashSet::new();
    for (no, line) in py_lines(&fs::read_to_string(path).map_err(|_| "cannot read allow file")?)
        .into_iter()
        .enumerate()
    {
        if line.trim_matches(py_space).is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<_> = line.split('\t').map(|s| s.trim_matches(py_space)).collect();
        if parts.len() != 3 || parts.iter().any(|v| v.is_empty()) {
            return Err(format!(
                "{}:{}: expected GLOB<TAB>RULE<TAB>REASON",
                Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                no + 1
            ));
        }
        match parts[0] {
            "!allow-exact" => {
                exact.insert(parts[1].into());
            }
            "!allow-word" => {
                words.insert(parts[1].to_lowercase());
            }
            _ => entries.push((parts[0].into(), parts[1].into(), parts[2].into())),
        }
    }
    Ok((entries, exact, words))
}
// Python fnmatchcase treats '/' and leading '.' as ordinary characters.
fn fnmatch(value: &str, pattern: &str) -> Result<bool> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("(?s)^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '[' => {
                let mut j = i + 1;
                if chars.get(j) == Some(&'!') {
                    j += 1;
                }
                if chars.get(j) == Some(&']') {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j == chars.len() {
                    out.push_str("\\[");
                } else {
                    let mut body: String = chars[i + 1..j].iter().collect();
                    let negate = body.starts_with('!');
                    if negate {
                        body.remove(0);
                    }
                    out.push('[');
                    if negate {
                        out.push('^');
                    }
                    for (n, ch) in body.chars().enumerate() {
                        if ch == '\\'
                            || ch == '['
                            || (ch == '^' && n == 0)
                            || ch == '&'
                            || ch == '~'
                        {
                            out.push('\\');
                        }
                        out.push(ch);
                    }
                    out.push(']');
                    i = j;
                }
            }
            ch => out.push_str(&regex::escape(&ch.to_string())),
        }
        i += 1;
    }
    out.push_str("\\z");
    compile(&out)
        .map_err(|_| "invalid exemption glob")?
        .is_match(value)
        .map_err(|_| "exemption glob could not be matched".into())
}
fn relative(path: &str, base: &str) -> String {
    fn absolute(raw: &str) -> Vec<std::ffi::OsString> {
        let path = Path::new(raw);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut out = Vec::new();
        for part in path.components() {
            match part {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                _ => out.push(part.as_os_str().to_owned()),
            }
        }
        out
    }
    let path = absolute(path);
    let base = absolute(base);
    let common = path.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let out: PathBuf = std::iter::repeat_n(std::ffi::OsString::from(".."), base.len() - common)
        .chain(path.into_iter().skip(common))
        .collect();
    if out.as_os_str().is_empty() {
        ".".into()
    } else {
        out.to_string_lossy().into_owned()
    }
}
fn scan_cli(args: &Args) -> Result<i32> {
    let (entries, mut exact, allowed) = if let Some(path) = args.get("--allow-file") {
        allow_file(path)?
    } else {
        (Vec::new(), HashSet::new(), HashSet::new())
    };
    exact.extend(args.all("--allow-exact").iter().cloned());
    let scanner = Scanner::new(
        deny(args)?
            .into_iter()
            .filter(|(w, _)| !allowed.contains(&w.to_lowercase()))
            .collect(),
    )?;
    let mut skips: HashSet<_> = args.all("--skip-rule").iter().map(String::as_str).collect();
    if args.get("--profile") == Some("tree") {
        skips.extend(TREE_SKIPS.iter().copied());
    }
    let base = args.get("--base").map(str::to_owned).unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let mut hits = Vec::new();
    let mut used = HashSet::new();
    for root in &args.paths {
        for path in files(Path::new(root), true) {
            let found = match fs::read(&path) {
                Err(_) => vec![("unreadable".into(), 0)],
                Ok(data) if data[..data.len().min(8192)].contains(&0) => {
                    vec![("binary-file".into(), 0)]
                }
                Ok(data) => scanner.scan(&String::from_utf8_lossy(&data), &exact)?,
            };
            let path = path.to_string_lossy().into_owned();
            for (rule, line) in found {
                if skips.contains(rule.as_str()) {
                    continue;
                }
                let rel = relative(&path, &base);
                let mut matched = false;
                for (index, (glob, allowed, _)) in entries.iter().enumerate() {
                    if (allowed == "*" || allowed == &rule) && fnmatch(&rel, glob)? {
                        used.insert(index);
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    hits.push((path.clone(), rule, line));
                }
            }
        }
    }
    for (index, (glob, rule, _)) in entries.iter().enumerate() {
        if !used.contains(&index) {
            eprintln!("unused exemption: {glob} {rule}");
        }
    }
    if args.has("--summary") {
        let mut rules: Vec<(String, usize)> = Vec::new();
        let mut paths = HashSet::new();
        for (path, rule, _) in &hits {
            paths.insert(path);
            if let Some((_, n)) = rules.iter_mut().find(|(r, _)| r == rule) {
                *n += 1;
            } else {
                rules.push((rule.clone(), 1));
            }
        }
        rules.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (rule, n) in rules {
            println!("{n:6}  {rule}");
        }
        println!("{} hits in {} files", hits.len(), paths.len());
    } else {
        for (path, rule, line) in &hits {
            println!("{path}:{line}: {rule}");
        }
    }
    Ok(i32::from(!hits.is_empty()))
}

pub fn scrub_cli(words: &[String]) -> i32 {
    if words
        .iter()
        .take_while(|w| w.as_str() != "--")
        .any(|w| matches!(w.as_str(), "-h" | "--help"))
    {
        print!("{}", include_str!("scrub-receipts-help.txt"));
        return 0;
    }
    match scrub_run(words) {
        Ok(code) => code,
        Err(_) => {
            eprintln!("scrub refused: file operation could not be completed");
            2
        }
    }
}
fn scrub_run(words: &[String]) -> Result<i32> {
    let args = parse(words, &["--home", "--user"], &["--check"])?;
    if args.paths.is_empty() {
        return Err("paths required".into());
    }
    let home = args
        .get("--home")
        .map(str::to_owned)
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
    let user = args.get("--user").map(str::to_owned).unwrap_or_else(|| {
        std::env::var("USER")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("LOGNAME").ok())
            .unwrap_or_default()
    });
    let home = home.trim_end_matches('/');
    let mut patterns = Vec::new();
    if !home.is_empty() && home != "/" {
        for (suffix, replacement) in [
            ("/Work/local-ai", "$LAB_ROOT"),
            ("/.local/share/ai-stack", "$AI_STACK"),
            ("/.local/state/oxc", "$OXC_STATE"),
            ("", "$HOME"),
        ] {
            patterns.push((
                "home-path",
                compile(&format!(
                    "{}(?![A-Za-z0-9_.-])",
                    regex::escape(&format!("{home}{suffix}"))
                ))
                .map_err(|_| "home path pattern refused")?,
                replacement,
            ));
        }
        patterns.push((
            "home-path",
            compile(&format!(
                "{}(?![A-Za-z0-9_.])",
                regex::escape(&home.replace('/', "-"))
            ))
            .map_err(|_| "home path pattern refused")?,
            "-HOME",
        ));
    }
    for (rule, pattern, replacement) in [
        ("user-slice", r"/run/user/[0-9]+", "$XDG_RUNTIME_DIR"),
        ("user-slice", r"\buser-[0-9]+\.slice", "user-UID.slice"),
        ("user-slice", r"\buser@[0-9]+\.service", "user@UID.service"),
        ("uuid", RULES[6].1, "<redacted-uuid>"),
    ] {
        patterns.push((rule, fixed(pattern), replacement));
    }
    if user.chars().count() >= 3 {
        patterns.push(("user-name", word_rx(&user, false)?, "user"));
    }
    let mut totals = BTreeMap::<&str, usize>::new();
    let mut changed = 0;
    for root in &args.paths {
        for path in files(Path::new(root), true)
            .into_iter()
            .filter(|p| p.is_file())
        {
            let data = fs::read(&path).map_err(|_| "cannot read scrub input")?;
            if data[..data.len().min(8192)].contains(&0) {
                continue;
            }
            let Ok(text) = String::from_utf8(data) else {
                continue;
            };
            let mut new = text.clone();
            let mut counts = BTreeMap::new();
            for (rule, rx, replacement) in &patterns {
                let mut next = String::new();
                let mut start = 0;
                let mut count = 0;
                for matched in rx.find_iter(&new) {
                    let matched = matched.map_err(|_| "scrub match could not be completed")?;
                    next.push_str(&new[start..matched.start()]);
                    next.push_str(replacement);
                    start = matched.end();
                    count += 1;
                }
                next.push_str(&new[start..]);
                new = next;
                if count > 0 {
                    *counts.entry(*rule).or_insert(0) += count;
                }
            }
            if new == text {
                continue;
            }
            changed += 1;
            for (rule, n) in counts {
                *totals.entry(rule).or_default() += n;
            }
            if !args.has("--check") {
                fs::write(path, new.as_bytes()).map_err(|_| "cannot write scrub output")?;
            }
        }
    }
    println!(
        "{} {changed} files; replacements: {}",
        if args.has("--check") {
            "would change"
        } else {
            "changed"
        },
        summary(&totals)
    );
    Ok(i32::from(args.has("--check") && changed > 0))
}
fn summary<K: std::fmt::Display + Ord>(counts: &BTreeMap<K, usize>) -> String {
    if counts.is_empty() {
        "nothing".into()
    } else {
        counts
            .iter()
            .map(|(rule, n)| format!("{rule}={n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// Export implementation below deliberately constructs a new tree, never writes
// the input object, and scans the complete public serialization before writing.
#[derive(Default)]
struct Report {
    kept: usize,
    dropped: BTreeMap<String, usize>,
    detail: Vec<(String, String)>,
}
impl Report {
    fn drop(&mut self, reason: &str, pointer: &str) {
        *self.dropped.entry(reason.into()).or_default() += 1;
        self.detail.push((reason.into(), pointer.into()));
    }
    fn keep(&mut self, value: Value) -> Option<Value> {
        self.kept += 1;
        Some(value)
    }
}
struct Options<'a> {
    scanner: &'a Scanner,
    prompt_hash: bool,
    source_hash: bool,
    allow_text: HashSet<String>,
    nonfinite: String,
}
fn norm(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}
fn argv_key(value: &str) -> bool {
    [
        "argv",
        "args",
        "command",
        "cmd",
        "cmdline",
        "commandline",
        "serverargs",
        "engineargs",
        "nativeargv",
    ]
    .contains(&value)
        || value.ends_with("argv")
}
fn pathish(value: &str) -> Result<bool> {
    if value.contains('\\') || value.starts_with('~') {
        return Ok(true);
    }
    for word in value.split(py_space).filter(|s| !s.is_empty()) {
        if word.contains('/') && !is_match(UNIT_SLASH, word)? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn file_name(value: &str) -> Result<Option<String>> {
    let name = value
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    Ok((is_match(FILE_NAME, name)? && !name.contains("..")).then(|| name.to_owned()))
}
fn safe_text(value: &str) -> Result<bool> {
    Ok(value.chars().count() <= 240
        && !value.contains(['\n', '\r', '@', '~'])
        && !pathish(value)?
        && is_match(r"^[A-Za-z0-9 /_.,;:+=()%<>\[\]'*×≤≥→±°µ#!?&|^-]*$", value)?)
}
fn sha(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn sort_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sort_json(&map[k])))
                    .collect(),
            )
        }
        Value::Array(array) => Value::Array(array.iter().map(sort_json).collect()),
        _ => value.clone(),
    }
}
fn digest_stub(kind: &str, value: &Value, opts: &Options<'_>) -> Value {
    let data = if let Some(s) = value.as_str() {
        s.to_owned()
    } else {
        input_json(&sort_json(value), &opts.nonfinite)
    };
    let mut out = json!({"redacted":kind,"bytes":data.len()});
    if let Some(array) = value.as_array() {
        out["count"] = json!(array.len());
    }
    if opts.prompt_hash {
        out["sha256"] = json!(sha(data.as_bytes()));
    }
    out
}
fn argv_value(value: &str, opts: &Options<'_>) -> Result<String> {
    if is_match(NUM, value)? || is_match(HEX, value)? {
        return Ok(value.into());
    }
    if pathish(value)? {
        return Ok(match file_name(value)? {
            Some(name) if !opts.scanner.hit(&name)? => format!("<path>/{name}"),
            _ => "<path>".into(),
        });
    }
    Ok(if is_match(IDENT, value)? && !opts.scanner.hit(value)? {
        value
    } else {
        "<text>"
    }
    .into())
}
fn shell_words(value: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = None;
    let mut started = false;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match quoted {
            Some('\'') => {
                if ch == '\'' {
                    quoted = None;
                } else {
                    word.push(ch);
                }
            }
            Some('"') => {
                if ch == '"' {
                    quoted = None;
                } else if ch == '\\' {
                    let next = chars.next()?;
                    if next != '\\' && next != '"' {
                        word.push('\\');
                    }
                    word.push(next);
                } else {
                    word.push(ch);
                }
            }
            _ => match ch {
                '\'' | '"' => {
                    quoted = Some(ch);
                    started = true;
                }
                '\\' => {
                    word.push(chars.next()?);
                    started = true;
                }
                ' ' | '\t' | '\n' | '\r' => {
                    if started {
                        out.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(ch);
                    started = true;
                }
            },
        }
    }
    if quoted.is_some() {
        return None;
    }
    if started {
        out.push(word);
    }
    Some(out)
}
fn sanitize_argv(value: &Value, opts: &Options<'_>) -> Result<Value> {
    let items = if let Some(text) = value.as_str() {
        let Some(words) = shell_words(text) else {
            return Ok(json!(["<unparsed>"]));
        };
        words.into_iter().map(Value::String).collect()
    } else {
        value.as_array().cloned().unwrap_or_default()
    };
    let mut out = Vec::new();
    let mut prev: Option<String> = None;
    for (index, item) in items.into_iter().enumerate() {
        if nonfinite(&item, &opts.nonfinite).is_some() {
            out.push(item);
            prev = None;
            continue;
        }
        let Some(text) = item.as_str() else {
            out.push(if item.is_number() {
                item
            } else {
                json!("<text>")
            });
            prev = None;
            continue;
        };
        let (flag, inline) = text
            .split_once('=')
            .map_or((text, None), |(f, v)| (f, Some(v)));
        if text.starts_with('-') && !is_match(NUM, text)? {
            if !is_match(r"^--?[A-Za-z0-9][A-Za-z0-9_.-]*\z", flag)? {
                out.push(json!("<text>"));
                prev = None;
                continue;
            }
            if let Some(inline) = inline {
                let value = if SECRET_FLAGS.contains(&flag) {
                    "<redacted>".into()
                } else if PROMPT_FLAGS.contains(&flag) && !is_match(NUM, inline)? {
                    "<prompt>".into()
                } else {
                    argv_value(inline, opts)?
                };
                out.push(json!(format!("{flag}={value}")));
                prev = None;
            } else {
                out.push(json!(flag));
                prev = Some(flag.into());
            }
            continue;
        }
        let clean = if prev.as_deref().is_some_and(|v| SECRET_FLAGS.contains(&v)) {
            "<redacted>".into()
        } else if prev.as_deref().is_some_and(|v| PROMPT_FLAGS.contains(&v))
            && !is_match(NUM, text)?
        {
            let stub = digest_stub("prompt", &item, opts);
            format!(
                "<prompt bytes={}{}>",
                stub["bytes"],
                stub.get("sha256")
                    .and_then(Value::as_str)
                    .map_or(String::new(), |h| format!(" sha256={h}"))
            )
        } else if inline.is_some() && is_match(r"^[A-Za-z_][A-Za-z0-9_]*\z", flag)? {
            format!(
                "{flag}={}",
                if is_match(ENV_ALLOW, flag)? {
                    argv_value(inline.unwrap_or_default(), opts)?
                } else {
                    "<redacted>".into()
                }
            )
        } else if index == 0
            || (!out.is_empty()
                && out
                    .iter()
                    .all(|v| py_string(v, &opts.nonfinite).contains('=')))
        {
            match file_name(text)? {
                Some(name) if is_match(PROGRAM, &name)? && !opts.scanner.hit(&name)? => name,
                _ => "<program>".into(),
            }
        } else {
            argv_value(text, opts)?
        };
        out.push(json!(clean));
        prev = None;
    }
    Ok(Value::Array(out))
}
fn sanitize(
    value: &Value,
    report: &mut Report,
    opts: &Options<'_>,
    pointer: &str,
    key: &str,
) -> Result<Option<Value>> {
    let nkey = norm(key);
    if nonfinite(value, &opts.nonfinite).is_some() {
        report.drop("non-finite-number", pointer);
        return Ok(None);
    }
    Ok(match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => report.keep(value.clone()),
        Value::String(text) => {
            if is_match(NUM, text)? || is_match(TIME, text)? {
                report.keep(value.clone())
            } else if !pathish(text)? && opts.scanner.hit(text)? {
                report.drop("scan-rule", pointer);
                None
            } else if is_match(HEX, text)?
                || (nkey == "path" && is_match(r"^[a-z][a-z0-9_-]{0,23}\z", text)?)
            {
                report.keep(value.clone())
            } else if is_match(PATH_KEY, key)? || pathish(text)? {
                let name = file_name(text)?;
                let allowed = if let Some(name) = &name {
                    (["model", "mtp", "head", "sidecar", "shard", "weights"]
                        .iter()
                        .any(|p| nkey.starts_with(p))
                        && [".gguf", ".safetensors", ".npz", ".dtbo"]
                            .iter()
                            .any(|e| name.to_lowercase().ends_with(e)))
                        || (["binary", "server", "cli", "engine", "program"]
                            .iter()
                            .any(|p| nkey.starts_with(p))
                            && is_match(PROGRAM, name)?)
                } else {
                    false
                };
                if allowed {
                    report.keep(json!(name.expect("allowed file name")))
                } else {
                    report.drop("path", pointer);
                    None
                }
            } else if TEXT_KEYS.contains(&nkey.as_str()) || opts.allow_text.contains(&nkey) {
                if safe_text(text)? {
                    report.keep(value.clone())
                } else {
                    report.drop("unsafe-text", pointer);
                    None
                }
            } else if is_match(IDENT, text)? {
                report.keep(value.clone())
            } else {
                report.drop("free-text", pointer);
                None
            }
        }
        Value::Array(items) => {
            if argv_key(&nkey) {
                report.kept += 1;
                Some(sanitize_argv(value, opts)?)
            } else {
                let mut out = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    if let Some(clean) =
                        sanitize(item, report, opts, &format!("{pointer}/{i}"), key)?
                    {
                        out.push(clean);
                    }
                }
                Some(Value::Array(out))
            }
        }
        Value::Object(items) => {
            let mut out = Map::new();
            let mut thermal = Vec::new();
            for (raw, value) in items {
                let is_text = value.is_string() && nonfinite(value, &opts.nonfinite).is_none();
                let k = norm(raw);
                let here = format!(
                    "{pointer}/{}",
                    if is_match(KEY, raw)? { raw } else { "<key>" }
                );
                if is_match(r"(?:^|/)thermal_zone\d+/temp$", raw)? {
                    thermal.push(value);
                    continue;
                }
                if !is_match(KEY, raw)? || pathish(raw)? || opts.scanner.hit(raw)? {
                    report.drop("unsafe-key", &here);
                    continue;
                }
                if DENY_KEYS.contains(&k.as_str())
                    || DENY_PARTS.iter().any(|p| k.contains(p))
                    || is_match(
                        r"^token$|(?:access|refresh|auth|hf|api|bearer|id|session|secret|bot|oauth)token$",
                        &k,
                    )?
                {
                    report.drop("denied-key", &here);
                    continue;
                }
                if ["prompt", "systemprompt", "userprompt", "input"].contains(&k.as_str())
                    && (is_text || value.is_array())
                    && truthy(value)
                {
                    out.insert(
                        format!("{}_redacted", public_key(raw)),
                        digest_stub("prompt", value, opts),
                    );
                    report.drop("prompt", &here);
                    continue;
                }
                if let Some(array) = value.as_array().filter(|a| !a.is_empty()) {
                    if is_match(r"(?:ids|tokenids|tokens|promptids|replyids)$", &k)?
                        && array.iter().all(integer)
                    {
                        out.insert(
                            format!("{}_redacted", public_key(raw)),
                            digest_stub("token-ids", value, opts),
                        );
                        report.drop("token-ids", &here);
                        continue;
                    }
                }
                if TEXT_DENY.contains(&k.as_str())
                    && !opts.allow_text.contains(&k)
                    && (is_text || value.is_array() || value.is_object())
                {
                    report.drop("denied-text-key", &here);
                    continue;
                }
                if argv_key(&k) && (value.is_array() || is_text) {
                    out.insert(raw.clone(), sanitize_argv(value, opts)?);
                    report.kept += 1;
                    continue;
                }
                if ["environment", "env", "environ"].contains(&k.as_str()) && value.is_object() {
                    let mut env = Map::new();
                    for (name, val) in value.as_object().expect("environment") {
                        if is_match(ENV_ALLOW, name)?
                            && (val.is_string() || val.is_number() || val.is_boolean())
                        {
                            env.insert(
                                name.clone(),
                                json!(argv_value(&py_string(val, &opts.nonfinite), opts)?),
                            );
                            report.kept += 1;
                        } else {
                            report.drop("environment", &format!("{here}/<name>"));
                        }
                    }
                    out.insert(raw.clone(), Value::Object(env));
                    continue;
                }
                if is_match(PATH_KEY, raw)? && (value.is_array() || value.is_object()) {
                    report.drop("path", &here);
                    continue;
                }
                if let Some(clean) = sanitize(value, report, opts, &here, raw)? {
                    out.insert(raw.clone(), clean);
                }
            }
            if !thermal.is_empty() {
                let values: Vec<_> = thermal
                    .into_iter()
                    .filter_map(|v| py_float(v, &opts.nonfinite))
                    .filter(|v| v.is_finite())
                    .map(|v| if v.abs() > 1000.0 { v / 1000.0 } else { v })
                    .collect();
                if !values.is_empty() {
                    let round = |n: f64| format!("{n:.1}").parse::<f64>().unwrap_or(n);
                    out.insert("thermal_summary".into(),json!({"sensors":values.len(),"min_c":round(values.iter().copied().fold(f64::INFINITY,f64::min)),"max_c":round(values.iter().copied().fold(f64::NEG_INFINITY,f64::max))}));
                    report.kept += 1;
                }
            }
            Some(Value::Object(out))
        }
    })
}
fn public_key(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect()
}
fn integer(value: &Value) -> bool {
    value
        .as_number()
        .is_some_and(|n| !n.to_string().contains(['.', 'e', 'E']))
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}

// Python accepts non-finite JSON constants. Represent them only in the private
// tree with a collision-free marker; normal scalar sanitization drops them.
// The marker is selected after reading the receipt bytes, never model weights.
fn nonfinite<'a>(value: &'a Value, prefix: &str) -> Option<&'a str> {
    let s = value.as_str()?.strip_prefix(prefix)?;
    matches!(s, "NaN" | "Infinity" | "-Infinity").then_some(s)
}
fn parse_private(raw: &[u8], prefix: &str) -> Result<Value> {
    let text = std::str::from_utf8(raw).map_err(|_| "not-json")?;
    let mut encoded = String::new();
    let mut i = 0;
    let mut quoted = false;
    let mut escaped = false;
    while i < text.len() {
        let ch = text[i..].chars().next().expect("JSON character");
        if quoted {
            encoded.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            i += ch.len_utf8();
            continue;
        }
        if ch == '"' {
            quoted = true;
            encoded.push(ch);
            i += 1;
            continue;
        }
        if let Some(token) = ["-Infinity", "Infinity", "NaN"]
            .into_iter()
            .find(|t| text[i..].starts_with(t))
        {
            encoded.push_str(&json!(format!("{prefix}{token}")).to_string());
            i += token.len();
        } else {
            encoded.push(ch);
            i += ch.len_utf8();
        }
    }
    let mut value: Value = serde_json::from_str(&encoded).map_err(|_| "not-json")?;
    fn numbers(value: &mut Value, prefix: &str) {
        match value {
            Value::Number(n) => {
                let s = n.to_string();
                if s.contains(['.', 'e', 'E']) {
                    let number = s.parse::<f64>().unwrap_or(f64::NAN);
                    *value = serde_json::Number::from_f64(number).map_or_else(
                        || {
                            json!(format!(
                                "{prefix}{}",
                                if number.is_nan() {
                                    "NaN"
                                } else if number.is_sign_negative() {
                                    "-Infinity"
                                } else {
                                    "Infinity"
                                }
                            ))
                        },
                        Value::Number,
                    );
                } else if s == "-0" {
                    *value = json!(0);
                }
            }
            Value::Array(v) => v.iter_mut().for_each(|v| numbers(v, prefix)),
            Value::Object(v) => v.values_mut().for_each(|v| numbers(v, prefix)),
            _ => {}
        }
    }
    numbers(&mut value, prefix);
    Ok(value)
}
fn float_json(number: &serde_json::Number) -> String {
    let raw = number.to_string();
    if !raw.contains(['.', 'e', 'E']) {
        return raw;
    }
    let negative = raw.starts_with('-');
    let raw = raw.trim_start_matches('-');
    let (mantissa, exponent) = raw
        .split_once(['e', 'E'])
        .map_or((raw, 0), |(m, e)| (m, e.parse::<i32>().unwrap_or(0)));
    let dot = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let first = digits.find(|c| c != '0');
    let Some(first) = first else {
        return if negative { "-0.0" } else { "0.0" }.into();
    };
    let significant = digits[first..].trim_end_matches('0');
    let exp = exponent + dot - first as i32 - 1;
    let sign = if negative { "-" } else { "" };
    if !(-4..16).contains(&exp) {
        let tail = if significant.len() > 1 {
            format!(".{}", &significant[1..])
        } else {
            String::new()
        };
        return format!("{sign}{}{tail}e{exp:+03}", &significant[..1]);
    }
    let decimals = exp + 1;
    if decimals <= 0 {
        format!(
            "{sign}0.{}{}",
            "0".repeat((-decimals) as usize),
            significant
        )
    } else if decimals as usize >= significant.len() {
        format!(
            "{sign}{significant}{}.0",
            "0".repeat(decimals as usize - significant.len())
        )
    } else {
        let index = decimals as usize;
        format!("{sign}{}.{}", &significant[..index], &significant[index..])
    }
}
fn input_json(value: &Value, prefix: &str) -> String {
    if let Some(n) = nonfinite(value, prefix) {
        return n.into();
    }
    match value {
        Value::Array(v) => format!(
            "[{}]",
            v.iter()
                .map(|v| input_json(v, prefix))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(v) => format!(
            "{{{}}}",
            v.iter()
                .map(|(k, v)| format!("{}: {}", json!(k), input_json(v, prefix)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Number(n) => float_json(n),
        _ => value.to_string(),
    }
}
fn pretty(value: &Value, prefix: &str, depth: usize) -> String {
    let indent = " ".repeat(depth);
    let next = " ".repeat(depth + 1);
    match value {
        Value::Array(v) if !v.is_empty() => format!(
            "[\n{}\n{indent}]",
            v.iter()
                .map(|v| format!("{next}{}", pretty(v, prefix, depth + 1)))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        Value::Object(v) if !v.is_empty() => format!(
            "{{\n{}\n{indent}}}",
            v.iter()
                .map(|(k, v)| format!("{next}{}: {}", json!(k), pretty(v, prefix, depth + 1)))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        _ => input_json(value, prefix),
    }
}
fn py_string(value: &Value, prefix: &str) -> String {
    if let Some(n) = nonfinite(value, prefix) {
        return match n {
            "NaN" => "nan",
            "Infinity" => "inf",
            _ => "-inf",
        }
        .into();
    }
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        _ => input_json(value, prefix),
    }
}
fn py_float(value: &Value, prefix: &str) -> Option<f64> {
    if let Some(n) = nonfinite(value, prefix) {
        return Some(match n {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        });
    }
    match value {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace('_', "").parse().ok(),
        _ => None,
    }
}
const HARVEST: &[(&str, &[&str])] = &[
    (
        "binary_sha256",
        &["binary_sha256", "server_sha256", "cli_sha256"],
    ),
    (
        "engine_commit",
        &["engine_commit", "source_commit", "commit"],
    ),
    ("model", &["model", "model_name"]),
    ("quant", &["quant", "quantization"]),
    ("threads", &["threads"]),
    ("threads_batch", &["threads_batch", "batch_threads"]),
    ("n_ctx", &["n_ctx", "ctx", "context"]),
];
fn harvest(value: &Value, found: &mut HashMap<String, Vec<Value>>) {
    match value {
        Value::Object(v) => {
            for (field, names) in HARVEST {
                for name in *names {
                    if let Some(value) = v
                        .get(*name)
                        .filter(|v| !v.is_null() && !v.is_object() && !v.is_array())
                    {
                        let values = found.entry((*field).into()).or_default();
                        if !values.iter().any(|v| {
                            v == value
                                || (v.is_number()
                                    && value.is_number()
                                    && v.as_f64() == value.as_f64())
                        }) {
                            values.push(value.clone());
                        }
                    }
                }
            }
            v.values().for_each(|v| harvest(v, found));
        }
        Value::Array(v) => v.iter().for_each(|v| harvest(v, found)),
        _ => {}
    }
}
fn single(found: &HashMap<String, Vec<Value>>, field: &str) -> Value {
    let Some(values) = found.get(field) else {
        return Value::Null;
    };
    if values.len() == 1 {
        return values[0].clone();
    }
    if values.iter().all(Value::is_string) {
        let longest = values
            .iter()
            .filter_map(Value::as_str)
            .max_by_key(|s| s.chars().count())
            .unwrap_or_default();
        if values
            .iter()
            .all(|v| longest.starts_with(v.as_str().expect("string")))
        {
            return json!(longest);
        }
    }
    Value::Null
}
fn public_name(value: &str) -> Result<String> {
    value
        .split('/')
        .map(|p| Ok(if is_match(FILE_NAME, p)? { p } else { "file" }))
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join("/"))
}
struct Input {
    name: String,
    bytes: Vec<u8>,
}
fn collect_inputs(paths: &[String]) -> Result<(Vec<Input>, BTreeMap<String, usize>)> {
    let mut out = Vec::new();
    let mut skipped = BTreeMap::new();
    for root in paths {
        let root = Path::new(root);
        if !root.is_dir() && !root.is_file() {
            return Err(format!(
                "no such receipt: {}",
                root.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        for path in files(root, false) {
            if !path.is_file() {
                continue;
            }
            let suffix = path
                .extension()
                .map(|v| format!(".{}", v.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| "(none)".into());
            if root.is_dir() && suffix != ".json" {
                *skipped.entry(suffix).or_default() += 1;
                continue;
            }
            let name = if root.is_dir() {
                path.strip_prefix(root).unwrap_or(&path).to_string_lossy()
            } else {
                path.file_name().unwrap_or_default().to_string_lossy()
            };
            out.push(Input {
                name: public_name(&name)?,
                bytes: fs::read(path).map_err(|_| "cannot read receipt input")?,
            });
        }
    }
    Ok((out, skipped))
}
fn checked_digest(value: Value, label: &str) -> Result<Value> {
    if value.is_null() {
        return Ok(value);
    }
    if let Some(s) = value.as_str() {
        if is_match(HEX, s)? {
            return Ok(value);
        }
    }
    Err(format!("{label} must be a hex digest"))
}
fn build_receipt(
    inputs: &[Input],
    skipped: &BTreeMap<String, usize>,
    hardware: Value,
    args: &Args,
    report: &mut Report,
    opts: &Options<'_>,
) -> Result<Value> {
    let mut sources = Vec::new();
    let mut found = HashMap::new();
    for input in inputs {
        let tree = match parse_private(&input.bytes, &opts.nonfinite) {
            Ok(tree) => tree,
            Err(_) => {
                report.drop("not-json", &format!("/{}", input.name));
                continue;
            }
        };
        let clean =
            sanitize(&tree, report, opts, &format!("/{}", input.name), "")?.unwrap_or(Value::Null);
        harvest(&clean, &mut found);
        let mut entry = json!({"name":input.name,"data":clean});
        if opts.source_hash {
            entry["private_file_sha256"] = json!(sha(&input.bytes));
        }
        sources.push(entry);
    }
    if let Some(ticket) = args.get("--ticket") {
        if !is_match(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,31}\z", ticket)? {
            return Err("ticket must be a short identifier".into());
        }
    }
    let mut model_files = Vec::new();
    for item in args.all("--model-file") {
        let (name, digest) = item
            .split_once('=')
            .ok_or("--model-file needs NAME=SHA256")?;
        model_files.push(json!({"name":file_name(name)?.unwrap_or_else(||"file".into()),"sha256":checked_digest(json!(digest),"model sha256")?}));
    }
    let mut text = |value: Value, pointer: &str| -> Result<Value> {
        if value.is_null() {
            Ok(value)
        } else {
            Ok(sanitize(&value, report, opts, pointer, "note")?.unwrap_or(Value::Null))
        }
    };
    let choose = |flag: &str, field: &str| -> Value {
        args.get(flag)
            .filter(|s| !s.is_empty())
            .map_or_else(|| single(&found, field), |s| json!(s))
    };
    let mut receipt = json!({
        "schema":SCHEMA,"kind":"runq-export","ticket":args.get("--ticket"),
        "title":text(json!(args.get("--title")),"/title")?,
        "created_utc":chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),"hardware":hardware,
        "engine":{"name":text(json!(args.get("--engine-name")),"/engine/name")?,"commit":checked_digest(choose("--engine-commit","engine_commit"),"engine commit")?,"binary_sha256":checked_digest(choose("--binary-sha256","binary_sha256"),"binary sha256")?},
        "model":{"name":text(choose("--model-name","model"),"/model/name")?,"quant":text(choose("--quant","quant"),"/model/quant")?,"files":model_files},
        "config":{"n_ctx":single(&found,"n_ctx"),"threads":single(&found,"threads"),"threads_batch":single(&found,"threads_batch")},
        "evidence":{"class":args.get("--evidence-class"),"n":args.get("--n").map(|s|s.parse::<serde_json::Number>()).transpose().map_err(|_|"n must be an integer")?,"quality_class":text(json!(args.get("--quality-class")),"/evidence/quality_class")?},
        "notes":[],"sources":sources,
    });
    let mut notes = Vec::new();
    for note in args.all("--note") {
        let clean = text(json!(note), "/notes")?;
        if truthy(&clean) {
            notes.push(clean);
        }
    }
    receipt["notes"] = json!(notes);
    receipt["export"] = json!({"scanner":SCANNER,"values_kept":report.kept,"dropped":report.dropped,"files_not_read":skipped,"prompt_hashes":opts.prompt_hash});
    Ok(receipt)
}
pub fn export_cli(words: &[String]) -> i32 {
    if words
        .iter()
        .take_while(|w| w.as_str() != "--")
        .any(|w| matches!(w.as_str(), "-h" | "--help"))
    {
        print!(
            "{}",
            match words.first().map(String::as_str) {
                Some("export") => include_str!("export-receipt-export-help.txt"),
                Some("scan") => include_str!("export-receipt-scan-help.txt"),
                Some("rules") => include_str!("export-receipt-rules-help.txt"),
                _ => include_str!("export-receipt-help.txt"),
            }
        );
        return 0;
    }
    match export_run(words) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("export refused: {error}");
            2
        }
    }
}
fn export_run(words: &[String]) -> Result<i32> {
    let command = words
        .first()
        .map(String::as_str)
        .ok_or("command required")?;
    if command == "rules" && words.len() == 1 {
        for (name, _) in RULES {
            println!("{name}");
        }
        println!("plus per-machine words: user-name, real-name, host-name, machine-id, device-serial-value, disk-serial-value, mac-address-value, deny-word");
        return Ok(0);
    }
    let mut values = vec!["--deny-word", "--deny-file", "--allow-word"];
    let mut switches = vec!["--no-system-denylist"];
    match command {
        "scan" => {
            values.extend([
                "--skip-rule",
                "--profile",
                "--allow-exact",
                "--allow-file",
                "--base",
            ]);
            switches.push("--summary");
        }
        "export" => {
            values.extend([
                "--out",
                "--ticket",
                "--title",
                "--engine-name",
                "--engine-commit",
                "--binary-sha256",
                "--model-name",
                "--quant",
                "--model-file",
                "--evidence-class",
                "--n",
                "--quality-class",
                "--note",
                "--hardware",
                "--allow-text-key",
                "--private-report",
            ]);
            switches.extend(["--no-prompt-hash", "--no-source-hash"]);
        }
        _ => return Err("unknown receipt command".into()),
    }
    let args = parse(&words[1..], &values, &switches)?;
    if args.paths.is_empty() {
        return Err("input paths required".into());
    }
    if let Some(profile) = args.get("--profile") {
        if !["receipt", "tree"].contains(&profile) {
            return Err("unknown scan profile".into());
        }
    }
    if command == "scan" {
        return scan_cli(&args);
    }
    for (flag, choices) in [
        ("--evidence-class", &["M", "S", "E"][..]),
        ("--quality-class", &["lossless", "reorder", "lossy"][..]),
    ] {
        if let Some(value) = args.get(flag) {
            if !choices.contains(&value) {
                return Err("invalid receipt metadata choice".into());
            }
        }
    }
    if args.get("--n").is_some_and(|s| {
        !s.parse::<serde_json::Number>()
            .is_ok_and(|n| integer(&Value::Number(n)))
    }) {
        return Err("n must be an integer".into());
    }
    let out = args.get("--out").ok_or("output path required")?;
    let scanner = Scanner::new(deny(&args)?)?;
    let (inputs, skipped) = collect_inputs(&args.paths)?;
    let hardware = args
        .get("--hardware")
        .map(fs::read)
        .transpose()
        .map_err(|_| "cannot read hardware receipt")?;
    let mut prefix = String::from("<non-finite-json:>");
    while inputs
        .iter()
        .any(|v| String::from_utf8_lossy(&v.bytes).contains(&prefix))
        || hardware
            .as_ref()
            .is_some_and(|v| String::from_utf8_lossy(v).contains(&prefix))
        || words.iter().any(|v| v.contains(&prefix))
    {
        prefix.push(':');
    }
    let opts = Options {
        scanner: &scanner,
        prompt_hash: !args.has("--no-prompt-hash"),
        source_hash: !args.has("--no-source-hash"),
        allow_text: args
            .all("--allow-text-key")
            .iter()
            .map(|s| norm(s))
            .collect(),
        nonfinite: prefix,
    };
    let mut hardware_report = Report::default();
    let hardware = hardware
        .map(|v| {
            parse_private(&v, &opts.nonfinite).and_then(|v| {
                sanitize(&v, &mut hardware_report, &opts, "/hardware", "")
                    .map(|v| v.unwrap_or(Value::Null))
            })
        })
        .transpose()?
        .unwrap_or(Value::Null);
    let mut report = Report::default();
    let mut receipt = build_receipt(&inputs, &skipped, hardware, &args, &mut report, &opts)?;
    // Preserve the reference's serialized body report: hardware counts are
    // merged only into the CLI/private report after building the receipt.
    report.kept += hardware_report.kept;
    for (reason, n) in hardware_report.dropped {
        *report.dropped.entry(reason).or_default() += n;
    }
    report.detail.extend(hardware_report.detail);
    let hits = scanner.scan(
        &(pretty(&receipt, &opts.nonfinite, 0) + "\n"),
        &HashSet::new(),
    )?;
    if let Some(path) = args.get("--private-report") {
        fs::write(
            path,
            report
                .detail
                .iter()
                .map(|(reason, pointer)| format!("{reason}\t{pointer}\n"))
                .collect::<String>(),
        )
        .map_err(|_| "cannot write private report")?;
    }
    eprintln!(
        "kept {} values; dropped: {}",
        report.kept,
        summary(&report.dropped)
    );
    if !hits.is_empty() {
        let rules: std::collections::BTreeSet<_> =
            hits.iter().map(|(rule, _)| rule.as_str()).collect();
        eprintln!(
            "export REFUSED, nothing written. Scan rules hit: {} (output lines {})",
            rules.into_iter().collect::<Vec<_>>().join(", "),
            hits.iter()
                .take(20)
                .map(|(_, no)| no.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(3);
    }
    receipt["public_safe"] = json!({"scanner":SCANNER,"hits":0});
    let path = Path::new(out);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|_| "cannot create output directory")?;
    }
    fs::write(path, pretty(&receipt, &opts.nonfinite, 0) + "\n")
        .map_err(|_| "cannot write public receipt")?;
    eprintln!(
        "wrote {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(0)
}
