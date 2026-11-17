//! D11: handlers of the built-in slash commands.
// ported from pi packages/coding-agent/src/modes/interactive/interactive-mode.ts (slash command handling) and docs/slash-commands.md
use crate::{AppAction, Block, Reopen, SelectorKind, SessionCmd, StatusModel, ViewModel};
use oxa_config::{resolve_model, Dirs, Env, Keymap, ModelRef, ModelsConfig, Settings};
use oxa_core::{context_text, messages_text, model_text, tokens_text, Row, StatusView};
use oxa_proto::{CommandSpec, Level, Mode, OxaError, PermissionMode, Source, ThinkingLevel};
use std::path::{Path, PathBuf};

/// What the dispatcher may read. It changes nothing itself: every effect is an [`AppAction`].
pub struct SlashCx<'a> {
    pub vm: &'a ViewModel,
    pub keymap: &'a Keymap,
    pub settings: &'a Settings,
    pub dirs: &'a Dirs,
    pub cwd: &'a Path,
    /// All commands of the session (`Session::commands()`): built-ins, templates, skills and
    /// extension commands, in pi's clash order.
    pub commands: &'a [CommandSpec],
    /// `true` while a run is active (some commands are refused with pi's "busy" notice).
    pub busy: bool,
    /// Path of the session file (`None` = ephemeral).
    pub session_file: Option<&'a Path>,
    pub session_id: &'a str,
    /// The text of the last assistant message (`/copy`).
    pub last_assistant_text: Option<&'a str>,
    /// The engine endpoint the session talks to (`EngineInfo::base_url`; D-INT). Empty = not
    /// known yet: `/engine` then falls back to the default endpoint of models.json.
    pub engine_url: &'a str,
    /// The models the endpoint can serve, for `/model <pattern>` and `/engine load` (D-INT).
    /// Empty = not known: the argument is sent as typed.
    pub models: &'a [String],
    /// Requests waiting on the engine, when known.
    pub queue: Option<u32>,
}

/// The built-in commands this module handles, without the slash, in pi's listing order.
/// Kept in oxa: every command of slash-commands.md that plan sec. 1 keeps, plus `engine`,
/// `mode` and `mcp`. Omitted pi commands (`login`, `logout`, `share`) are handled too: they
/// answer the fixed `unsupported_in_oxa` notice.
pub const INTERACTIVE_COMMANDS: [&str; 33] = [
    "settings",
    "model",
    "scoped-models",
    "export",
    "copy",
    "name",
    "session",
    "status",
    "hotkeys",
    "fork",
    "clone",
    "tree",
    "trust",
    "new",
    "compact",
    "resume",
    "reload",
    "debug",
    "thinking",
    "theme",
    "mcp",
    "engine",
    "mode",
    "yolo",
    "auto",
    "bug",
    "quit",
    "exit",
    "login",
    "logout",
    "share",
    "help",
    "usage",
];

/// pi commands oxa omits (verdict O). A hook command, template or skill with the same name
/// still runs first, as in `oxa_core::expand_input`.
const OMITTED: [&str; 3] = ["login", "logout", "share"];

/// #202: the usage line of `/mode`.
const MODE_USAGE: &str = "Usage: /mode [standard|auto|yolo|plan|workspace|ask] [--save]";

/// Names added after pi: a template, skill or extension command with the same name wins.
const OVERRIDABLE: [&str; 2] = ["help", "usage"];

