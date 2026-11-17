//! C12: command-line parsing.
// ported from pi packages/coding-agent/src/cli/args.ts (docs: cli.md)
use oxa_proto::{codes, FlagSpec, Mode, OxaError, PermissionMode, ThinkingLevel, Value};
use std::path::PathBuf;

/// `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeArg {
    Text,
    Json,
    Rpc,
}

/// The `--no-<tool>` aliases (UX/DX review item 5): each is sugar for `--exclude-tools <tool>`,
/// so one exclusion precedence (`resolve_toolset`) covers both spellings and the effective tool
/// selection is identical. A denylist has no replacement meaning: every spelling appends to
/// `Args::exclude_tools`, in the order given. `--no-web-search` and `--no-mcp` are not here:
/// they also skip building the web-search hook set and the MCP manager.
pub const NO_TOOL_ALIASES: [(&str, &str); 8] = [
    ("--no-read", "read"),
    ("--no-bash", "bash"),
    ("--no-edit", "edit"),
    ("--no-write", "write"),
    ("--no-grep", "grep"),
    ("--no-find", "find"),
    ("--no-ls", "ls"),
    ("--no-subagent", "subagent"),
];

/// The tool a `--no-<tool>` alias excludes, when `flag` is one of [`NO_TOOL_ALIASES`].
pub fn no_tool_alias(flag: &str) -> Option<&'static str> {
    NO_TOOL_ALIASES
        .iter()
        .find(|(name, _)| *name == flag)
        .map(|(_, tool)| *tool)
}

/// A subcommand (first positional word).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subcommand {
    /// Typed commands admitted by the shared registry; arguments never enter Debug output.
    Registered(oxa_interactive::command_registry::CommandId, RegistryArgs),
    #[cfg(feature = "share")]
    Share(Vec<String>),
    /// Read the live snapshot of a saved session.
    Status(Vec<String>),
    /// Explicit owner ChatGPT sign-in/sign-out.
    Login(bool, LoginArgs),
    /// `oxa work <taskfile> | --issue N --model MODEL`.
    Work(Vec<String>),
    /// `oxa mcp add|remove|list|login|logout ..`: the words after `mcp`.
    Mcp(Vec<String>),
    /// `oxa install|remove|update|list ..`: the verb and the words after it.
    Pkg(String, Vec<String>),
    /// `oxa config ..`.
    Config(Vec<String>),
    /// O-152: `oxa identity [--refresh] [--offline]`.
    Identity(Vec<String>),
    /// O-152: `oxa report-issue draft|submit ..`.
    ReportIssue(Vec<String>),
    /// P6: `oxa wipe --session <id> | --all [--yes] [--engine]`.
    Wipe(Vec<String>),
    /// Publish a follow-up to a session's file inbox.
    Send(Vec<String>),
}

#[derive(Clone, PartialEq, Eq)]
pub struct RegistryArgs(pub Vec<String>);
impl std::fmt::Debug for RegistryArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RegistryArgs([redacted])")
    }
}

pub(crate) fn registry_owned(id: oxa_interactive::command_registry::CommandId) -> bool {
    use oxa_interactive::command_registry::CommandId;
    !matches!(id, CommandId::Help | CommandId::WorkStatus | CommandId::WorkBoard)
}

/// Login words can contain a mistakenly pasted callback. Never expose them
/// through Args/Subcommand Debug; command validation uses fixed diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginArgs(pub Vec<String>);
impl std::fmt::Debug for LoginArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginArgs([redacted])")
    }
}

