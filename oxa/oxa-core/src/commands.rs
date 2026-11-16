//! C5: the slash-command registry and input expansion.
// ported from pi packages/coding-agent/src/core/slash-commands.ts and prompt-templates.ts (expansion)
use crate::SessionState;
use oxa_proto::{CommandSpec, Source};
use oxa_resources::{expand_skill_command, expand_template_command};
use std::collections::HashSet;

/// What a line typed by the user turns into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expanded {
    /// Send this text as the user message (a template or `/skill:name` already expanded, or
    /// plain text).
    Prompt(String),
    /// A built-in command for the front end (`/model`, `/tree`, ..): name without `/`, raw args.
    Builtin { name: String, args: String },
    /// A command registered by a hook set (extension, MCP, subagent).
    Hook { name: String, args: String },
    /// `!cmd` / `!!cmd`.
    Bash { command: String, excluded: bool },
}

/// `(name, description, argument_hint)` of every built-in, in pi's order with `/engine` and
/// `/mode` added. pi's `/login`, `/logout` and `/share` are omitted (see [`OMITTED`]).
const BUILTINS: &[(&str, &str, Option<&str>)] = &[
    ("settings", "Open settings menu", None),
    ("model", "Select model (opens selector UI)", Some("[model]")),
    ("tree", "Navigate session tree (switch branches)", None),
    ("thinking", "Set thinking level", Some("<level>")),
    (
        "engine",
        "Engine status, or load a model on the engine (replaces /llama)",
        Some("[status|load <model>]"),
    ),
    ("mode", "Set permission mode", Some("<plan|workspace|ask>")),
    ("scoped-models", "Enable/disable models for Ctrl+P cycling", None),
    (
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
        Some("[path]"),
    ),
    (
        "bug",
        "Write a local, redacted bug report bundle",
        Some("<description>"),
    ),
    ("copy", "Copy last agent message to clipboard", None),
    ("name", "Set session display name", Some("[name]")),
    ("session", "Show session info and stats", None),
    ("hotkeys", "Show all keyboard shortcuts", None),
    ("fork", "Create a new fork from a previous user message", None),
    (
        "clone",
        "Duplicate the current session at the current position",
        None,
    ),
    ("trust", "Save project trust decision for future sessions", None),
    ("mcp", "Manage MCP servers", None),
    ("new", "Start a new session", None),
    (
        "compact",
        "Manually compact the session context",
        Some("[instructions]"),
    ),
    ("resume", "Resume another session", None),
    (
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        None,
    ),
    ("help", "List the commands", None),
    ("usage", "Show token usage of this session", None),
    ("quit", "Quit oxa", None),
];

/// pi commands oxa does not provide. Typing one yields `Expanded::Builtin` with a name that is
/// not in [`builtin_commands`]; the front end then reports `unsupported_in_oxa`. They are not
/// reserved: a hook command, template or skill with the same name still runs first.
const OMITTED: &[&str] = &["login", "logout", "share"];

/// The built-in commands of slash-commands.md that oxa keeps (plan sec. 1), plus `/engine`
/// (replaces `/llama`) and `/mode`. `Source::Builtin`. Omitted pi commands (`/login`,
/// `/logout`, `/share`) are not listed; typing one yields the `unsupported_in_oxa` error.
pub fn builtin_commands() -> Vec<CommandSpec> {
    BUILTINS
        .iter()
        .map(|(name, description, hint)| CommandSpec {
            name: (*name).to_string(),
            description: (*description).to_string(),
            argument_hint: hint.map(str::to_string),
            source: Source::Builtin,
        })
        .collect()
}

/// All commands for autocomplete and RPC `get_commands`, in pi's order and with pi's
/// name-clash rule: built-ins, then extension commands, then prompt templates, then skills
/// (`skill:<name>`, only when `enableSkillCommands`); a later source with a taken name is
/// dropped with a diagnostic.
///
/// Diagnostics: this function takes `&SessionState` and cannot push to
/// `Resources::diagnostics`, so a dropped clash is silent here. Templates and skills carry
/// `Source::Extension(<file path>)`, since `Source` has no prompt or skill variant.
pub fn all_commands(st: &SessionState) -> Vec<CommandSpec> {
    let mut out = builtin_commands();
    let mut taken: HashSet<String> = out.iter().map(|c| c.name.clone()).collect();

    for hook in &st.hooks {
        for c in hook.commands() {
            admit(&mut out, &mut taken, c);
        }
    }
    for t in &st.resources.templates {
        admit(
            &mut out,
            &mut taken,
            CommandSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                argument_hint: t.argument_hint.clone(),
                source: Source::Extension(t.path.display().to_string()),
            },
        );
    }
    // pi agent-session.ts getCommands lists every skill; `enableSkillCommands` only decides
    // whether the interactive autocomplete offers them (interactive-mode.ts).
    {
        for s in &st.resources.skills {
            admit(
                &mut out,
                &mut taken,
                CommandSpec {
                    name: format!("skill:{}", s.name),
                    description: s.description.clone(),
                    argument_hint: None,
                    source: Source::Extension(s.path.display().to_string()),
                },
            );
        }
    }
    out
}