/// Dispatch one submitted editor line.
///
/// * Not starting with `/`, or a `/name` that is not in [`INTERACTIVE_COMMANDS`]: returns
///   `None` (the app sends it as a prompt; the core expands templates, `/skill:name` and
///   extension commands, and reports unknown commands).
/// * A built-in: returns the action. Arguments follow slash-commands.md: `/model <pattern>`
///   switches directly, `/model` opens the selector; `/compact [instructions]`; `/name <text>`;
///   `/export [path]`; `/thinking [level]`; `/theme [name]`;
///   `/mode [plan|workspace|ask]`; `/resume`, `/tree`, `/fork`, `/settings`, `/scoped-models`,
///   `/mcp`, `/trust` open selectors; `/session`, `/hotkeys`, `/engine` show
///   lines; `/debug` -> `DebugDump`; `/bug` writes a local redacted bundle (no upload) and
///   reports its path; `/quit` and `/exit` -> `Quit`.
/// * A command that needs an idle session while `cx.busy` -> a warning `Notify`.
/// * Bad arguments -> an error `Notify` with pi's usage text.
///
/// Acceptance (plan D11): every command of slash-commands.md kept in sec. 1, plus `/engine`;
/// `/debug` file; `/hotkeys` reflects the keymap.
pub fn dispatch_slash(line: &str, cx: &SlashCx) -> Option<AppAction> {
    let body = line.trim_start().strip_prefix('/')?;
    let name_len = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..name_len];
    let args = body[name_len..].trim();
    if name.is_empty() {
        return None;
    }
    if !INTERACTIVE_COMMANDS.contains(&name) {
        // A template, skill or extension command goes to the core; a path (`/tmp/x`) is text.
        // A bare unknown name (`/bogus`) is a typo and is rejected here, without a model call.
        // A name followed by text (`/src explain this`) is a prompt and reaches the model,
        // so the outcome never depends on the host filesystem.
        if name.contains('/')
            || name.contains('.')
            || !args.is_empty()
            || cx.commands.iter().any(|c| c.name == name)
        {
            return None;
        }
        return Some(notify(
            Level::Error,
            format!("Unknown command /{name}. Type /help for the list of commands; add text after a name to send the line as a prompt."),
        ));
    }

    if OVERRIDABLE.contains(&name)
        && cx
            .commands
            .iter()
            .any(|c| c.name == name && c.source != Source::Builtin)
    {
        return None;
    }

    if OMITTED.contains(&name) {
        if cx
            .commands
            .iter()
            .any(|c| c.name == name && c.source != Source::Builtin)
        {
            return None;
        }
        let e = OxaError::unsupported(&format!("/{name}"))
            .with_hint("oxa has no provider login and no upload; see PARITY.md");
        return Some(notify(Level::Error, format!("{}: {}", e.code, e)));
    }

    let action = match name {
        "quit" | "exit" => AppAction::Quit,
        "settings" => AppAction::Open(SelectorKind::Settings),
        "scoped-models" => AppAction::Open(SelectorKind::ScopedModels),
        "model" => {
            if args.is_empty() {
                AppAction::Open(SelectorKind::Model)
            } else {
                idle_or(cx, name, || switch_model(cx, args))
            }
        }
        "thinking" => {
            if args.is_empty() {
                AppAction::Open(SelectorKind::Thinking)
            } else {
                match ThinkingLevel::parse(args) {
                    Some(level) => AppAction::Session(SessionCmd::SetThinking(level)),
                    None => usage("Usage: /thinking [off|minimal|low|medium|high|xhigh|max]"),
                }
            }
        }
        // #202: no argument opens the picker; `--save` also makes the mode the default.
        "mode" => {
            let mut words: Vec<&str> = args.split_whitespace().collect();
            let save = words.contains(&"--save");
            words.retain(|w| *w != "--save");
            match words.as_slice() {
                [] if !save => AppAction::Open(SelectorKind::Mode),
                [name] => match (PermissionMode::parse(name), Mode::parse(name)) {
                    (Some(mode), _) => AppAction::SetPermissionMode { mode, save },
                    // The table modes `workspace` and `ask` prompt: they come with `standard`.
                    (None, Some(mode)) if !save => AppAction::Many(vec![
                        AppAction::SetPermissionMode {
                            mode: PermissionMode::Standard,
                            save: false,
                        },
                        AppAction::Session(SessionCmd::SetMode(mode)),
                    ]),
                    _ => usage(MODE_USAGE),
                },
                _ => usage(MODE_USAGE),
            }
        }
        "auto" => match args.trim() {
            "" => AppAction::SetPermissionMode {
                mode: PermissionMode::Auto,
                save: false,
            },
            _ => usage("Usage: /auto"),
        },
        "theme" => {
            if args.is_empty() {
                AppAction::Open(SelectorKind::Theme)
            } else {
                AppAction::SetTheme {
                    name: args.to_string(),
                    persist: true,
                }
            }
        }
        "export" => AppAction::Export {
            path: (!args.is_empty()).then(|| PathBuf::from(args)),
        },
        "copy" => match cx.last_assistant_text {
            Some(text) => AppAction::Copy(text.to_string()),
            None => notify(Level::Warning, "No assistant message to copy yet."),
        },
        "name" => {
            if args.is_empty() {
                let text = match &cx.vm.status.session_name {
                    Some(n) => format!("Session name: {n}"),
                    None => "Session name: (not set)".to_string(),
                };
                AppAction::ShowLines(vec![text])
            } else {
                AppAction::Session(SessionCmd::SetName(args.to_string()))
            }
        }
        "session" | "usage" => AppAction::ShowLines(session_info_lines(cx)),
        // One read-only status view, shared with the headless `/status` and `oxa status`: rows
        // this view cannot source stay unavailable instead of showing a zero.
        "status" => AppAction::ShowLines(status_lines(cx)),
        "help" => AppAction::ShowLines(help_lines(cx)),
        "hotkeys" => AppAction::ShowLines(hotkeys_lines(cx.keymap)),
        "fork" => AppAction::Open(SelectorKind::ForkPoint),
        // O-182: `/yolo` toggles, `/yolo on` / `/yolo off` set.
        "yolo" => match args.trim() {
            "" => AppAction::Yolo(None),
            "on" => AppAction::Yolo(Some(true)),
            "off" => AppAction::Yolo(Some(false)),
            _ => AppAction::Notify {
                level: Level::Warning,
                text: "Usage: /yolo [on|off]".to_string(),
            },
        },
        "clone" => idle_or(cx, name, || AppAction::Session(SessionCmd::CloneSession)),
        "tree" => AppAction::Open(SelectorKind::Tree),
        "trust" => AppAction::Open(SelectorKind::Trust),
        "new" => idle_or(cx, name, || AppAction::Session(SessionCmd::Reopen(Reopen::New))),
        "compact" => idle_or(cx, name, || {
            AppAction::Session(SessionCmd::Compact {
                instructions: (!args.is_empty()).then(|| args.to_string()),
            })
        }),
        "resume" => AppAction::Open(SelectorKind::Sessions),
        "reload" => idle_or(cx, name, || {
            AppAction::Many(vec![
                AppAction::Notify {
                    level: Level::Info,
                    text: "Reloading keybindings, extensions, skills, templates, themes and context files. \
                           A changed prefix opens a new epoch and prints its re-prefill cost."
                        .to_string(),
                },
                AppAction::Session(SessionCmd::Reload),
            ])
        }),
        "debug" => AppAction::DebugDump,
        "mcp" => AppAction::Open(SelectorKind::Mcp),
        "engine" => engine_action(cx, args),
        "bug" => AppAction::BugReport {
            description: args.to_string(),
        },
        _ => return None,
    };
    Some(action)
}