/// A secret given on the command line. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ApiKey(pub String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Every flag of cli.md that oxa keeps. Field comments give the spellings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Args {
    /// Explicit lead shadow preset; ordinary sessions retain their existing tools.
    pub lead: bool,
    pub shadow: bool,
    pub subcommand: Option<Subcommand>,
    /// Positional prompt words (each is one prompt in print/json mode, as pi).
    pub messages: Vec<String>,
    /// `@path` arguments, in order (text files and images for the first prompt).
    pub files: Vec<String>,
    /// `-p`, `--print`.
    pub print: bool,
    /// `--output-schema FILE`: validate final text as JSON, with one repair turn.
    pub output_schema: Option<String>,
    /// Stored goal objective from a UTF-8 file (print/JSON).
    pub goal: Option<String>,
    /// Soft runtime limit for print, JSON and RPC modes (`--max-time`, seconds).
    pub max_time: Option<std::time::Duration>,
    /// Flag a saved active session after this long without progress.
    pub stall_timeout: Option<std::time::Duration>,
    /// Soft run budget: uncached prompt plus output tokens (`--max-tokens`).
    pub max_tokens: Option<u64>,
    /// `--mode text|json|rpc`.
    pub mode: Option<ModeArg>,
    /// `--export <input> [output]`.
    pub export: Option<(String, Option<String>)>,
    /// `--model <pattern>`.
    pub model: Option<String>,
    /// `--provider <name>` (an engine endpoint name; anything else is rejected later by B6).
    pub provider: Option<String>,
    /// CLI-013: `--api-key <key>`, the bearer key for the engine endpoint of this run. Sent
    /// only as `Authorization: Bearer`; never printed (its `Debug` is redacted), never stored.
    pub api_key: Option<ApiKey>,
    /// `--models <patterns>` (comma-separated).
    pub models: Option<Vec<String>>,
    /// `--list-models [search]`: `Some(None)` without a search word.
    pub list_models: Option<Option<String>>,
    /// `--thinking <level>`.
    pub thinking: Option<ThinkingLevel>,
    /// `-c`, `--continue [id]`.
    pub continue_: bool,
    /// Explicit session id supplied to `--continue`.
    pub continue_id: Option<String>,
    /// The word after a bare `--continue`, and where it sat among the messages. It is a session
    /// id only if a session with that id exists; otherwise it stays a prompt word
    /// (`--continue fix` always continued the latest session with the prompt "fix").
    pub continue_word: Option<(String, usize)>,
    /// `-r`, `--resume`.
    pub resume: bool,
    /// `--session <path|id>`.
    pub session: Option<String>,
    /// `--session-id <id>`.
    pub session_id: Option<String>,
    /// `--fork <path|id>`.
    pub fork: Option<String>,
    /// `--session-dir <dir>`.
    pub session_dir: Option<PathBuf>,
    /// `--no-session`.
    pub no_session: bool,
    /// `-n`, `--name <name>`.
    pub name: Option<String>,
    /// `--tools <list>` / `-t`.
    pub tools: Option<Vec<String>>,
    /// `--exclude-tools <list>` / `-xt`, plus every `--no-<tool>` alias (see
    /// [`NO_TOOL_ALIASES`]).
    pub exclude_tools: Vec<String>,
    /// `-nbt`, `--no-builtin-tools`.
    pub no_builtin_tools: bool,
    /// `-nt`, `--no-tools`.
    pub no_tools: bool,
    /// `--no-mcp`.
    pub no_mcp: bool,
    /// `--no-web-search` (oxa, O-170): the `web_search` tool is off for this run.
    pub no_web_search: bool,
    /// `--animation <mode>`: `ui.animation` for this run (over the setting, in memory only).
    pub animation: Option<String>,
    /// `-e`, `--extension <path>` (repeatable).
    pub extensions: Vec<String>,
    /// `-ne`, `--no-extensions`.
    pub no_extensions: bool,
    /// `--skill <path>` (repeatable).
    pub skills: Vec<String>,
    /// `-ns`, `--no-skills`.
    pub no_skills: bool,
    /// `--prompt-template <path>` (repeatable).
    pub prompt_templates: Vec<String>,
    /// `-np`, `--no-prompt-templates`.
    pub no_prompt_templates: bool,
    /// `--theme <path>` (repeatable).
    pub themes: Vec<String>,
    /// `--no-themes`.
    pub no_themes: bool,
    /// `--use-theme <name[/name]>`.
    pub use_theme: Option<String>,
    /// `-nc`, `--no-context-files`.
    pub no_context_files: bool,
    /// `--system-prompt <text|path>`.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt <text|path>` (repeatable).
    pub append_system_prompt: Vec<String>,
    /// `-a`, `--approve` = `Some(true)`; `-na`, `--no-approve` = `Some(false)`.
    pub approve: Option<bool>,
    /// `--tui-mode <mode>` (`fullscreen` | `regular`).
    pub tui_mode: Option<String>,
    /// oxa: `--permission-mode plan|workspace|ask` (the table mode; `standard`, `auto` and
    /// `yolo` set [`Args::level`]).
    pub permission_mode: Option<oxa_proto::Mode>,
    /// #202: `--standard`, `--auto`, `--yolo` (or `--permission-mode <that>`): the permission
    /// mode for this run, over `permissions.defaultMode`. The last one given wins.
    pub level: Option<oxa_proto::PermissionMode>,
    /// `--verbose`.
    pub verbose: bool,
    /// `-h`, `--help`.
    pub help: bool,
    /// `-v`, `--version`.
    pub version: bool,
    /// The hard offline switch (see the help text).
    pub offline: bool,
    /// Flags registered by extensions (`registerFlag`): name -> value.
    pub extension_flags: Vec<(String, Value)>,
    /// Unknown `--long` flags seen before the extension flags are known (resolved by the
    /// second pass, as pi does).
    pub unknown: Vec<(String, Option<String>)>,
}

/// The usage hint printed under every usage error.
const USAGE_HINT: &str = "Run `oxa --help` for the list of options.";

/// pi's session id rule (`assertValidSessionId`), spelled out for the usage error.
const SESSION_ID_ERROR: &str = "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character";