/// Classify and expand one input line. Templates expand with their arguments
/// (`oxa_resources` rules) BEFORE the text becomes a message; `/skill:name args` expands to
/// pi's skill invocation text; an unknown `/name` is plain prompt text (pi's behaviour).
///
/// Order: `!`/`!!` first; then `/name` against built-ins, hook commands, templates, skills,
/// and the omitted names (which yield `Builtin`, see [`OMITTED`]); anything else is `Prompt`
/// with the line unchanged. A skill whose file cannot be read passes through as text (pi).
pub fn expand_input(st: &SessionState, line: &str) -> Expanded {
    let t = line.trim_start();

    if let Some(rest) = t.strip_prefix('!') {
        let (excluded, cmd) = match rest.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let command = cmd.trim();
        if command.is_empty() {
            return Expanded::Prompt(line.to_string());
        }
        return Expanded::Bash {
            command: command.to_string(),
            excluded,
        };
    }

    let Some(body) = t.strip_prefix('/') else {
        return Expanded::Prompt(line.to_string());
    };
    let name_len = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..name_len];
    let args = body[name_len..].trim();
    if name.is_empty() {
        return Expanded::Prompt(line.to_string());
    }

    if BUILTINS.iter().any(|(n, _, _)| *n == name) {
        return Expanded::Builtin {
            name: name.to_string(),
            args: args.to_string(),
        };
    }
    if st
        .hooks
        .iter()
        .any(|h| h.commands().iter().any(|c| c.name == name))
    {
        return Expanded::Hook {
            name: name.to_string(),
            args: args.to_string(),
        };
    }
    if let Some(text) = expand_template_command(&st.resources.templates, t) {
        return Expanded::Prompt(text);
    }
    // pi agent-session.ts `_expandSkillCommand`: a typed `/skill:name` always expands;
    // `enableSkillCommands` only hides the commands from the autocomplete.
    if let Some(skill_name) = name.strip_prefix("skill:") {
        if let Some(skill) = st.resources.skills.iter().find(|s| s.name == skill_name) {
            return match expand_skill_command(skill, args) {
                Ok(text) => Expanded::Prompt(text),
                Err(_) => Expanded::Prompt(line.to_string()),
            };
        }
    }
    if OMITTED.contains(&name) {
        return Expanded::Builtin {
            name: name.to_string(),
            args: args.to_string(),
        };
    }
    Expanded::Prompt(line.to_string())
}

// ---- internal helpers -----------------------------------------------------------------------

/// Add `c` unless its name is taken by an earlier source (the earlier one wins).
fn admit(out: &mut Vec<CommandSpec>, taken: &mut HashSet<String>, c: CommandSpec) {
    if taken.insert(c.name.clone()) {
        out.push(c);
    }
}

// ---- built-in commands outside the interactive mode ---------------------------------------

/// Run a built-in slash command outside the interactive mode (print, JSON and RPC `prompt`).
/// Returns the lines it shows the user; the front end prints them (print mode: stdout) or sends
/// them as notices. pi has no built-in commands outside its TUI (it sends `/reload` to the
/// model as text); oxa runs the ones that have a meaning without a TUI:
///
/// * `/engine [status|load <model>]`, `/session`, `/trust [parent|no]` (a bare `/trust` saves
///   "trust this project": the command itself is the decision, there is no selector),
///   `/reload`, `/bug <description>` (the local bundle), `/compact [instructions]`,
///   `/name <name>`, `/thinking <level>`, `/mode <mode>`, `/model <id>`;
/// * `/mcp [...]` runs the MCP hook set's own command (its status lines arrive as
///   notifications, as pi's `/mcp` prints status outside the TUI);
/// * `/share`, `/login`, `/logout`: `unsupported_in_oxa`;
/// * the selector and editor commands (`/tree`, `/resume`, `/settings`, ...): `invalid`,
///   "available in interactive mode" (pi's wording for its interactive-only `/llama`).
///
/// A hook command, template or skill with the same name never reaches this function
/// (`expand_input` resolves those first for the omitted names).
pub fn run_builtin(
    session: &mut crate::Session,
    name: &str,
    args: &str,
    sink: &mut dyn oxa_proto::EventSink,
    ui: &mut dyn oxa_proto::Ui,
) -> Result<Vec<String>, oxa_proto::OxaError> {
    let result = run_builtin_inner(session, name, args, sink, ui);
    session.set_command_outcome(match &result {
        Ok(_) => crate::RunOutcome::Done,
        Err(error) => crate::RunOutcome::from_error(error),
    });
    result
}