/// The `/help` listing: every command of the session with its description, then the keys hint.
pub fn help_lines(cx: &SlashCx) -> Vec<String> {
    let mut out = vec!["Commands:".to_string()];
    let mut names: Vec<(String, String)> = INTERACTIVE_COMMANDS
        .iter()
        .filter(|n| !OMITTED.contains(n) && **n != "exit")
        .map(|n| {
            let d = cx.commands.iter().find(|c| c.name == *n);
            (
                n.to_string(),
                d.map(|c| c.description.clone()).unwrap_or_default(),
            )
        })
        .collect();
    for c in cx
        .commands
        .iter()
        .filter(|c| !INTERACTIVE_COMMANDS.contains(&c.name.as_str()))
    {
        names.push((c.name.clone(), c.description.clone()));
    }
    for (n, d) in names {
        out.push(format!("  /{n:<18} {d}"));
    }
    out.push(String::new());
    out.push("Keys: /hotkeys lists every key binding.".to_string());
    out
}

/// The `/hotkeys` listing built from the live keymap (user overrides included): grouped as in
/// keybindings.md, one `keys  description` line per action; unbound actions show `(unbound)`.
pub fn hotkeys_lines(keymap: &Keymap) -> Vec<String> {
    let mut out = Vec::new();
    let mut group: Option<&str> = None;
    for def in Keymap::actions() {
        let g = group_of(def.id);
        if group != Some(g) {
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push(g.to_string());
            group = Some(g);
        }
        let keys = keymap.keys_for(def.id);
        let shown = if keys.is_empty() {
            "(unbound)".to_string()
        } else {
            keys.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ")
        };
        out.push(format!("  {shown:<30} {}", def.description));
    }
    out
}