/// Subcommands take every following word raw; they are parsed by their own modules.
const COMMANDS: &[(&str, &str)] = &[
    #[cfg(feature = "share")]
    (
        "oxa share key <add|list|revoke>",
        "Manage private guest keys and daily limits",
    ),
    (
        "oxa status <session-id|session-file>",
        "Read a saved session's current progress and stall signal",
    ),
    (
        "oxa install <source> [-l]",
        "Install a package source and add it to settings",
    ),
    (
        "oxa remove <source> [-l]",
        "Remove a package source from settings (alias: uninstall)",
    ),
    (
        "oxa update [source]",
        "Update packages from their git or local sources (not oxa itself or model catalogs)",
    ),
    ("oxa list", "List installed packages"),
    (
        "oxa login chatgpt --experimental [--paste] [--replace]",
        "Owner-only native experimental ChatGPT sign-in",
    ),
    ("oxa logout chatgpt", "Remove the owner ChatGPT token record"),
    (
        "oxa send <session-id> \"text\"",
        "Queue a session follow-up, waking its TUI or RPC owner",
    ),
    ("oxa config [-l]", "Print every setting with its value and source"),
    (
        "oxa mcp <command>",
        "Manage MCP servers: add, remove, list, login, logout",
    ),
    (
        "oxa identity [--refresh]",
        "Show, or rewrite from this machine and the engine, the identity in the system prompt",
    ),
    (
        "oxa report-issue <draft|submit>",
        "Draft a scrubbed ox-llm issue; submit posts it only after you type yes",
    ),
    (
        "oxa work <taskfile> [--rerun] [--publish] [--report-problems] | --issue N --model MODEL | --status | board --shadow --repo OWNER/REPO --once",
        "Run tasks in isolated worktrees with external acceptance checks",
    ),
    (
        "oxa wipe --session <id> | --all [--yes] [--engine]",
        "Delete one session (or every session) and its spill files; --engine also drops the engine's state",
    ),
];

/// Every option oxa keeps, with pi's wording and `oxa` names.
const OPTIONS: &[(&str, &str)] = &[
    (
        "--provider <name>",
        "Provider to search for --model (requires --model)",
    ),
    (
        "--model <pattern>",
        "Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")",
    ),
    (
        "--models <patterns>",
        "Comma-separated model patterns for Ctrl+P cycling",
    ),
    (
        "--api-key <key>",
        "Bearer key for the engine endpoint of this run (not stored)",
    ),
    (
        "--list-models [search]",
        "List available models (with optional fuzzy search) and exit",
    ),
    (
        "--thinking <level>",
        "Thinking level: off, minimal, low, medium, high, xhigh, max",
    ),
    ("--mode <mode>", "Output mode: text (default), json, or rpc"),
    ("--output-schema <file>", "Check final JSON against a schema; retry once on mismatch"),
    (
        "--print, -p",
        "Non-interactive mode: process the prompts and exit",
    ),
    (
        "--export <input> [output]",
        "Export a session file to HTML and exit",
    ),
    (
        "--continue [id], -c",
        "Continue the named session, or the most recent session when no id is given",
    ),
    ("--goal <file>", "Pursue and persist this goal until complete, blocked, stalled or interrupted"),
    (
        "--max-time <seconds>",
        "Gracefully wrap up a headless run after this time (fractional seconds allowed)",
    ),
    (
        "--stall-timeout <seconds>",
        "Flag an active session after this long without progress (no automatic cancellation)",
    ),
    (
        "--max-tokens <N>",
        "Gracefully wrap up a headless run after N new prompt + output tokens (cached tokens are free)",
    ),
    ("--resume, -r", "Pick a session to resume"),
    (
        "--session <path|id>",
        "Open a session by file path or full/partial ID",
    ),
    (
        "--session-id <id>",
        "Use the current directory's session with this ID, creating it otherwise",
    ),
    (
        "--fork <path|id>",
        "Fork a session into a new session in the current directory",
    ),
    ("--session-dir <dir>", "Directory for session storage and lookup"),
    ("--no-session", "Do not save the session (ephemeral)"),
    ("--name, -n <name>", "Set the session display name"),
    (
        "--tools, -t <list>",
        "Comma-separated allowlist of tool names or patterns (*)",
    ),
    (
        "--exclude-tools, -xt <list>",
        "Comma-separated denylist of tool names or patterns (*)",
    ),
    (
        "--no-<tool>",
        "Disable one tool: read, bash, edit, write, grep, find, ls or subagent (same as --exclude-tools <tool>)",
    ),
    (
        "--animation <mode>",
        "Animation of the oxa skin for this run: arcade, minimal or off (over ui.animation)",
    ),
    (
        "--no-builtin-tools, -nbt",
        "Disable built-in tools by default, keep extension and custom tools",
    ),
    ("--no-tools, -nt", "Disable all tools by default"),
    ("--shadow", "Lead preset only: return proposals without executing actions"),
    (
        "--no-mcp",
        "Disable MCP support: no servers connect and no MCP tools",
    ),
    (
        "--no-web-search",
        "Disable the web_search tool (also OXA_WEB_SEARCH=0)",
    ),
    (
        "--extension, -e <path>",
        "Load an extension file or builtin:<name> (repeatable)",
    ),
    (
        "--no-extensions, -ne",
        "Disable extension discovery and built-in extensions",
    ),
    ("--skill <path>", "Load a skill file or directory (repeatable)"),
    ("--no-skills, -ns", "Disable skill discovery and loading"),
    (
        "--prompt-template <path>",
        "Load a prompt template file or directory (repeatable)",
    ),
    (
        "--no-prompt-templates, -np",
        "Disable prompt template discovery and loading",
    ),
    ("--theme <path>", "Load a theme file or directory (repeatable)"),
    (
        "--use-theme <name[/name]>",
        "Set the initial interactive theme for this run",
    ),
    ("--no-themes", "Disable theme discovery and loading"),
    (
        "--no-context-files, -nc",
        "Disable AGENTS.md and CLAUDE.md discovery",
    ),
    (
        "--system-prompt <text|path>",
        "Replace the system prompt with text or the contents of a file",
    ),
    (
        "--append-system-prompt <text|path>",
        "Append text or a file to the system prompt (repeatable)",
    ),
    (
        "--tui-mode <mode>",
        "Terminal mode: fullscreen (default) or regular",
    ),
    (
        "--approve, -a",
        "Trust project-local configuration and resources for this run",
    ),
    (
        "--no-approve, -na",
        "Ignore project-local configuration and resources for this run",
    ),
    (
        "--standard",
        "Permission mode standard: prompts before actions outside the workspace",
    ),
    (
        "--auto",
        "Permission mode auto: no prompts; the bash sandbox and the deny-list stay on",
    ),
    (
        "--yolo",
        "Permission mode yolo, full access: no prompts, bash runs without the sandbox",
    ),
    (
        "--permission-mode <mode>",
        "Permission mode: standard, auto, yolo, plan (or workspace, ask); default: permissions.defaultMode, else auto",
    ),
    ("--verbose", "Show verbose startup information"),
    (
        "--offline",
        "Hard offline switch: non-local connections refused, bash has no network, web_search off",
    ),
    (
        "--                ",
        "End option parsing; treat the remaining arguments as messages and files",
    ),
    ("--help, -h", "Show this help"),
    ("--version, -v", "Show the version number"),
];