fn run_builtin_inner(
    session: &mut crate::Session,
    name: &str,
    args: &str,
    sink: &mut dyn oxa_proto::EventSink,
    ui: &mut dyn oxa_proto::Ui,
) -> Result<Vec<String>, oxa_proto::OxaError> {
    use oxa_proto::{codes, Mode, OxaError, ThinkingLevel};
    let usage = |text: &str| OxaError::new(codes::USAGE, text.to_string());
    match name {
        "share" | "login" | "logout" => Err(OxaError::unsupported(&format!("/{name}"))
            .with_hint("oxa has no provider login and no upload; see PARITY.md")),
        "engine" => {
            let (sub, rest) = match args.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (args, ""),
            };
            match sub {
                "" | "status" => {
                    let st = session.inner();
                    let last = st.last_usage.map(|u| (u.cached, u.prompt));
                    Ok(engine_status_lines(
                        &st.info.base_url,
                        &st.model,
                        st.info.n_ctx,
                        last,
                        None,
                        st.stats.cache_misses,
                    ))
                }
                "load" if !rest.is_empty() => {
                    session.set_model(rest, sink)?;
                    Ok(vec![format!("Model: {rest}")])
                }
                _ => Err(usage("Usage: /engine [status | load <model>]")),
            }
        }
        "model" if !args.is_empty() => {
            session.set_model(args, sink)?;
            Ok(vec![format!("Model: {args}")])
        }
        "thinking" if !args.is_empty() => {
            let level = ThinkingLevel::parse(args)
                .ok_or_else(|| usage(&format!("Invalid thinking level: {args}")))?;
            session.set_thinking(level, sink);
            Ok(vec![format!("Thinking level: {}", level.as_str())])
        }
        "mode" if !args.is_empty() => {
            let mode = Mode::parse(args).ok_or_else(|| usage("Usage: /mode <plan|workspace|ask>"))?;
            session.set_mode(mode, sink);
            Ok(vec![format!("Mode: {}", mode.as_str())])
        }
        "name" if !args.is_empty() => {
            session.set_name(args, sink)?;
            Ok(vec![format!("Session name: {}", args.trim())])
        }
        "compact" => {
            let instructions = (!args.is_empty()).then_some(args);
            session.compact(instructions, sink, ui)?;
            Ok(Vec::new())
        }
        "session" => Ok(session_lines(session)),
        "reload" => {
            let changed = session.reload(sink, ui)?;
            Ok(vec![if changed {
                "Reloaded keybindings, extensions, skills, templates, themes and context files; the prefix changed (new epoch).".to_string()
            } else {
                "Reloaded keybindings, extensions, skills, templates, themes and context files.".to_string()
            }])
        }
        "trust" => {
            let st = session.inner();
            let (dir, trusted, what) = match args {
                "" | "yes" => (st.cwd.clone(), true, "This project is trusted"),
                "parent" => (
                    st.cwd.parent().unwrap_or(&st.cwd).to_path_buf(),
                    true,
                    "The parent directory is trusted",
                ),
                "no" => (st.cwd.clone(), false, "This project is not trusted"),
                _ => return Err(usage("Usage: /trust [parent | no]")),
            };
            let file = st.dirs.trust_file();
            oxa_perm::TrustStore::load(&file).save(&file, &dir, trusted)?;
            Ok(vec![format!(
                "{what}. Saved; it applies the next time oxa starts here."
            )])
        }
        "bug" => {
            let st = session.inner();
            let facts = vec![
                format!("session id: {}", st.sref.id),
                format!("engine: {}", st.info.base_url),
                format!("model: {}", st.model),
                "mode: headless".to_string(),
            ];
            let path = write_bug_report(&st.dirs, args, &facts)?;
            Ok(vec![format!(
                "Bug report written to {} (review it before you share it)",
                path.display()
            )])
        }
        "mcp" => {
            let owned = session
                .inner()
                .hooks
                .iter()
                .any(|h| h.commands().iter().any(|c| c.name == "mcp"));
            if !owned {
                return Ok(vec!["No MCP servers configured".to_string()]);
            }
            session.run_command("mcp", args, sink, ui)?;
            Ok(Vec::new())
        }
        _ => Err(OxaError::new(
            codes::INVALID,
            format!("/{name} is available in interactive mode"),
        )),
    }
}