/// The `/session` listing: file, id, name, message counts, token totals, context use.
pub fn session_info_lines(cx: &SlashCx) -> Vec<String> {
    let st: &StatusModel = &cx.vm.status;
    let (mut users, mut answers, mut tools) = (0u32, 0u32, 0u32);
    for block in &cx.vm.blocks {
        match block {
            Block::User { .. } => users += 1,
            Block::Assistant { streaming: false, .. } => answers += 1,
            Block::Tool(_) => tools += 1,
            _ => {}
        }
    }
    let file = match cx.session_file {
        Some(p) => p.display().to_string(),
        None => "(ephemeral, not saved)".to_string(),
    };
    let name = st.session_name.as_deref().unwrap_or("(not set)");
    let mut out = vec![
        format!("Session file: {file}"),
        format!("Session id: {}", cx.session_id),
        format!("Name: {name}"),
        format!("Messages: user {users}, assistant {answers}, tool calls {tools}"),
        format!("Tokens: input {}, output {}", st.total_in, st.total_out),
    ];
    if st.n_ctx > 0 {
        out.push(format!("Context: {} / {} tokens", st.context_tokens, st.n_ctx));
    } else {
        out.push(format!("Context: {} tokens (window unknown)", st.context_tokens));
    }
    if let (Some(cached), Some(prompt)) = (st.last_cached, st.last_prompt) {
        out.push(format!("Cache, last request: {}", cache_text(cached, prompt)));
    }
    out
}

/// The `/status` view: the session facts of `/session`, the live turn, the queues and the
/// engine the session talks to, in the one layout every status surface shares. Facts this view
/// does not receive (the job registry, children, the goal, lead facts, a cost meter) stay
/// unavailable with their fixed reason.
pub fn status_lines(cx: &SlashCx) -> Vec<String> {
    let st: &StatusModel = &cx.vm.status;
    let (mut users, mut answers, mut tools) = (0u32, 0u32, 0u32);
    for block in &cx.vm.blocks {
        match block {
            Block::User { .. } => users += 1,
            Block::Assistant { streaming: false, .. } => answers += 1,
            Block::Tool(_) => tools += 1,
            _ => {}
        }
    }
    let file = match cx.session_file {
        Some(p) => p.display().to_string(),
        None => "(ephemeral, not saved)".to_string(),
    };
    let name = st.session_name.clone().unwrap_or_else(|| "(not set)".to_string());
    let turn = match crate::chrome::activity_text(st) {
        Some(text) => text,
        None if cx.busy => "running".to_string(),
        None => "idle".to_string(),
    };
    let model = model_text(&st.model, st.model_remote, st.model_provider);
    let mut view = StatusView::new(cx.session_id);
    view = view.set(Row::SessionFile, file);
    view = view.set(Row::SessionId, cx.session_id);
    view = view.set(Row::SessionName, name);
    view = view.set(Row::Messages, messages_text(users, answers, tools));
    view = view.set(Row::Tokens, tokens_text(st.total_in, st.total_out));
    view = view.set(Row::Context, context_text(st.context_tokens, st.n_ctx));
    view = view.set(Row::Turn, turn);
    view = view.set(Row::Model, model);
    view = view.set(Row::Steering, st.steering.len().to_string());
    view = view.set(Row::FollowUp, st.follow_up.len().to_string());
    if let (Some(cached), Some(prompt)) = (st.last_cached, st.last_prompt) {
        view = view.set(Row::LastCache, cache_text(cached, prompt));
    }
    if !cx.engine_url.is_empty() {
        view = view.set(Row::EngineEndpoint, cx.engine_url);
        view = view.set(Row::EngineModel, st.model.clone());
    }
    if let Some(queue) = cx.queue {
        // `EngineInfo::queue`: requests waiting, 0 when the engine does not report a queue.
        view = view.set(Row::EngineQueue, format!("{queue} waiting"));
    }
    view.lines()
}

/// The `/engine` listing from the status model (endpoint, model, context window, cache hit of
/// the last request, queue length when known, unknown-miss count).
pub fn engine_lines(status: &StatusModel, base_url: &str, queue: Option<u32>) -> Vec<String> {
    let last = status.last_cached.zip(status.last_prompt);
    let mut lines = oxa_core::engine_status_lines(
        base_url,
        &status.model,
        status.n_ctx,
        last,
        queue,
        status.unknown_misses,
    );
    lines.push(format!("Tool call repairs: {}", status.tool_repairs));
    lines
}

/// The content of the `/debug` file: the last rendered frame (escape sequences made visible),
/// terminal size and capabilities, and the view model's block summary. No message text beyond
/// what is on screen; no environment variables.
pub fn debug_dump(last_frame: &[String], cols: u16, rows: u16, vm: &ViewModel) -> String {
    let mut out = String::new();
    out.push_str("oxa debug dump\n");
    out.push_str(&format!("terminal: {cols}x{rows}\n"));
    out.push_str(&format!("activity: {:?}\n", vm.status.activity));
    out.push_str(&format!("model: {}\n", vm.status.model));
    out.push_str(&format!("blocks: {}\n", block_summary(vm)));
    out.push_str("--- last frame ---\n");
    for line in last_frame {
        out.push_str(&visible(line));
        out.push('\n');
    }
    out
}