fn usage(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::USAGE, message).with_hint(USAGE_HINT)
}

/// The first word of a subcommand. `None` when the word is an ordinary prompt.
fn subcommand(word: &str, rest: &[String]) -> Result<Option<Subcommand>, OxaError> {
    use oxa_interactive::command_registry::{parse_cli, COMMANDS};
    let words: Vec<_> = std::iter::once(word.to_owned())
        .chain(rest.iter().cloned())
        .collect();
    if let Some(invocation) = parse_cli(&words).filter(|i| registry_owned(i.command.id)) {
        if invocation.command.id == oxa_interactive::command_registry::CommandId::Lead
            && !invocation.args.is_empty()
        {
            return Err(usage("registered command arguments are unavailable"));
        }
        return Ok(Some(Subcommand::Registered(
            invocation.command.id,
            RegistryArgs(invocation.args.to_vec()),
        )));
    }
    if COMMANDS
        .iter()
        .any(|entry| registry_owned(entry.id) && entry.path.first() == Some(&word))
    {
        return Err(usage("registered command is unavailable on this surface"));
    }
    Ok(Some(match word {
        "login" => Subcommand::Login(false, LoginArgs(rest.to_vec())),
        "logout" => Subcommand::Login(true, LoginArgs(rest.to_vec())),
        "mcp" => Subcommand::Mcp(rest.to_vec()),
        "install" | "remove" | "uninstall" | "update" | "list" => {
            Subcommand::Pkg(word.to_string(), rest.to_vec())
        }
        #[cfg(feature = "share")]
        "share" => Subcommand::Share(rest.to_vec()),
        #[cfg(not(feature = "share"))]
        "share" => return Err(usage("this build does not include sharing")),
        "config" => Subcommand::Config(rest.to_vec()),
        "identity" => Subcommand::Identity(rest.to_vec()),
        "report-issue" => Subcommand::ReportIssue(rest.to_vec()),
        "wipe" => Subcommand::Wipe(rest.to_vec()),
        "send" => Subcommand::Send(rest.to_vec()),
        "status" => Subcommand::Status(rest.to_vec()),
        "work" => Subcommand::Work(rest.to_vec()),
        "auth" => return Err(OxaError::unsupported("the auth command")),
        _ => return Ok(None),
    }))
}