/// The `/session` listing outside the TUI: file, id, name, message counts, tokens, context.
fn session_lines(session: &crate::Session) -> Vec<String> {
    let state = session.state();
    let stats = session.stats();
    let file = match &state.session_file {
        Some(p) => p.display().to_string(),
        None => "(ephemeral, not saved)".to_string(),
    };
    vec![
        format!("Session file: {file}"),
        format!("Session id: {}", state.session_id),
        format!("Name: {}", state.session_name.as_deref().unwrap_or("(not set)")),
        format!(
            "Messages: user {}, assistant {}, tool calls {}",
            stats.user_messages, stats.assistant_messages, stats.tool_calls
        ),
        format!(
            "Tokens: input {}, output {}",
            stats.prompt_tokens, stats.completion_tokens
        ),
        format!("Context: {} / {} tokens", stats.context_tokens, state.n_ctx),
    ]
}

/// The `/engine` status lines (both front ends): endpoint, model, context window, cache hit of
/// the last request (`(cached, prompt)`), queue length when known, unknown cache misses.
pub fn engine_status_lines(
    endpoint: &str,
    model: &str,
    n_ctx: u32,
    last_cache: Option<(u32, u32)>,
    queue: Option<u32>,
    unknown_misses: u32,
) -> Vec<String> {
    let endpoint = if endpoint.is_empty() {
        "(none configured)"
    } else {
        endpoint
    };
    let window = if n_ctx > 0 {
        format!("{n_ctx} tokens")
    } else {
        "unknown".to_string()
    };
    let cache = match last_cache {
        Some((cached, prompt)) => cache_text(cached, prompt),
        None => "no request yet".to_string(),
    };
    let queue = match queue {
        Some(n) => n.to_string(),
        None => "not reported".to_string(),
    };
    vec![
        format!("Endpoint: {endpoint}"),
        format!("Model: {model}"),
        format!("Context window: {window}"),
        format!("Cache hit, last request: {cache}"),
        format!("Queue: {queue}"),
        format!("Unknown cache misses: {unknown_misses}"),
    ]
}

/// `cached / prompt tokens (pct%)`.
pub fn cache_text(cached: u32, prompt: u32) -> String {
    if prompt == 0 {
        return format!("{cached} / {prompt} tokens");
    }
    let pct = cached as f64 * 100.0 / prompt as f64;
    format!("{cached} / {prompt} tokens ({pct:.0}%)")
}

/// `/bug` (both front ends): write the local, redacted report bundle under
/// `<agent_dir>/bug-reports/` (mode 0600) and return its path. The bundle holds the
/// description, the version and the caller's `facts`; the home directory is shown as `~`.
/// Nothing leaves the machine.
pub fn write_bug_report(
    dirs: &oxa_config::Dirs,
    description: &str,
    facts: &[String],
) -> Result<std::path::PathBuf, oxa_proto::OxaError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let home = dirs.home.display().to_string();
    let redact = |s: &str| {
        if home.is_empty() {
            s.to_string()
        } else {
            s.replace(&home, "~")
        }
    };
    let mut text = String::new();
    text.push_str("oxa bug report (local file; nothing was uploaded)\n");
    text.push_str(&format!("description: {}\n", redact(description)));
    text.push_str(&format!("version: {}\n", env!("CARGO_PKG_VERSION")));
    for f in facts {
        text.push_str(&redact(f));
        if !f.ends_with('\n') {
            text.push('\n');
        }
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dirs.agent_dir.join("bug-reports").join(format!("bug-{ms}.txt"));
    let io = |e: std::io::Error| oxa_proto::OxaError::io(&format!("cannot write {}", path.display()), &e);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(io)?;
    file.write_all(text.as_bytes()).map_err(io)?;
    Ok(path)
}