// ---- internal helpers -----------------------------------------------------------------------

fn notify(level: Level, text: impl Into<String>) -> AppAction {
    AppAction::Notify {
        level,
        text: text.into(),
    }
}

/// pi's usage line as an error notice.
fn usage(text: &str) -> AppAction {
    notify(Level::Error, text)
}

/// Run `f` when the session is idle; otherwise the warning notice of pi's "busy" rule.
fn idle_or(cx: &SlashCx, name: &str, f: impl FnOnce() -> AppAction) -> AppAction {
    if cx.busy {
        notify(
            Level::Warning,
            format!("/{name} needs an idle session: wait for the current run to finish."),
        )
    } else {
        f()
    }
}

/// `/engine` with no argument or `status` shows the endpoint; `load <model>` switches the
/// model on the engine (a model switch, so a new epoch and the idle rule apply).
fn engine_action(cx: &SlashCx, args: &str) -> AppAction {
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((s, r)) => (s, r.trim()),
        None => (args, ""),
    };
    match sub {
        "" | "status" => {
            let endpoint = if cx.engine_url.is_empty() {
                default_endpoint(cx.dirs)
            } else {
                cx.engine_url.to_string()
            };
            AppAction::ShowLines(engine_lines(&cx.vm.status, &endpoint, cx.queue))
        }
        "load" if !rest.is_empty() => idle_or(cx, "engine load", || switch_model(cx, rest)),
        _ => usage("Usage: /engine [status | load <model>]"),
    }
}

/// `/model <pattern>` and `/engine load <pattern>`: the pattern is resolved against the
/// engine's models as `--model` is (exact id, substring, glob; a `:level` suffix also sets the
/// thinking level). Without a model list the text is sent as typed.
fn switch_model(cx: &SlashCx, pattern: &str) -> AppAction {
    if cx.models.is_empty() {
        return AppAction::Session(SessionCmd::SetModel(pattern.to_string()));
    }
    let available: Vec<ModelRef> = cx
        .models
        .iter()
        .map(|id| ModelRef {
            endpoint: String::new(),
            model: id.clone(),
            thinking: None,
        })
        .collect();
    match resolve_model(pattern, None, &available) {
        Ok(picked) => {
            let switch = AppAction::Session(SessionCmd::SetModel(picked.model));
            match picked.thinking {
                Some(level) => {
                    AppAction::Many(vec![switch, AppAction::Session(SessionCmd::SetThinking(level))])
                }
                None => switch,
            }
        }
        Err(e) => notify(Level::Error, e.to_string()),
    }
}

/// The default engine endpoint of models.json (or the built-in default). Only the files under
/// `dirs` are read; the process environment is not (see the task notes on `OXA_ENGINE_URL`).
fn default_endpoint(dirs: &Dirs) -> String {
    let (cfg, _) = ModelsConfig::load(dirs, &Env::default());
    cfg.endpoints
        .iter()
        .find(|e| e.name == cfg.default)
        .map(|e| e.base_url.clone())
        .unwrap_or_default()
}

/// The keymap group of an action id: `tui.editor.cursorUp` -> `tui.editor`, `app.mode.cycle` ->
/// `app.mode`.
fn group_of(id: &str) -> &str {
    id.rsplit_once('.').map(|(group, _)| group).unwrap_or(id)
}

fn cache_text(cached: u32, prompt: u32) -> String {
    oxa_core::cache_text(cached, prompt)
}

fn block_summary(vm: &ViewModel) -> String {
    let (mut user, mut assistant, mut tool, mut other) = (0u32, 0u32, 0u32, 0u32);
    for block in &vm.blocks {
        match block {
            Block::User { .. } => user += 1,
            Block::Assistant { .. } => assistant += 1,
            Block::Tool(_) => tool += 1,
            _ => other += 1,
        }
    }
    format!(
        "{} (user {user}, assistant {assistant}, tool {tool}, other {other})",
        user + assistant + tool + other
    )
}

/// Escape sequences and control characters made visible, so the dump is readable as text.
fn visible(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\u{1b}' => out.push_str("\\e"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