/// The value of a value flag: the `=value` part, else the next word (even one that starts with a
/// dash, as pi does for `--system-prompt`).
fn value(
    argv: &[String],
    i: &mut usize,
    inline: Option<&str>,
    flag: &str,
    expected: &str,
) -> Result<String, OxaError> {
    if let Some(v) = inline {
        return Ok(v.to_string());
    }
    match argv.get(*i) {
        Some(v) => {
            *i += 1;
            Ok(v.clone())
        }
        None => Err(usage(format!("{flag} requires {expected}"))),
    }
}

/// The next word when it can be a value (not an option, not a file argument).
fn optional_value(argv: &[String], i: &mut usize, inline: Option<&str>) -> Option<String> {
    if let Some(v) = inline {
        return Some(v.to_string());
    }
    match argv.get(*i) {
        Some(v) if !v.starts_with('-') && !v.starts_with('@') => {
            *i += 1;
            Some(v.clone())
        }
        _ => None,
    }
}

fn no_value(inline: Option<&str>, flag: &str) -> Result<(), OxaError> {
    match inline {
        Some(_) => Err(usage(format!("{flag} does not take a value"))),
        None => Ok(()),
    }
}

/// A comma-separated list: trimmed, empty entries dropped.
fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// pi's `getToolListError`: `+name` / `-name` entries modify the defaults and cannot be mixed with
/// plain names or use patterns.
fn check_tool_list(flag: &str, entries: &[String]) -> Result<(), OxaError> {
    let is_modifier = |e: &str| e.starts_with('+') || e.starts_with('-');
    let modifiers = entries.iter().filter(|e| is_modifier(e)).count();
    if modifiers == 0 {
        return Ok(());
    }
    if modifiers < entries.len() {
        return Err(usage(format!(
            "{flag}: tool names cannot be mixed with +name or -name entries"
        )));
    }
    if let Some(p) = entries.iter().find(|e| e.contains('*')) {
        return Err(usage(format!(
            "{flag}: +name and -name entries take exact tool names, not patterns: {p}"
        )));
    }
    Ok(())
}

pub(crate) fn valid_session_id(id: &str) -> bool {
    let b = id.as_bytes();
    let alnum = |c: u8| c.is_ascii_alphanumeric();
    match (b.first(), b.last()) {
        (Some(&first), Some(&last)) => {
            alnum(first) && alnum(last) && b.iter().all(|&c| alnum(c) || c == b'.' || c == b'_' || c == b'-')
        }
        _ => false,
    }
}

/// Positional words: `@path` is a file argument, anything else a prompt word.
fn positional(a: &mut Args, word: &str) {
    match word.strip_prefix('@') {
        Some(path) => a.files.push(path.to_string()),
        None => a.messages.push(word.to_string()),
    }
}

/// Parse the arguments after the program name. `ext_flags` are the flags extensions
/// registered (empty on the first pass). Errors are `usage` errors with pi's messages: the
/// documented conflicts (e.g. `--continue` with `--resume`, `--session` with `--no-session`,
/// `--fork` with `--continue`/`--resume`/`--session`, `--print` with `--mode rpc`), a missing
/// value, an invalid `--thinking` / `--mode` value, an unknown short flag. `--` stops option
/// parsing. `@file` is collected into `files`. Omitted pi commands (`auth`) =
/// `unsupported_in_oxa`.
pub fn parse_args(argv: &[String], ext_flags: &[FlagSpec]) -> Result<Args, OxaError> {
    let mut a = Args::default();
    let mut i = 0;
    let mut stop = false;
    while i < argv.len() {
        let arg = argv[i].as_str();
        i += 1;
        if !stop && i == 1 && arg == "lead" {
            let reserved_child = oxa_interactive::command_registry::COMMANDS.iter().any(|entry| {
                entry.path.first() == Some(&arg)
                    && entry
                        .path
                        .get(1)
                        .is_some_and(|part| argv.get(i).map(String::as_str) == Some(*part))
            });
            if !reserved_child
                && argv[i..]
                    .iter()
                    .any(|word| word.split('=').next() == Some("--shadow"))
            {
                a.lead = true;
                continue;
            }
        }
        // Inbox delivery, like work, accepts leading offline switches. Once a prompt,
        // another option or `--` appears, later words retain their positional meaning.
        if !stop
            && (i == 1
                || ((matches!(arg, "send" | "work")
                    || oxa_interactive::command_registry::parse_cli(&argv[i - 1..])
                        .is_some_and(|invocation| registry_owned(invocation.command.id)))
                    && argv[..i - 1].iter().all(|word| word == "--offline")))
        {
            if let Some(sub) = subcommand(arg, &argv[i..])? {
                a.subcommand = Some(sub);
                break;
            }
        }
        if stop || !arg.starts_with('-') {
            positional(&mut a, arg);
            continue;
        }
        if arg == "--" {
            stop = true;
            continue;
        }
        let (flag, inline) = match arg.starts_with("--") {
            true => match arg.split_once('=') {
                Some((f, v)) => (f, Some(v)),
                None => (arg, None),
            },
            false => (arg, None),
        };
        parse_flag(&mut a, argv, &mut i, flag, inline, ext_flags)?;
    }
    if !a.help && !a.version {
        crate::lead_preset::apply(&mut a)?;
        check_conflicts(&a)?;
    }
    Ok(a)
}

fn parse_flag(
    a: &mut Args,
    argv: &[String],
    i: &mut usize,
    flag: &str,
    inline: Option<&str>,
    ext: &[FlagSpec],
) -> Result<(), OxaError> {
    match flag {
        "--shadow" => {
            no_value(inline, flag)?;
            a.shadow = true;
        }
        "-h" | "--help" => {
            no_value(inline, flag)?;
            a.help = true;
        }
        "-v" | "--version" => {
            no_value(inline, flag)?;
            a.version = true;
        }
        "-p" | "--print" => {
            no_value(inline, flag)?;
            a.print = true;
        }
        "--output-schema" => a.output_schema = Some(value(argv, i, inline, flag, "a file")?),
        "--mode" => {
            let v = value(argv, i, inline, flag, "text, json, or rpc")?;
            a.mode = Some(match v.as_str() {
                "text" => ModeArg::Text,
                "json" => ModeArg::Json,
                "rpc" => ModeArg::Rpc,
                _ => {
                    return Err(usage(format!(
                        "Invalid mode \"{v}\". Valid values: text, json, rpc"
                    )))
                }
            });
        }
        "-c" => {
            no_value(inline, flag)?;
            a.continue_ = true;
        }
        "--continue" => {
            a.continue_ = true;
            match inline {
                Some(v) => a.continue_id = Some(v.to_string()),
                None => {
                    a.continue_word = optional_value(argv, i, None).map(|w| (w, a.messages.len()));
                }
            }
        }
        "-r" | "--resume" => {
            no_value(inline, flag)?;
            a.resume = true;
        }
        "--goal" => a.goal = Some(value(argv, i, inline, flag, "a goal file")?),
        "--max-time" => {
            let text = value(argv, i, inline, flag, "a positive number of seconds")?;
            let seconds = text
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .ok_or_else(|| usage("--max-time requires a positive, finite number of seconds"))?;
            a.max_time = Some(
                std::time::Duration::try_from_secs_f64(seconds)
                    .map_err(|_| usage("--max-time is too large"))?,
            );
        }
        "--stall-timeout" => {
            let text = value(argv, i, inline, flag, "a positive number of seconds")?;
            let seconds = text
                .parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && *s >= 0.001)
                .ok_or_else(|| usage("--stall-timeout requires at least 0.001 finite seconds"))?;
            let duration = std::time::Duration::try_from_secs_f64(seconds)
                .map_err(|_| usage("--stall-timeout is too large"))?;
            if duration.as_millis() > u64::MAX as u128 {
                return Err(usage("--stall-timeout is too large"));
            }
            a.stall_timeout = Some(duration);
        }
        "--max-tokens" => {
            let text = value(argv, i, inline, flag, "a positive integer")?;
            a.max_tokens = Some(
                text.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| usage("--max-tokens requires a positive integer"))?,
            );
        }
        "--provider" => a.provider = Some(value(argv, i, inline, flag, "a provider name")?),
        "--model" => a.model = Some(value(argv, i, inline, flag, "a model pattern")?),
        "--api-key" => {
            let key = value(argv, i, inline, flag, "a key")?;
            if key.trim().is_empty() {
                return Err(usage("--api-key requires a key"));
            }
            a.api_key = Some(ApiKey(key));
        }
        "--system-prompt" => a.system_prompt = Some(value(argv, i, inline, flag, "a value")?),
        "--append-system-prompt" => a
            .append_system_prompt
            .push(value(argv, i, inline, flag, "a value")?),
        "-n" | "--name" => a.name = Some(value(argv, i, inline, flag, "a value")?),
        "--no-session" => {
            no_value(inline, flag)?;
            a.no_session = true;
        }
        "--session" => a.session = Some(value(argv, i, inline, flag, "a path or id")?),
        "--session-id" => a.session_id = Some(value(argv, i, inline, flag, "an id")?),
        "--fork" => a.fork = Some(value(argv, i, inline, flag, "a path or id")?),
        "--session-dir" => a.session_dir = Some(PathBuf::from(value(argv, i, inline, flag, "a directory")?)),
        "--models" => a.models = Some(list(&value(argv, i, inline, flag, "comma-separated patterns")?)),
        "-nt" | "--no-tools" => {
            no_value(inline, flag)?;
            a.no_tools = true;
        }
        "-nbt" | "--no-builtin-tools" => {
            no_value(inline, flag)?;
            a.no_builtin_tools = true;
        }
        "-t" | "--tools" => {
            let entries = list(&value(argv, i, inline, flag, "a comma-separated list")?);
            check_tool_list(flag, &entries)?;
            a.tools = Some(entries);
        }
        // Repeated `-xt` / `--exclude-tools` lists accumulate, so a later exclusion never
        // undoes an earlier explicit one (a denylist has no replacement meaning, unlike the
        // `--tools` allowlist). `--no-<tool>` appends to the same list, in the order given.
        "-xt" | "--exclude-tools" => {
            a.exclude_tools
                .extend(list(&value(argv, i, inline, flag, "a comma-separated list")?))
        }
        _ if no_tool_alias(flag).is_some() => {
            no_value(inline, flag)?;
            a.exclude_tools
                .push(no_tool_alias(flag).unwrap_or_default().to_string());
        }
        "--thinking" => {
            let v = value(argv, i, inline, flag, "a thinking level")?;
            a.thinking = Some(ThinkingLevel::parse(&v).ok_or_else(|| {
                let valid: Vec<&str> = ThinkingLevel::ALL.iter().map(|l| l.as_str()).collect();
                usage(format!(
                    "Invalid thinking level \"{v}\". Valid values: {}",
                    valid.join(", ")
                ))
            })?);
        }
        "--export" => {
            let input = value(argv, i, inline, flag, "a session file")?;
            let output = optional_value(argv, i, None);
            a.export = Some((input, output));
        }
        "-e" | "--extension" => a.extensions.push(value(argv, i, inline, flag, "a path")?),
        "-ne" | "--no-extensions" => {
            no_value(inline, flag)?;
            a.no_extensions = true;
        }
        "--no-mcp" => {
            no_value(inline, flag)?;
            a.no_mcp = true;
        }
        "--no-web-search" => {
            no_value(inline, flag)?;
            a.no_web_search = true;
        }
        "--skill" => a.skills.push(value(argv, i, inline, flag, "a path")?),
        "-ns" | "--no-skills" => {
            no_value(inline, flag)?;
            a.no_skills = true;
        }
        "--prompt-template" => a.prompt_templates.push(value(argv, i, inline, flag, "a path")?),
        "-np" | "--no-prompt-templates" => {
            no_value(inline, flag)?;
            a.no_prompt_templates = true;
        }
        "--theme" => a.themes.push(value(argv, i, inline, flag, "a path")?),
        "--no-themes" => {
            no_value(inline, flag)?;
            a.no_themes = true;
        }
        "--use-theme" => a.use_theme = Some(value(argv, i, inline, flag, "a theme name")?),
        "-nc" | "--no-context-files" => {
            no_value(inline, flag)?;
            a.no_context_files = true;
        }
        "--list-models" => a.list_models = Some(optional_value(argv, i, inline)),
        "--tui-mode" => {
            let v = value(argv, i, inline, flag, "regular or fullscreen")?;
            if v != "regular" && v != "fullscreen" {
                return Err(usage(format!(
                    "Invalid TUI mode \"{v}\". Valid values: regular, fullscreen"
                )));
            }
            a.tui_mode = Some(v);
        }
        "--animation" => {
            let v = value(argv, i, inline, flag, "arcade, minimal, or off")?;
            if !oxa_interactive::skin::Animation::ALL.contains(&v.as_str()) {
                return Err(usage(format!(
                    "Invalid animation \"{v}\". Valid values: {}",
                    oxa_interactive::skin::Animation::ALL.join(", ")
                )));
            }
            a.animation = Some(v);
        }
        "--permission-mode" => {
            let v = value(
                argv,
                i,
                inline,
                flag,
                "standard, auto, yolo, plan, workspace, or ask",
            )?;
            if let Some(m) = Mode::parse(&v) {
                a.permission_mode = Some(m);
                // The last flag given wins: `workspace` and `ask` prompt, so they end an
                // earlier `--yolo` / `--auto`.
                a.level = Some(match m {
                    Mode::Plan => PermissionMode::Plan,
                    _ => PermissionMode::Standard,
                });
            } else if let Some(level) = PermissionMode::parse(&v) {
                a.level = Some(level);
            } else {
                return Err(usage(format!(
                    "Invalid permission mode \"{v}\". Valid values: standard, auto, yolo, plan, workspace, ask"
                )));
            }
        }
        "--verbose" => {
            no_value(inline, flag)?;
            a.verbose = true;
        }
        "-a" | "--approve" => {
            no_value(inline, flag)?;
            a.approve = Some(true);
        }
        "-na" | "--no-approve" => {
            no_value(inline, flag)?;
            a.approve = Some(false);
        }
        "--offline" => {
            no_value(inline, flag)?;
            a.offline = true;
        }
        "--yolo" => {
            no_value(inline, flag)?;
            a.level = Some(PermissionMode::Yolo);
        }
        "--auto" => {
            no_value(inline, flag)?;
            a.level = Some(PermissionMode::Auto);
        }
        "--standard" => {
            no_value(inline, flag)?;
            a.level = Some(PermissionMode::Standard);
        }
        _ => return parse_extension_or_unknown(a, argv, i, flag, inline, ext),
    }
    Ok(())
}

/// A long flag that oxa does not know: an extension flag when `ext` registers it (second pass),
/// otherwise kept in `unknown` for the caller. Short unknown options are usage errors.
fn parse_extension_or_unknown(
    a: &mut Args,
    argv: &[String],
    i: &mut usize,
    flag: &str,
    inline: Option<&str>,
    ext: &[FlagSpec],
) -> Result<(), OxaError> {
    let name = match flag.strip_prefix("--") {
        Some(n) if !n.is_empty() => n,
        _ => return Err(usage(format!("Unknown option: {flag}"))),
    };
    if let Some(spec) = ext.iter().find(|s| s.name == name) {
        let v = if spec.boolean {
            no_value(inline, flag)?;
            Value::Bool(true)
        } else {
            Value::String(value(argv, i, inline, flag, "a value")?)
        };
        match a.extension_flags.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = v,
            None => a.extension_flags.push((name.to_string(), v)),
        }
    } else {
        let v = optional_value(argv, i, inline);
        a.unknown.push((name.to_string(), v));
    }
    Ok(())
}

/// The documented conflicts, checked after all words are read.
fn check_conflicts(a: &Args) -> Result<(), OxaError> {
    if a.no_session && a.stall_timeout.is_some() {
        return Err(usage("--stall-timeout requires a saved session"));
    }
    if a.continue_ && a.resume {
        return Err(usage("--continue cannot be combined with --resume"));
    }
    if let Some(id) = &a.continue_id {
        if !valid_session_id(id) {
            return Err(usage(SESSION_ID_ERROR));
        }
        if a.no_session {
            return Err(usage("--continue cannot be combined with --no-session"));
        }
        if a.session.is_some() {
            return Err(usage("--continue cannot be combined with --session"));
        }
    }
    if a.session.is_some() && a.no_session {
        return Err(usage("--session cannot be combined with --no-session"));
    }
    if a.fork.is_some() {
        let present = present_flags(&[
            (a.session.is_some(), "--session"),
            (a.continue_, "--continue"),
            (a.resume, "--resume"),
            (a.no_session, "--no-session"),
        ]);
        if !present.is_empty() {
            return Err(usage(format!(
                "--fork cannot be combined with {}",
                present.join(", ")
            )));
        }
    }
    if let Some(id) = &a.session_id {
        let present = present_flags(&[
            (a.session.is_some(), "--session"),
            (a.continue_, "--continue"),
            (a.resume, "--resume"),
        ]);
        if !present.is_empty() {
            return Err(usage(format!(
                "--session-id cannot be combined with {}",
                present.join(", ")
            )));
        }
        if !valid_session_id(id) {
            return Err(usage(SESSION_ID_ERROR));
        }
    }
    if a.print && a.mode == Some(ModeArg::Rpc) {
        return Err(usage("--print cannot be combined with --mode rpc"));
    }
    if let Some(provider) = &a.provider {
        if a.model.is_none() {
            return Err(usage(format!(
                "--provider requires --model (for example: --provider {provider} --model <pattern>)"
            )));
        }
    }
    Ok(())
}

fn present_flags<'a>(flags: &[(bool, &'a str)]) -> Vec<&'a str> {
    flags
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect()
}

/// The `--help` text (cli.md's flag list with `oxa` names), followed by extension flags.
pub fn usage_text(ext_flags: &[FlagSpec]) -> String {
    let mut out = String::new();
    out.push_str("oxa - coding agent with read, bash, edit and write tools\n\n");
    out.push_str("Usage:\n  oxa [options] [--] [@files...] [messages...]\n  oxa lead --shadow [options] [messages...]\n\n");
    out.push_str("Commands:\n");
    for (left, desc) in COMMANDS {
        out.push_str(&format!("  {left:<36}{desc}\n"));
    }
    for entry in oxa_interactive::command_registry::entries(oxa_interactive::command_registry::Surface::Cli)
        .filter(|entry| registry_owned(entry.id))
    {
        out.push_str(&format!("  oxa {:<32}{}\n", entry.path.join(" "), entry.summary));
    }
    out.push_str("\nOptions:\n");
    for (left, desc) in OPTIONS {
        out.push_str(&format!("  {left:<36}{desc}\n"));
    }
    if !ext_flags.is_empty() {
        out.push_str("\nExtension CLI Flags:\n");
        for spec in ext_flags {
            let value = if spec.boolean { "" } else { " <value>" };
            let left = format!("  --{}{}", spec.name, value);
            let desc = if spec.description.is_empty() {
                "Registered by an extension"
            } else {
                &spec.description
            };
            out.push_str(&format!("{left:<36}{desc}\n"));
        }
    }
    out
}
