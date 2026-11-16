//! D13: mode choice and wiring.
// ported from pi packages/coding-agent/src/main.ts
use crate::hooks::{build_hooks, HookInputs, HookSets};
use crate::{parse_args, run_config, run_mcp, run_pkg, usage_text, Args, ModeArg, Subcommand};
use oxa_config::{
    dirs_from_env, resolve_model, Dirs, Endpoint, Env, Keymap, ModelRef, ModelsConfig, Settings,
};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_export::{default_output_name, export_html, write_export, ExportOpts, ExportTheme};
use oxa_interactive::{
    ask_trust_interactive, image_part, run as run_interactive, ClipImage, InteractiveOpts, Reopen, ReopenFn,
    TrustChoice,
};
use oxa_perm::{
    gated_resources, resolve_trust, DefaultTrust, ModePolicy, TrustInputs, TrustOutcome, TrustStore,
};
use oxa_proto::{codes, Engine, Level, Mode, OxaError, Part, Sandbox, ThinkingLevel, Value};
use oxa_resources::{discover, DiscoverOpts, Resources};
use oxa_rpc::{run_json_outcome, run_print_outcome, serve_outcome, Out};
use oxa_sandbox::{Bwrap, NoSandbox};
use oxa_session::{decode_entry, decode_header, FsStore, MemStore, SessionStore};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The process's standard streams and terminal facts, injectable for tests.
pub struct Stdio {
    pub stdin: Box<dyn Read + Send>,
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    /// The process working directory.
    pub cwd: std::path::PathBuf,
}

impl Stdio {
    /// The real streams (`libc::isatty` on fds 0 and 1, `std::env::current_dir`).
    pub fn real() -> Stdio {
        use std::io::IsTerminal;
        Stdio {
            stdin: Box::new(std::io::stdin()),
            stdout: Box::new(std::io::stdout()),
            stderr: Box::new(std::io::stderr()),
            stdin_is_tty: std::io::stdin().is_terminal(),
            stdout_is_tty: std::io::stdout().is_terminal(),
            // An empty path means the working directory is gone; `run` reports it.
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

/// cli.md's tty rules (see the crate docs). Pure.
pub fn choose_mode(a: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> RunMode {
    match a.mode {
        Some(ModeArg::Rpc) => RunMode::Rpc,
        Some(ModeArg::Json) => RunMode::Json,
        _ if a.print || !stdin_is_tty || !stdout_is_tty => RunMode::Print,
        _ => RunMode::Interactive,
    }
}

pub(crate) const USAGE_HINT: &str = "Run `oxa --help` for the list of options.";

fn headless_argument_error(argv: &[String], stdin_tty: bool, stdout_tty: bool) -> bool {
    let standalone = parse_args(&argv[..argv.len().min(1)], &[])
        .ok()
        .is_some_and(|args| args.subcommand.is_some());
    let flags: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .take_while(|arg| *arg != "--")
        .collect();
    let single_action = flags.iter().any(|arg| {
        ["--export", "--list-models", "--help", "-h", "--version", "-v"]
            .iter()
            .any(|flag| *arg == *flag || arg.starts_with(&format!("{flag}=")))
    });
    if standalone || single_action {
        return false;
    }
    !stdin_tty
        || !stdout_tty
        || flags
            .iter()
            .any(|arg| matches!(*arg, "-p" | "--print" | "--mode=rpc" | "--mode=json"))
        || flags
            .windows(2)
            .any(|pair| pair[0] == "--mode" && matches!(pair[1], "rpc" | "json"))
}

/// The whole program. Returns the exit code; never panics on user error.
///
/// Order: parse args (two passes for extension flags) -> `--help` / `--version` ->
/// subcommands (`mcp`, packages, `config`) -> `--export` -> resolve `Dirs`, trust, settings,
/// resources -> build hook sets (extensions, exec hooks, MCP unless `--no-mcp`, codemode,
/// subagent when enabled) -> engine (`HttpEngine`; down = one-line error, exit 2) ->
/// `--list-models` -> sandbox (`Bwrap::detect`, else `NoSandbox` + notice) -> `Session::open`
/// per the session flags -> the chosen mode. Piped stdin text is prepended to the first
/// prompt; `@file` text is included as pi does and images become `Part::Image`.
pub fn run(argv: &[String], env: &Env, io: Stdio) -> i32 {
    let Stdio {
        mut stdin,
        mut stdout,
        mut stderr,
        stdin_is_tty,
        stdout_is_tty,
        cwd,
    } = io;

    let a = match parse_args(argv, &[]) {
        Ok(a) => a,
        Err(e) => {
            let headless = headless_argument_error(argv, stdin_is_tty, stdout_is_tty);
            return if headless {
                finish_outcome(Err(e), &mut *stderr)
            } else {
                report(&mut *stderr, &e)
            };
        }
    };
    if a.help {
        let flags = help_extension_flags(&a, env, &cwd, &mut *stderr);
        let _ = stdout.write_all(usage_text(&flags).as_bytes());
        return 0;
    }
    if a.version {
        let _ = writeln!(stdout, "oxa {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    let headless = a.subcommand.is_none()
        && a.export.is_none()
        && a.list_models.is_none()
        && choose_mode(&a, stdin_is_tty, stdout_is_tty) != RunMode::Interactive;
    let report = |err: &mut dyn Write, error: &OxaError| {
        if headless {
            finish_outcome(Err(error.clone()), err)
        } else {
            report(err, error)
        }
    };
    // A long flag oxa does not know may belong to an extension (`registerFlag`): the session
    // paths resolve it once the extensions are loaded (`build_hooks`). The paths that load no
    // extension (subcommands, `--export`, `--list-models`) refuse it here.
    let loads_extensions = a.subcommand.is_none() && a.export.is_none() && a.list_models.is_none();
    if let Some((name, _)) = a.unknown.first().filter(|_| !loads_extensions) {
        let e = OxaError::new(codes::USAGE, format!("Unknown option: --{name}")).with_hint(USAGE_HINT);
        return report(&mut *stderr, &e);
    }
    if cwd.as_os_str().is_empty() {
        let e = OxaError::new(codes::NOT_FOUND, "The current working directory no longer exists")
            .with_hint("change to an existing directory and try again");
        return report(&mut *stderr, &e);
    }

    crate::netguard::install(a.offline, env, a.session_dir.as_deref(), &cwd);

    if let Some(sub) = a.subcommand.clone() {
        let dirs = match dirs_from_env(env, None) {
            Ok(d) => d,
            Err(e) => return report(&mut *stderr, &e),
        };
        let result = match sub {
            Subcommand::Work(words) => {
                return crate::work_cmd::run(&words, &cwd, env, &dirs, &mut *stdout, &mut *stderr)
            }
            Subcommand::Mcp(words) => run_mcp(&words, &dirs, &cwd, &mut *stdout, &mut *stderr),
            Subcommand::Pkg(verb, words) => run_pkg(&verb, &words, &dirs, &cwd, &mut *stdout, &mut *stderr),
            Subcommand::Config(words) => run_config(&words, &dirs, &cwd, &mut *stdout),
            Subcommand::Identity(words) => crate::run_identity(&words, &dirs, env, &mut *stdout),
            Subcommand::ReportIssue(words) => crate::run_report_issue(&words, &dirs, &cwd, env, &mut *stdout),
            Subcommand::Wipe(words) => crate::run_wipe(
                &words,
                &dirs,
                &cwd,
                env,
                crate::WipeIo {
                    stdin: &mut *stdin,
                    stdin_is_tty,
                    out: &mut *stdout,
                    err: &mut *stderr,
                },
            ),
        };
        return finish(result, &mut *stderr);
    }

    if let Some((input, output)) = &a.export {
        return export_session(
            input,
            output.as_deref(),
            Path::new(env.get("HOME").unwrap_or("")),
            &cwd,
            &mut *stdout,
            &mut *stderr,
        );
    }

    let dirs = match dirs_from_env(env, a.session_dir.as_deref()) {
        Ok(d) => d,
        Err(e) => return report(&mut *stderr, &e),
    };

    if let Some(search) = &a.list_models {
        let (models, diags) = ModelsConfig::load(&dirs, env);
        write_diagnostics(
            &mut *stderr,
            diags
                .iter()
                .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
        );
        let endpoint = match default_endpoint(&models) {
            Ok(ep) => with_api_key(ep, &a),
            Err(e) => return report(&mut *stderr, &e),
        };
        let engine = match engine_for(&endpoint) {
            Ok(e) => e,
            Err(e) => return report(&mut *stderr, &e),
        };
        let result = list_models(
            engine.as_ref(),
            &endpoint.name,
            search.as_deref(),
            &mut *stdout,
            &mut *stderr,
        );
        return finish(result, &mut *stderr);
    }

    let mode = choose_mode(&a, stdin_is_tty, stdout_is_tty);
    if (a.max_time.is_some() || a.max_tokens.is_some()) && mode == RunMode::Interactive {
        let flag = if a.max_tokens.is_some() {
            "--max-tokens"
        } else {
            "--max-time"
        };
        return report(
            &mut *stderr,
            &OxaError::new(codes::USAGE, format!("{flag} requires print, JSON or RPC mode")),
        );
    }
    if mode == RunMode::Rpc && !a.files.is_empty() {
        let e = OxaError::new(codes::USAGE, "@file arguments are not supported in RPC mode")
            .with_hint(USAGE_HINT);
        return report(&mut *stderr, &e);
    }
    // Item 3 (sec-prod): a session in the home directory or above it needs the user's say-so.
    let home_workspace = match crate::home_cwd::check(mode, &cwd, env, &dirs, &mut *stdin, &mut *stdout) {
        Ok(w) => w,
        Err(e) => return report(&mut *stderr, &e),
    };

    // CLI-019: pi's question when `--session <id>` names a session of another directory
    // (main.ts `promptConfirm`): the notice and the question on stdout, one answer line from
    // stdin (read byte by byte, so piped prompt text after it stays unread).
    let mut a = a;
    let prepared = {
        let mut confirm = |notice: &str, question: &str| -> bool {
            if mode == RunMode::Rpc {
                return false;
            }
            let _ = writeln!(stdout, "{notice}");
            let _ = write!(stdout, "{question} [y/N] ");
            let _ = stdout.flush();
            let answer = read_line(&mut *stdin);
            let answer = answer.trim().to_lowercase();
            answer == "y" || answer == "yes"
        };
        prepare(
            &mut a,
            env,
            &dirs,
            &cwd,
            mode,
            home_workspace,
            stdin_is_tty,
            stdout_is_tty,
            &mut confirm,
        )
    };
    let prep = match prepared {
        Ok(p) => p,
        Err(e) if e.code == FORK_DECLINED => {
            let _ = writeln!(stdout, "Aborted.");
            return if headless {
                finish_outcome(Ok(oxa_core::RunOutcome::Interrupted), &mut *stderr)
            } else {
                0
            };
        }
        Err(e) => return report(&mut *stderr, &e),
    };
    // Second pass (pi): the long flags the first pass did not know are parsed again with the
    // flags the loaded extensions registered; one that nobody registered is a usage error.
    let a = if a.unknown.is_empty() {
        a
    } else {
        let again = match parse_args(argv, &prep.built.hook_sets.flags) {
            Ok(again) => again,
            Err(e) => return report(&mut *stderr, &e),
        };
        if let Some((name, _)) = again.unknown.first() {
            let e = OxaError::new(codes::USAGE, format!("Unknown option: --{name}")).with_hint(USAGE_HINT);
            return report(&mut *stderr, &e);
        }
        let again = resolve_continue_word(&again, &prep.built.dirs).unwrap_or(again);
        for (name, value) in &again.extension_flags {
            for h in &prep.built.hook_sets.hooks {
                h.set_flag(name, value);
            }
        }
        again
    };

    match mode {
        RunMode::Interactive => {
            let dirs = prep.built.dirs.clone();
            run_tui(&a, env, &dirs, &cwd, prep, &mut *stderr)
        }
        RunMode::Rpc => {
            let dirs = prep.built.dirs.clone();
            run_rpc(prep, a.max_time, a.max_tokens, dirs, stdin, stdout, &mut *stderr)
        }
        RunMode::Print | RunMode::Json => {
            let stdin_text = match read_piped_stdin(&mut *stdin, stdin_is_tty) {
                Ok(t) => t,
                Err(e) => return report(&mut *stderr, &e),
            };
            let prompts = match build_prompts(&a, &dirs, &cwd, stdin_text) {
                Ok(p) => p,
                Err(e) => return report(&mut *stderr, &e),
            };
            if prompts.is_empty() {
                let e = OxaError::new(codes::USAGE, "no prompt: give a message, an @file or piped stdin")
                    .with_hint(USAGE_HINT);
                return report(&mut *stderr, &e);
            }
            run_headless(
                mode,
                a.max_time,
                a.max_tokens,
                prep,
                prompts,
                &mut *stdout,
                &mut *stderr,
            )
        }
    }
}

/// Everything decided before a session is opened.
struct Prep {
    built: Built,
    open: Open,
    session_id: Option<String>,
    notices: Vec<(Level, String)>,
    /// The gated project resources (for `/trust`).
    gated: Vec<String>,
    /// CLI-024: `-n/--name`, trimmed and non-empty; written as a `session_info` entry once
    /// the session is open.
    name: Option<String>,
}

/// Startup warnings after opening the session, so restored/selected model limits are final.
/// No extra engine requests: context tokens are a cheap estimate (four UTF-8 bytes per token).
fn startup_notices(built: &Built, session: &Session) -> Vec<(Level, String)> {
    const CONTEXT_WARNING_PERCENT: u64 = 5;
    let mut notices: Vec<_> = built
        .engine
        .handshake_warning()
        .map(|w| (Level::Warning, w))
        .into_iter()
        .collect();
    let state = session.state();
    let mut total = 0_u64;
    let mut largest = None;
    for file in &session.inner().resources.context_files {
        let tokens = file.content.len().div_ceil(4) as u64;
        total = total.saturating_add(tokens);
        if largest.is_none_or(|(_, size)| tokens > size) {
            largest = Some((file.path.as_path(), tokens));
        }
    }
    let window = u64::from(state.n_ctx);
    if window > 0 && total.saturating_mul(100) > window * CONTEXT_WARNING_PERCENT {
        if let Some((path, tokens)) = largest {
            let shown = if let Ok(rel) = path.strip_prefix(&built.cwd) {
                rel.display().to_string()
            } else if let Ok(rel) = path.strip_prefix(&built.dirs.home) {
                format!("~/{}", rel.display())
            } else {
                path.display().to_string()
            };
            notices.push((Level::Warning, format!(
                "context files use ~{total} tokens ({:.1}% of {}'s {window}-token window); largest: {shown} (~{tokens} tokens)",
                total as f64 * 100.0 / window as f64, state.model,
            )));
        }
    }
    notices
}

/// `Session::open`, then pi's `--name` (`appendSessionInfo`).
fn open_named(
    built: &Built,
    session_id: Option<String>,
    open: Open,
    name: Option<&str>,
) -> Result<Session, OxaError> {
    let mut session = Session::open(built.deps(session_id), open)?;
    session.set_output_schema(built.output_schema.clone());
    if let Some(name) = name {
        session.set_name(name, &mut oxa_proto::VecSink::default())?;
    }
    Ok(session)
}

/// The parts of a session that do not depend on the open mode. Cloned into the interactive
/// `reopen` closure, which builds sessions for `/new`, `/resume`, fork and import.
#[derive(Clone)]
struct Built {
    output_schema: Option<oxa_core::output_schema::OutputSchema>,
    engine: Arc<dyn Engine>,
    store: Arc<dyn SessionStore>,
    sandbox: Arc<dyn Sandbox>,
    settings: Settings,
    dirs: Dirs,
    cwd: PathBuf,
    resources: Resources,
    selection: ToolSelection,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    mode: Option<Mode>,
    /// A supervised subagent keeps SIGTERM as immediate supervisor cancellation.
    supervised_child: bool,
    /// RPC hosts opt into owner questions; unattended hosts need not handle UI requests.
    ask_owner_present: bool,
    /// The hook sets of the process (E-INT 4), shared by every session it opens.
    hook_sets: HookSets,
    /// O-182: the YOLO switch (`--yolo`, `/yolo`), shared by the policy of every session.
    yolo: oxa_proto::YoloSwitch,
    /// #202: the sandbox of the bash tool and its background jobs: the session's sandbox,
    /// bypassed while the switch is at yolo (full access).
    bash_sandbox: Arc<oxa_sandbox::FullAccess>,
}

/// #597: `guard` plus the session's scratch directory (`/tmp` in its sandbox), which the file
/// tools read without asking.
fn session_guard(guard: oxa_perm::PathGuard, sandbox: &dyn Sandbox) -> oxa_perm::PathGuard {
    match sandbox.scratch_dir() {
        Some(dir) => guard.with_scratch(dir),
        None => guard,
    }
}

impl Built {
    /// Fresh `Deps` (the policy and built-in tools are not `Clone`).
    fn deps(&self, session_id: Option<String>) -> Deps {
        // #457: the session's own sandbox (its scratch directory at /tmp), dropped with it.
        let sandbox = self.sandbox.for_session().unwrap_or_else(|| self.sandbox.clone());
        // Issue #127 (b): one background-job registry per session, sharing the session's
        // sandbox. The session kills its jobs on close/shutdown.
        let jobs = Arc::new(oxa_tools::JobRegistry::new());
        let bash_sandbox: Arc<dyn Sandbox> = Arc::new(self.bash_sandbox.with_inner(sandbox.clone()));
        jobs.set_sandbox(bash_sandbox.clone());
        // pi's `shellPath` and `shellCommandPrefix` (SET-055, SET-056).
        let mut tools = oxa_tools::builtin_tools_with_bash_sandbox(
            oxa_tools::ShellConfig {
                shell_path: self.settings.shell_path(&self.dirs.home),
                command_prefix: self.settings.shell_command_prefix(),
            },
            jobs.clone(),
            bash_sandbox,
        );
        // Owner questions belong to lead sessions (including unattended leads). Keep worker
        // declarations and their small frozen prefixes unchanged.
        if !self.supervised_child {
            tools.push(Arc::new(
                oxa_tools::AskTool::new(self.dirs.agent_dir.join("asks.jsonl"))
                    .with_owner_present(self.ask_owner_present),
            ));
        }
        Deps {
            engine: self.engine.clone(),
            store: self.store.clone(),
            tools,
            // Extensions (E1/E2), exec hooks (E5), MCP (E7/E9, F6), the built-in subagent (E11)
            // and codemode (E6); see `hooks.rs`.
            hooks: self.hook_sets.hooks.clone(),
            // B3: the file tools keep the deny-list and protected paths in standard/auto.
            // #202/#314: only user-selected yolo outside plan mode bypasses that floor.
            policy: Box::new(
                ModePolicy::new()
                    .with_guard(session_guard(
                        oxa_perm::PathGuard::with_real_home(
                            &self.dirs.home,
                            oxa_sandbox::account_home().as_deref(),
                            &self.dirs.agent_dir,
                            &self.dirs.tmp_dir,
                        ),
                        sandbox.as_ref(),
                    ))
                    .with_yolo(self.yolo.clone()),
            ),
            // File names follow bash's current view, without giving hooks/extensions the
            // bash-only execution bypass. The shared switch also handles live /yolo changes.
            sandbox: self.bash_sandbox.file_view(sandbox),
            settings: self.settings.clone(),
            dirs: self.dirs.clone(),
            cwd: self.cwd.clone(),
            selection: self.selection.clone(),
            resources: self.resources.clone(),
            model: self.model.clone(),
            thinking: self.thinking,
            mode: self.mode,
            session_id,
            jobs,
        }
    }
}

/// Settings, trust, resources, engine, sandbox and the open mode. Notices are printed by the
/// caller once the session is open, so a failed start prints only its one error line.
fn prepare(
    a: &mut Args,
    env: &Env,
    dirs: &Dirs,
    cwd: &Path,
    mode: RunMode,
    home_workspace: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    confirm: &mut dyn FnMut(&str, &str) -> bool,
) -> Result<Prep, OxaError> {
    let output_schema = a
        .output_schema
        .as_ref()
        .map(|file| {
            let path = cwd.join(file);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| OxaError::io(&format!("read output schema {}", path.display()), &e))?;
            let value = serde_json::from_str(&text)
                .map_err(|e| OxaError::new(codes::USAGE, format!("Invalid output schema JSON: {e}")))?;
            oxa_core::output_schema::OutputSchema::new(value)
        })
        .transpose()?;
    let mut notices: Vec<(Level, String)> = Vec::new();
    let (models, diags) = ModelsConfig::load(dirs, env);
    notices.extend(
        diags
            .into_iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    let endpoint = with_api_key(default_endpoint(&models)?, a);
    // E11: a subagent child sends its requests under the parent's child hint (`child.rs`).
    let child = crate::child::ChildIdentity::from_env(env, &mut notices);
    notices.extend(crate::netguard::endpoint_notice(&endpoint.base_url));

    let gated = gated_resources(cwd);
    let trusted = project_trusted(
        a,
        env,
        dirs,
        cwd,
        &gated,
        mode == RunMode::Interactive && stdin_is_tty && stdout_is_tty,
        &mut notices,
    )?;
    let mut settings = child.apply(Settings::load(dirs, cwd, trusted));
    // CLI-045 (pi cli.md): `--verbose` overrides `quietStartup` for this run (in memory only).
    if a.verbose {
        settings = settings.with_override("quietStartup", Value::Bool(false));
    }
    // SET-008: `--models` replaces `enabledModels` for this run (the TUI's cycle list reads it).
    if let Some(patterns) = &a.models {
        let list = patterns.iter().cloned().map(serde_json::Value::String).collect();
        settings = settings.with_override("enabledModels", serde_json::Value::Array(list));
    }
    notices.extend(
        settings
            .diagnostics()
            .iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    // keybindings.json problems are reported in every mode (pi reports them at startup),
    // not only by the interactive terminal.
    let (_, keymap_diags) = Keymap::load(dirs);
    notices.extend(
        keymap_diags
            .iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    // SET-021 / SES-010: `--session-dir` > `OXA_SESSION_DIR` > the `sessionDir` setting
    // (relative to the working directory) > `<agent dir>/sessions`.
    let mut session_dirs = dirs.clone();
    if a.session_dir.is_none() && env.get("OXA_SESSION_DIR").is_none() {
        if let Some(dir) = settings.session_dir(&dirs.home, cwd) {
            session_dirs.sessions_dir = dir;
        }
    }
    let dirs = &session_dirs;
    // Spaced --continue is ambiguous until the effective store is known.
    if let Some(resolved) = resolve_continue_word(a, dirs) {
        *a = resolved;
    }
    // The engine client with the settings' silence limit (`httpIdleTimeoutMs`,
    // `retry.provider.timeoutMs`; SET-051, SET-054).
    let engine = child.wrap(engine_with_settings(&endpoint, &settings)?);

    let opts = discover_opts(a, dirs, cwd, trusted)?;
    // O-152: the built-in skills' files, so the model can read their bodies (written only
    // when missing or changed; a failure only costs the on-demand bodies).
    if settings.bool(oxa_resources::SETTING_BUILTIN_SKILLS, true) && !a.no_skills {
        if let Err(e) = oxa_resources::install_builtin_skills(dirs) {
            notices.push((Level::Warning, e.message));
        }
    }
    // O-152, automatic: the identity section is refreshed at every start, before the prefix is
    // built, from cheap local facts (machine, OS, oxa version) and the engine's model list (a
    // few loopback requests), so a missed `oxa identity --refresh` or an update never leaves
    // a stale or generic identity. The file is rewritten only when its bytes change, so the
    // prefix (and the cache) changes only when a fact did; a running session keeps its epoch's
    // prefix (W3). When the engine does not answer, an existing file keeps its model list.
    // Never under test isolation or in a subagent child (its parent just did it); a failure
    // only keeps the previous text.
    if env.get(oxa_proto::names::ENV_TEST_ISOLATION) != Some("1") && !child.nested() {
        let probe = (!oxa_proto::offline()).then_some(engine.as_ref());
        let facts = crate::identity_cmd::identity_facts(Path::new("/"), probe);
        let keep_known_models = facts.models.is_empty() && oxa_resources::read_identity(dirs).is_some();
        if !keep_known_models {
            if let Err(e) = crate::identity_cmd::write_identity(dirs, &facts) {
                notices.push((Level::Warning, e.message));
            }
        }
    }
    // Resources, package sources included (E10).
    let resources = discover(dirs, cwd, &settings, &opts);
    notices.extend(resources.diagnostics.iter().map(|d| {
        let text = match &d.path {
            Some(p) => format!("{}: {}", p.display(), d.message),
            None => d.message.clone(),
        };
        (d.level, text)
    }));
    // THM-003 / THM-004: invalid theme files are reported at startup in every mode.
    let cli_themes: Vec<PathBuf> = a
        .themes
        .iter()
        .map(|p| resolve_path(&dirs.home, cwd, p))
        .collect();
    notices.extend(crate::themes::theme_diagnostics(&crate::themes::ThemeSources {
        dirs,
        cwd,
        settings: &settings,
        trusted,
        package_paths: &resources.theme_paths,
        cli_paths: &cli_themes,
        no_themes: a.no_themes,
    }));
    let sandbox: Arc<dyn Sandbox> = match Bwrap::detect(env.get("PATH").unwrap_or(""), &dirs.home) {
        Ok(mut b) => {
            b.layout = crate::sandbox_layout::layout(env, dirs, cwd, home_workspace, &mut notices);
            // #457: each session gets its own directory at /tmp (`Built::deps`).
            let root = oxa_sandbox::scratch_root(&dirs.home);
            match std::fs::create_dir_all(&root) {
                Ok(()) => b.layout.scratch_root = Some(root),
                Err(e) => notices.push((
                    Level::Warning,
                    format!("no session scratch directory ({e}); /tmp is discarded after each command"),
                )),
            }
            let b = b.with_git_dirs(cwd);
            notices.extend(b.layout.git.notes.iter().map(|n| (Level::Warning, n.clone())));
            Arc::new(b)
        }
        Err(e) => {
            notices.push((
                Level::Warning,
                format!("bwrap is missing or unusable ({}); bash is disabled", e.message),
            ));
            Arc::new(NoSandbox)
        }
    };
    let sandbox = crate::netguard::with_net(sandbox, &settings);

    // `--model` resolves against the engine's models; its `:level` suffix sets thinking unless
    // `--thinking` is given.
    let mut model = None;
    let mut thinking = a.thinking;
    // `defaultModel` (SET-004) is applied by the core when no model was chosen here
    // (`Session::open`), so a resumed session keeps its own model and `--models` scopes it.
    if let Some(pattern) = &a.model {
        let info = engine.info(None)?;
        let mut ids = info.models.clone();
        if !ids.contains(&info.model) {
            ids.insert(0, info.model.clone());
        }
        let available: Vec<ModelRef> = ids
            .into_iter()
            .map(|id| ModelRef {
                endpoint: endpoint.name.clone(),
                model: id,
                thinking: None,
            })
            .collect();
        let picked = resolve_model(pattern, a.provider.as_deref(), &available)?;
        model = Some(picked.model);
        thinking = thinking.or(picked.thinking);
    } else {
        // CLI-015 (pi main.ts): `--models` (else `enabledModels`) scopes the startup model of a
        // new session: `defaultModel` when it is in scope, else the first scoped model, with the
        // thinking level of its pattern unless `--thinking` is given. Patterns that match
        // nothing are reported.
        let patterns = a.models.clone().unwrap_or_else(|| settings.list("enabledModels"));
        let continuing = a.continue_ || a.session.is_some() || a.fork.is_some() || a.resume;
        if !patterns.is_empty() && !continuing {
            let info = engine.info(None)?;
            let mut ids = info.models.clone();
            if !ids.contains(&info.model) {
                ids.insert(0, info.model.clone());
            }
            let available: Vec<ModelRef> = ids
                .into_iter()
                .map(|id| ModelRef {
                    endpoint: endpoint.name.clone(),
                    model: id,
                    thinking: None,
                })
                .collect();
            let (scoped, diags) = oxa_config::resolve_scoped(&patterns, &available);
            notices.extend(diags.into_iter().map(|d| (d.level, d.message)));
            let saved = settings.string("defaultModel");
            let pick = scoped
                .iter()
                .find(|m| saved.as_deref() == Some(m.model.as_str()))
                .or_else(|| scoped.first());
            if let Some(pick) = pick {
                model = Some(pick.model.clone());
                thinking = thinking.or(pick.thinking);
            }
        }
    }

    // O-182, #202: one switch for the policy of every session and for the subagent children.
    // The command line wins; else `permissions.defaultMode` (a project may only lower it).
    // `--permission-mode workspace|ask` alone keeps its old meaning: the table asks you.
    let level = a
        .level
        .or(match a.permission_mode {
            Some(Mode::Workspace | Mode::Ask) => Some(oxa_proto::PermissionMode::Standard),
            _ => None,
        })
        .unwrap_or_else(|| settings.default_permission_mode());
    let yolo = oxa_proto::YoloSwitch::new(level.autonomy());
    let table_mode = match (a.permission_mode, level) {
        (None, oxa_proto::PermissionMode::Plan) => Some(Mode::Plan),
        (m, _) => m,
    };
    if !matches!(mode, RunMode::Interactive) {
        notices.push((
            Level::Info,
            format!("permission mode {}: {}", level.as_str(), level.describe()),
        ));
    }
    let bash_sandbox = Arc::new(oxa_sandbox::FullAccess::new(
        sandbox.clone(),
        yolo.clone(),
        env.get("PATH").unwrap_or(""),
        dirs.home.clone(),
        oxa_proto::offline(),
    ));
    let hook_sets = build_hooks(
        &HookInputs {
            args: a,
            dirs,
            cwd,
            settings: &settings,
            resources: &resources,
            sandbox: &sandbox,
            trusted,
            nested: child.nested(),
            env,
            yolo: yolo.clone(),
        },
        &mut notices,
    )?;
    // `ctx.mode` of the extensions (pi's names).
    if let Some(ext) = &hook_sets.extensions {
        ext.set_mode(match mode {
            RunMode::Interactive => "tui",
            RunMode::Print => "print",
            RunMode::Json => "json",
            RunMode::Rpc => "rpc",
        });
    }

    let store: Arc<dyn SessionStore> = if a.no_session {
        Arc::new(MemStore::new())
    } else {
        Arc::new(FsStore::new(dirs.sessions_dir.clone()))
    };
    let (open, session_id) = choose_open(a, store.as_ref(), cwd, mode, &mut notices, confirm)?;
    let name = match a.name.as_deref().map(str::trim) {
        Some("") => {
            return Err(OxaError::new(codes::INVALID, "--name requires a non-empty value"));
        }
        other => other.map(str::to_string),
    };

    let selection = ToolSelection {
        default_tools: settings.default_tools(),
        tools: a.tools.clone(),
        exclude: a.exclude_tools.clone(),
        no_builtin: a.no_builtin_tools,
        no_tools: a.no_tools,
        default_edits: settings.default_tools_edits(),
    };
    let built = Built {
        output_schema,
        engine,
        store,
        sandbox,
        settings,
        dirs: dirs.clone(),
        cwd: cwd.to_path_buf(),
        resources,
        selection,
        model,
        thinking,
        mode: table_mode,
        supervised_child: child.nested(),
        ask_owner_present: mode != RunMode::Rpc || env.get("OXA_RPC_OWNER_PRESENT") == Some("1"),
        hook_sets,
        yolo,
        bash_sandbox,
    };
    Ok(Prep {
        built,
        open,
        session_id,
        notices,
        gated,
        name,
    })
}

/// The resource discovery options of the flags.
/// The same project trust and executable-content approval for sessions and workers.
pub(crate) fn project_trusted(
    a: &Args,
    env: &Env,
    dirs: &Dirs,
    cwd: &Path,
    gated: &[String],
    interactive: bool,
    notices: &mut Vec<(Level, String)>,
) -> Result<bool, OxaError> {
    // Trust: the user's own settings decide the default; project settings load only when the
    // project is trusted.
    let user_settings = Settings::load(dirs, cwd, false);
    let default = match user_settings.string("defaultProjectTrust").as_deref() {
        Some("always") => DefaultTrust::Trust,
        Some("never") => DefaultTrust::Untrusted,
        _ => DefaultTrust::Ask,
    };
    // `OXA_TRUST_PROJECT=1` is `--approve` for the trust decision, for unattended runs;
    // an explicit `-na` still wins.
    let approve = a
        .approve
        .or_else(|| matches!(env.get("OXA_TRUST_PROJECT"), Some("1" | "true")).then_some(true));
    let mut trust_store = TrustStore::load(&dirs.trust_file());
    // SEC-003 / EXT-008: the user-level and `-e` extensions answer `project_trust` before the
    // saved decision and the default (pi's `resolveProjectTrusted`).
    let extension = if approve.is_none() && !gated.is_empty() {
        extension_trust(a, dirs, cwd, &user_settings, &mut trust_store, notices)
    } else {
        None
    };
    let saved = trust_store.lookup(cwd);
    let outcome = resolve_trust(&TrustInputs {
        flag: approve,
        extension,
        saved,
        default,
        interactive,
        has_gated: !gated.is_empty(),
    });
    // B4: the user answered the trust question in this start (it approves the content too).
    let mut asked_now = false;
    let trusted = match outcome {
        TrustOutcome::Trusted => true,
        TrustOutcome::Untrusted => false,
        TrustOutcome::AskUser => {
            // The trust dialog (D10) on the terminal, before the session exists. A dismissed
            // question leaves the project untrusted for this run and is asked again next time.
            let (keymap, _) = Keymap::load(dirs);
            let choice = ask_trust_interactive(env.pairs(), &keymap, &cwd.display().to_string(), gated)?;
            asked_now = true;
            let file = dirs.trust_file();
            match choice {
                Some(TrustChoice::Trust) => {
                    trust_store.save(&file, cwd, true)?;
                    true
                }
                Some(TrustChoice::TrustParent) => {
                    trust_store.save(&file, cwd.parent().unwrap_or(cwd), true)?;
                    true
                }
                Some(TrustChoice::NoTrust) => {
                    trust_store.save(&file, cwd, false)?;
                    false
                }
                Some(TrustChoice::Skip) | None => {
                    notices.push((
                        Level::Warning,
                        "the project is not trusted for this run; /trust saves a decision".to_string(),
                    ));
                    false
                }
            }
        }
    };
    // B4: what runs outside the sandbox loads only as the user approved it (content hashes).
    let trusted = if trusted && !gated.is_empty() {
        crate::approval::check(crate::approval::Check {
            env,
            dirs,
            cwd,
            store: &mut trust_store,
            via_flag: approve == Some(true),
            via_default: extension.is_none() && saved.is_none() && default == DefaultTrust::Trust,
            asked_now,
            interactive,
            notices,
        })?
    } else {
        trusted
    };
    // #201: a project trusted by a saved decision gets its approval baseline now (also with
    // no project config yet), before any tool runs: `.oxa` / `.agents` content the session
    // creates is then new at the next start and goes through the normal approval.
    if trusted && saved == Some(true) {
        trust_store.record_baseline(&dirs.trust_file(), cwd)?;
    }
    if !trusted && !gated.is_empty() {
        notices.push((
            Level::Warning,
            format!(
                "project resources were skipped (not trusted): {}",
                gated.join(", ")
            ),
        ));
    }
    // `done` is a project-only key in practice: an untrusted project's list is never merged,
    // so say that its checks will not run.
    if !trusted && Settings::project_declares_done(dirs, cwd) {
        notices.push((
            Level::Warning,
            "done checks in .oxa/settings.json were not run: project is not trusted".to_string(),
        ));
    }
    Ok(trusted)
}

fn discover_opts(a: &Args, dirs: &Dirs, cwd: &Path, trusted: bool) -> Result<DiscoverOpts, OxaError> {
    Ok(DiscoverOpts {
        trusted,
        no_context: a.no_context_files,
        no_skills: a.no_skills,
        no_templates: a.no_prompt_templates,
        no_extensions: a.no_extensions,
        extra_skills: a
            .skills
            .iter()
            .map(|p| resolve_path(&dirs.home, cwd, p))
            .collect(),
        extra_templates: a
            .prompt_templates
            .iter()
            .map(|p| resolve_path(&dirs.home, cwd, p))
            .collect(),
        extra_extensions: a.extensions.clone(),
        system_prompt: match &a.system_prompt {
            Some(text) => Some(text_or_file(text, &dirs.home, cwd)?),
            None => None,
        },
        append_system_prompt: a
            .append_system_prompt
            .iter()
            .map(|text| text_or_file(text, &dirs.home, cwd))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

/// CLI-048: the flags the extensions of this run register, for `--help` (pi main.ts loads the
/// resources, then prints the help with them). Trust is decided without asking: a project
/// that would need the dialog stays untrusted. Problems are reported on `err` and give no
/// flags; the help is printed anyway.
fn help_extension_flags(a: &Args, env: &Env, cwd: &Path, err: &mut dyn Write) -> Vec<oxa_proto::FlagSpec> {
    let Ok(dirs) = dirs_from_env(env, a.session_dir.as_deref()) else {
        return Vec::new();
    };
    let user = Settings::load(&dirs, cwd, false);
    let default = match user.string("defaultProjectTrust").as_deref() {
        Some("always") => DefaultTrust::Trust,
        Some("never") => DefaultTrust::Untrusted,
        _ => DefaultTrust::Ask,
    };
    let gated = gated_resources(cwd);
    let trusted = resolve_trust(&TrustInputs {
        flag: a.approve,
        extension: None,
        saved: TrustStore::load(&dirs.trust_file()).lookup(cwd),
        default,
        interactive: false,
        has_gated: !gated.is_empty(),
    }) == TrustOutcome::Trusted;
    let settings = Settings::load(&dirs, cwd, trusted);
    let opts = match discover_opts(a, &dirs, cwd, trusted) {
        Ok(o) => o,
        Err(e) => {
            report(err, &e);
            return Vec::new();
        }
    };
    let resources = discover(&dirs, cwd, &settings, &opts);
    if resources.extension_paths.is_empty() {
        return Vec::new();
    }
    match oxa_ext::ExtensionHost::new(cwd.to_path_buf()) {
        Ok(host) => {
            for ext in host.load(&resources.extension_paths, &oxa_ext::LoadOpts::default()) {
                if let Some(error) = ext.error {
                    let _ = writeln!(err, "Error: extension {}: {error}", ext.path);
                }
            }
            let flags = oxa_proto::Hooks::flags(&host);
            oxa_proto::Hooks::shutdown(&host);
            flags
        }
        Err(e) => {
            let _ = writeln!(err, "Error: extensions are not available: {e}");
            Vec::new()
        }
    }
}

/// `--continue <word>`: the word is the id of an existing session, or else the first words of
/// the prompt (and the latest session continues). `None` when there is nothing to settle.
fn resolve_continue_word(a: &Args, dirs: &Dirs) -> Option<Args> {
    let store = FsStore::new(dirs.sessions_dir.clone());
    let (word, at) = a.continue_word.as_ref()?;
    let mut out = a.clone();
    out.continue_word = None;
    let is_id = !a.no_session
        && a.session.is_none()
        && a.fork.is_none()
        && !a.resume
        && crate::args::valid_session_id(word)
        && store.resolve(word).is_ok_and(|session| session.id == *word);
    if is_id {
        out.continue_id = Some(word.clone());
    } else {
        out.messages.insert((*at).min(out.messages.len()), word.clone());
    }
    Some(out)
}

/// The open mode of the flags: `--no-session`, `--fork`, `--session`, `--session-id`,
/// `--continue`, `--resume` (interactive only, through the picker) or a new session.
fn choose_open(
    a: &Args,
    store: &dyn SessionStore,
    cwd: &Path,
    mode: RunMode,
    _notices: &mut Vec<(Level, String)>,
    confirm: &mut dyn FnMut(&str, &str) -> bool,
) -> Result<(Open, Option<String>), OxaError> {
    if a.no_session {
        return Ok((Open::Ephemeral, a.session_id.clone()));
    }
    if let Some(s) = &a.fork {
        // pi passes `--session-id` on to `SessionManager.forkFrom` (the fork's new id).
        return Ok((Open::Fork(store.resolve(s)?), a.session_id.clone()));
    }
    if let Some(s) = a.continue_id.as_ref().or(a.session.as_ref()) {
        let sref = store.resolve(s)?;
        // pi main.ts `resolveSessionPath`: a path opens as it is; an id found only in another
        // directory is offered as a fork into this one (declining exits 0 with "Aborted.").
        let is_path = s.contains('/') || s.contains('\\') || s.ends_with(".jsonl");
        if !is_path {
            if let Ok(header) = store.header(&sref) {
                if header.cwd != cwd {
                    let notice = format!("Session found in different project: {}", header.cwd.display());
                    if mode == RunMode::Rpc {
                        return Err(OxaError::new(codes::INVALID, notice)
                            .with_hint("use --fork <id> to copy it into this directory"));
                    }
                    if confirm(&notice, "Fork this session into current directory?") {
                        return Ok((Open::Fork(sref), None));
                    }
                    return Err(OxaError::new(FORK_DECLINED, "Aborted."));
                }
            }
        }
        return Ok((Open::Resume(sref), None));
    }
    if let Some(id) = &a.session_id {
        // The current directory's session with this id resumes; otherwise a new one is created
        // with the id.
        if let Ok(sref) = store.resolve(id) {
            if store.header(&sref).map(|h| h.cwd == cwd).unwrap_or(false) {
                return Ok((Open::Resume(sref), None));
            }
        }
        return Ok((Open::New, Some(id.clone())));
    }
    if a.continue_ {
        return Ok((Open::Continue, None));
    }
    if a.resume {
        if mode == RunMode::Interactive {
            // The interactive mode opens the session picker at start (`open_picker`).
            return Ok((Open::New, None));
        }
        return Err(OxaError::new(
            codes::USAGE,
            "--resume needs the interactive session picker; use --continue or --session <id>",
        )
        .with_hint(USAGE_HINT));
    }
    Ok((Open::New, None))
}

/// The mode runs that print to stdout and exit: `-p` and `--mode json`.
fn run_headless(
    mode: RunMode,
    max_time: Option<std::time::Duration>,
    max_tokens: Option<u64>,
    prep: Prep,
    prompts: Vec<(String, Vec<Part>)>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let Prep {
        built,
        open,
        session_id,
        mut notices,
        name,
        ..
    } = prep;
    let mut session = match open_named(&built, session_id, open, name.as_deref()) {
        Ok(s) => s,
        Err(e) => return finish_outcome(Err(e), stderr),
    };
    notices.extend(startup_notices(&built, &session));
    write_notices(stderr, notices);
    if mode == RunMode::Print {
        // Printed before the run so a driver that kills a long task still knows what to resume.
        let st = session.state();
        if st.session_file.is_some() {
            let _ = writeln!(
                stderr,
                "oxa: session {} (resume with --continue {})",
                st.session_id, st.session_id
            );
        }
    }
    let _limit = match crate::limits::LimitWatcher::start(
        session.handle(),
        max_time,
        max_tokens,
        built.supervised_child,
    ) {
        Ok(limit) => limit,
        Err(error) => return finish_outcome(Err(error), stderr),
    };
    let result = match mode {
        RunMode::Json => run_json_outcome(&mut session, prompts, stdout, stderr),
        _ => run_print_outcome(&mut session, prompts, stdout, stderr),
    };
    // `session_shutdown` for the hook sets, then their script contexts and MCP servers stop.
    let mut ui = oxa_proto::HeadlessUi::default();
    session.shutdown(&mut ui);
    write_notices(stderr, ui.notes);
    if mode == RunMode::Json && built.supervised_child && max_tokens.is_some() {
        // Private child/parent accounting: message_end omits summaries and tool
        // prefill. Report the uncapped total, including the child's final turn.
        let usage = serde_json::json!({
            "type": "run_usage",
            "tokens": session.handle().run_tokens_used(),
        });
        let _ = writeln!(stdout, "{usage}");
    }
    finish_outcome(result, stderr)
}

/// `--mode rpc`: the protocol owns stdin and stdout; notices go to stderr.
fn run_rpc(
    prep: Prep,
    max_time: Option<std::time::Duration>,
    max_tokens: Option<u64>,
    dirs: Dirs,
    stdin: Box<dyn Read + Send>,
    stdout: Box<dyn Write + Send>,
    stderr: &mut dyn Write,
) -> i32 {
    let Prep {
        built,
        open,
        session_id,
        mut notices,
        name,
        ..
    } = prep;
    let session = match open_named(&built, session_id, open, name.as_deref()) {
        Ok(s) => s,
        Err(e) => return finish_outcome(Err(e), stderr),
    };
    notices.extend(startup_notices(&built, &session));
    write_notices(stderr, notices);
    let _limit = match crate::limits::LimitWatcher::start(
        session.handle(),
        max_time,
        max_tokens,
        built.supervised_child,
    ) {
        Ok(limit) => limit,
        Err(error) => return finish_outcome(Err(error), stderr),
    };
    let out: Out = Arc::new(Mutex::new(stdout));
    let input = Box::new(BufReader::new(stdin));
    finish_outcome(serve_outcome(session, dirs, input, out), stderr)
}

/// The interactive terminal: the first prompt and the further messages come from the command
/// line; `reopen` builds the sessions of `/new`, `/resume`, fork, clone and import.
fn run_tui(a: &Args, env: &Env, dirs: &Dirs, cwd: &Path, prep: Prep, stderr: &mut dyn Write) -> i32 {
    let prompts = match build_prompts(a, dirs, cwd, None) {
        Ok(p) => p,
        Err(e) => return report(stderr, &e),
    };
    let mut prompts = prompts.into_iter();
    let (initial_prompt, initial_parts) = match prompts.next() {
        Some((text, parts)) => ((!text.is_empty()).then_some(text), parts),
        None => (None, Vec::new()),
    };
    let initial_follow_ups: Vec<String> = prompts.map(|(text, _)| text).collect();

    let Prep {
        built,
        open,
        session_id,
        mut notices,
        gated,
        name,
    } = prep;
    let session = match open_named(&built, session_id, open, name.as_deref()) {
        Ok(s) => s,
        Err(e) => return report(stderr, &e),
    };
    notices.extend(startup_notices(&built, &session));
    let yolo = built.yolo.clone();
    // Its diagnostics are among the startup notices (`prepare`).
    let (keymap, _) = Keymap::load(dirs);
    let trust_file = dirs.trust_file();
    let trust_dir = cwd.to_path_buf();
    let save_trust: Box<dyn FnMut(TrustChoice) -> Result<(), OxaError> + Send> = Box::new(move |choice| {
        let mut store = TrustStore::load(&trust_file);
        // B4: `/trust` approves the project files as they are now.
        match choice {
            TrustChoice::Trust => store
                .save(&trust_file, &trust_dir, true)
                .and_then(|_| store.approve(&trust_file, &trust_dir)),
            TrustChoice::TrustParent => store
                .save(&trust_file, trust_dir.parent().unwrap_or(&trust_dir), true)
                .and_then(|_| store.approve(&trust_file, &trust_dir)),
            TrustChoice::NoTrust => store.save(&trust_file, &trust_dir, false),
            TrustChoice::Skip => Ok(()),
        }
    });
    let settings = built.settings.clone();
    let store = built.store.clone();
    let base = built.clone();
    let reopen: ReopenFn = Box::new(move |request, carry| {
        let mut deps = base.deps(None);
        // `/new` keeps the model and thinking level of the session it leaves (a `/model`
        // switch is present in the status bar but only as a `ModelChange` entry of the old
        // session); `/resume`, fork and import restore the target session's own, and any
        // startup `--model` choice in `base` is left untouched for them.
        let how = match request {
            Reopen::New => {
                if let Some((model, thinking)) = carry {
                    deps.model = Some(model);
                    deps.thinking = Some(thinking);
                }
                Open::New
            }
            Reopen::Resume(sref) => Open::Resume(sref),
            Reopen::Fork(sref) => Open::Fork(sref),
        };
        let mut session = Session::open(deps, how)?;
        session.set_output_schema(base.output_schema.clone());
        Ok(session)
    });
    let opts = InteractiveOpts {
        // `--tui-mode` over the `tuiMode` setting (default fullscreen).
        fullscreen: a
            .tui_mode
            .clone()
            .or_else(|| settings.string("tuiMode"))
            .as_deref()
            != Some("regular"),
        dirs: dirs.clone(),
        settings,
        keymap,
        store,
        hooks: built.hook_sets.hooks.clone(),
        mcp: built.hook_sets.mcp.clone(),
        initial_prompt,
        initial_parts,
        initial_follow_ups,
        theme: a.use_theme.clone().or_else(|| a.themes.first().cloned()),
        notices,
        env: env.pairs().to_vec(),
        caps: None,
        reopen,
        open_picker: a.resume,
        save_trust: Some(save_trust),
        gated,
        yolo: Some(yolo),
    };
    match run_interactive(session, opts) {
        Ok(code) => code,
        Err(e) => report(stderr, &e),
    }
}

/// The default engine endpoint of models.json (or the one `OXA_ENGINE_URL` names).
pub(crate) fn default_endpoint(models: &ModelsConfig) -> Result<Endpoint, OxaError> {
    models
        .endpoints
        .iter()
        .find(|ep| ep.name == models.default)
        .or_else(|| models.endpoints.first())
        .cloned()
        .ok_or_else(|| OxaError::new(codes::NOT_FOUND, "no engine endpoint is configured"))
}

pub(crate) fn engine_for(endpoint: &Endpoint) -> Result<Arc<dyn Engine>, OxaError> {
    crate::netguard::check_endpoint(&endpoint.base_url)?;
    let engine = HttpEngine::new(&endpoint.base_url, endpoint.api_key.clone())?;
    Ok(Arc::new(engine))
}

/// CLI-013: `--api-key` replaces the endpoint's key for this run (sent only as
/// `Authorization: Bearer`; never stored in a session or put on the model wire).
fn with_api_key(mut endpoint: Endpoint, a: &Args) -> Endpoint {
    if let Some(key) = &a.api_key {
        endpoint.api_key = Some(key.0.clone());
    }
    endpoint
}

/// [`engine_for`] with the stream silence limit of the settings: `retry.provider.timeoutMs`,
/// else `httpIdleTimeoutMs` (`0` / `"disabled"` = no limit).
fn engine_with_settings(endpoint: &Endpoint, settings: &Settings) -> Result<Arc<dyn Engine>, OxaError> {
    /// "Disabled": a silence limit no stream reaches (one year).
    const NO_LIMIT_MS: u64 = 365 * 24 * 3600 * 1000;
    // P3: the same remote-engine guard as `engine_for`.
    crate::netguard::check_endpoint(&endpoint.base_url)?;
    let silence = settings
        .provider_retry()
        .timeout_ms
        .filter(|ms| *ms > 0)
        .or_else(|| settings.http_idle_timeout_ms())
        .unwrap_or(NO_LIMIT_MS);
    let opts = oxa_engine::StreamOpts {
        silence_limit_ms: silence,
        ..oxa_engine::StreamOpts::default()
    };
    // The core owns session retries so every retry uses its backoff and visible notice.
    let engine = HttpEngine::new(&endpoint.base_url, endpoint.api_key.clone())?.with_opts(opts, 0);
    Ok(Arc::new(engine))
}

/// The provider column: the controller's `remote_provider` (else `cloud`) for a cloud model,
/// the endpoint name for a local one.
fn row_provider(endpoint: &str, info: &oxa_proto::EngineInfo) -> String {
    match (info.caps.remote, info.caps.remote_provider) {
        (_, Some(p)) => p.to_string(),
        (true, None) => "cloud".to_string(),
        _ => endpoint.to_string(),
    }
}

/// `--list-models [search]`: a table of the engine's models (pi's columns), sorted by id.
fn list_models(
    engine: &dyn Engine,
    provider: &str,
    search: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let info = engine.info(None)?;
    let mut ids = info.models.clone();
    if !ids.contains(&info.model) {
        ids.push(info.model.clone());
    }
    ids.sort();
    ids.dedup();
    let needle = search.map(|s| s.to_lowercase());
    let rows: Vec<[String; 6]> = ids
        .into_iter()
        .map(|id| {
            let model_info = if id == info.limits_for {
                Ok(info.clone())
            } else {
                engine.info(Some(&id))
            };
            match model_info {
                Ok(model_info) => [
                    row_provider(provider, &model_info),
                    id,
                    format_tokens(model_info.n_ctx as u64),
                    format_tokens(model_info.max_output as u64),
                    model_info.thinking.map_or_else(|| "?".to_string(), bool_word),
                    model_info.images.map_or_else(|| "?".to_string(), bool_word),
                ],
                // A failing per-model info call degrades only this row: '?' in the limits
                // columns (never another model's limits), and the listing continues.
                Err(e) => {
                    let _ = writeln!(err, "warning: cannot list {id}: {}", e.message);
                    [
                        provider.to_string(),
                        id,
                        "?".to_string(),
                        "?".to_string(),
                        "?".to_string(),
                        "?".to_string(),
                    ]
                }
            }
        })
        .filter(|row| match &needle {
            Some(n) => format!("{} {}", row[0], row[1])
                .to_lowercase()
                .contains(n.as_str()),
            None => true,
        })
        .collect();
    let write_err = |e: std::io::Error| OxaError::io("cannot write the model list", &e);
    if rows.is_empty() {
        match search {
            Some(s) => writeln!(out, "No models matching \"{s}\"").map_err(write_err)?,
            None => writeln!(out, "No models available").map_err(write_err)?,
        }
        return Ok(0);
    }
    let headers = ["provider", "model", "context", "max-out", "thinking", "images"];
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let line = |cells: &[&str]| -> String {
        // Every column but the last is padded, so the lines carry no trailing spaces.
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                s.push_str(cell);
            } else {
                s.push_str(&format!("{cell:<w$}  ", w = widths[i]));
            }
        }
        s
    };
    writeln!(out, "{}", line(&headers)).map_err(write_err)?;
    for row in &rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        writeln!(out, "{}", line(&cells)).map_err(write_err)?;
    }
    Ok(0)
}

/// `true` -> `yes`, `false` -> `no` for the `thinking`/`images` columns of `--list-models`.
fn bool_word(b: bool) -> String {
    if b {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

/// pi's token counts: `200K`, `1M`, `1.5M`; below a thousand the plain number.
fn format_tokens(count: u64) -> String {
    let scaled = |div: f64, unit: &str| {
        let v = count as f64 / div;
        if v.fract() == 0.0 {
            format!("{v}{unit}")
        } else {
            format!("{v:.1}{unit}")
        }
    };
    if count >= 1_000_000 {
        scaled(1_000_000.0, "M")
    } else if count >= 1_000 {
        scaled(1_000.0, "K")
    } else {
        count.to_string()
    }
}

/// `--export <input> [output]`: the session file to HTML; the output name is derived when it
/// is omitted. Prints `Exported to: <path>`.
fn export_session(
    input: &str,
    output: Option<&str>,
    home: &Path,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match export_inner(input, output, home, cwd) {
        Ok(path) => {
            let _ = writeln!(out, "Exported to: {}", path.display());
            0
        }
        Err(e) => report(err, &e),
    }
}

fn export_inner(input: &str, output: Option<&str>, home: &Path, cwd: &Path) -> Result<PathBuf, OxaError> {
    let input_path = resolve_path(home, cwd, input);
    let text = std::fs::read_to_string(&input_path)
        .map_err(|e| OxaError::io(&format!("cannot read {}", input_path.display()), &e))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next().ok_or_else(|| {
        OxaError::new(
            codes::INVALID,
            format!("{} has no session header", input_path.display()),
        )
    })?;
    let header = decode_header(first.as_bytes())?;
    let entries = lines
        .map(|line| decode_entry(line.as_bytes()))
        .collect::<Result<Vec<_>, OxaError>>()?;
    let opts = ExportOpts {
        theme: ExportTheme::dark(),
        session_name: None,
        system_prompt: None,
        tools: Value::Array(Vec::new()),
        rendered_tools: Vec::new(),
        leaf: None,
    };
    let html = export_html(&header, &entries, &opts)?;
    let dest = match output {
        Some(o) => resolve_path(home, cwd, o),
        None => default_output_name(Some(&input_path), &header.id, cwd),
    };
    write_export(&dest, &html)
}

/// The error code of a declined cross-directory fork offer (CLI-019): `run` prints
/// "Aborted." and exits 0, as pi does.
const FORK_DECLINED: &str = "oxa_cli_fork_declined";

/// One line from `stdin`, read byte by byte (nothing after the newline is consumed).
fn read_line(stdin: &mut dyn Read) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = stdin.read(&mut byte) {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8_lossy(&line).into_owned()
}

/// Piped stdin (trimmed), `None` when stdin is a terminal or empty.
fn read_piped_stdin(stdin: &mut dyn Read, stdin_is_tty: bool) -> Result<Option<String>, OxaError> {
    if stdin_is_tty {
        return Ok(None);
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .map_err(|e| OxaError::io("cannot read stdin", &e))?;
    let trimmed = text.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

/// The prompts of a run, as pi builds them: piped stdin, the `@file` texts and the first
/// message form the first prompt (images become parts of it); each further message is its own
/// prompt.
fn build_prompts(
    a: &Args,
    dirs: &Dirs,
    cwd: &Path,
    stdin_text: Option<String>,
) -> Result<Vec<(String, Vec<Part>)>, OxaError> {
    let mut text = stdin_text.unwrap_or_default();
    let mut parts: Vec<Part> = Vec::new();
    for arg in &a.files {
        let path = resolve_path(&dirs.home, cwd, arg);
        let shown = path.display().to_string();
        let meta = std::fs::metadata(&path)
            .map_err(|_| OxaError::new(codes::NOT_FOUND, format!("File not found: {shown}")))?;
        if meta.len() == 0 {
            continue;
        }
        let data =
            std::fs::read(&path).map_err(|e| OxaError::io(&format!("Could not read file {shown}"), &e))?;
        match image_mime(&data) {
            Some(mime) => {
                let img = ClipImage {
                    data,
                    mime: mime.to_string(),
                };
                parts.push(image_part(&img, &dirs.tmp_dir)?);
                text.push_str(&format!("<file name=\"{shown}\"></file>\n"));
            }
            None => {
                let content = String::from_utf8_lossy(&data);
                let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
                text.push_str(&format!("<file name=\"{shown}\">\n{content}\n</file>\n"));
            }
        }
    }
    let mut messages = a.messages.iter();
    if let Some(first) = messages.next() {
        text.push_str(first);
    }
    let mut prompts = Vec::new();
    if !text.is_empty() || !parts.is_empty() {
        prompts.push((text, parts));
    }
    for message in messages {
        prompts.push((message.clone(), Vec::new()));
    }
    Ok(prompts)
}

/// The image type by its magic bytes (PNG, JPEG, GIF, WebP).
fn image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF8") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A path argument: `~` expands to `home` (from the `Env` given to `run`, never the process
/// environment: P56), a relative path is taken from `cwd`.
fn resolve_path(home: &Path, cwd: &Path, arg: &str) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// `--system-prompt` and `--append-system-prompt`: the value is text, or the contents of an
/// existing file.
fn text_or_file(value: &str, home: &Path, cwd: &Path) -> Result<String, OxaError> {
    let path = resolve_path(home, cwd, value);
    if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e))
    } else {
        Ok(value.to_string())
    }
}

fn write_notices(err: &mut dyn Write, notices: Vec<(Level, String)>) {
    write_diagnostics(err, notices.into_iter());
}

fn write_diagnostics(err: &mut dyn Write, notices: impl Iterator<Item = (Level, String)>) {
    for (level, text) in notices {
        let label = match level {
            Level::Info => "Info",
            Level::Warning => "Warning",
            Level::Error => "Error",
        };
        let _ = writeln!(err, "{label}: {text}");
    }
}

/// One error line on stderr: the message, then the hint in parentheses. Returns the exit code.
fn report(err: &mut dyn Write, e: &OxaError) -> i32 {
    let line = match &e.hint {
        Some(hint) => format!("{} ({hint})", e.message),
        None => e.message.clone(),
    };
    let _ = writeln!(err, "Error: {line}");
    exit_code(e)
}

fn finish_outcome(result: Result<oxa_core::RunOutcome, OxaError>, err: &mut dyn Write) -> i32 {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            report(err, &error);
            oxa_core::RunOutcome::from_error(&error)
        }
    };
    let _ = writeln!(err, "{}", outcome.record());
    let _ = err.flush();
    outcome.exit_code()
}

fn finish(result: Result<i32, OxaError>, err: &mut dyn Write) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) => report(err, &e),
    }
}

/// 2 for usage errors, an unsupported pi flag and "no engine"; 1 for every other failure.
fn exit_code(e: &OxaError) -> i32 {
    match e.code {
        codes::USAGE | codes::ENGINE_DOWN | codes::UNSUPPORTED_IN_OXA => 2,
        codes::BUDGET_EXHAUSTED => oxa_proto::EXIT_BUDGET,
        _ => 1,
    }
}

/// Fire `project_trust` on the extensions that may decide trust (pi: user/global and CLI `-e`
/// extensions only; project-local ones are not loaded before trust is resolved), in a host of
/// their own that is dropped afterwards. The first `yes` / `no` wins; `remember: true` saves it.
/// `None` = no extension decided.
fn extension_trust(
    a: &Args,
    dirs: &Dirs,
    cwd: &Path,
    user_settings: &Settings,
    store: &mut TrustStore,
    notices: &mut Vec<(Level, String)>,
) -> Option<bool> {
    use oxa_proto::{HookPoint, HookReply, Hooks};
    let opts = DiscoverOpts {
        trusted: false,
        no_context: true,
        no_skills: true,
        no_templates: true,
        no_extensions: a.no_extensions,
        extra_extensions: a.extensions.clone(),
        ..DiscoverOpts::default()
    };
    let paths = discover(dirs, cwd, user_settings, &opts).extension_paths;
    if paths.is_empty() {
        return None;
    }
    let host = match oxa_ext::ExtensionHost::new(cwd.to_path_buf()) {
        Ok(h) => h,
        Err(_) => return None,
    };
    let _ = host.load(&paths, &oxa_ext::LoadOpts::default());
    host.attach(Arc::new(TrustHost {
        cwd: cwd.to_path_buf(),
        mode: a.permission_mode.unwrap_or(Mode::Workspace),
    }));
    // Load errors are reported by the full load after trust is resolved.
    let _ = host.state().take_errors();
    let payload = serde_json::json!({ "cwd": cwd.display().to_string() });
    let mut ui = oxa_proto::HeadlessUi::default();
    let reply = host.fire(HookPoint::ProjectTrust, &payload, &mut ui);
    for e in host.state().take_errors() {
        notices.push((
            Level::Warning,
            format!(
                "Extension \"{}\" project_trust error: {}",
                e.extension_path, e.error
            ),
        ));
    }
    host.shutdown();
    match reply {
        HookReply::Trust(t) => Some(t),
        HookReply::TrustRemembered(t) => {
            if let Err(e) = store.save(&dirs.trust_file(), cwd, t) {
                notices.push((Level::Warning, e.message));
            }
            Some(t)
        }
        _ => None,
    }
}

/// The host of the trust-time extensions: the session does not exist yet, so only file reads
/// and writes are served (through the permission policy of the mode, no prompt); every other
/// action is refused (pi's limited `ProjectTrustContext`).
struct TrustHost {
    cwd: PathBuf,
    mode: Mode,
}

impl oxa_proto::Host for TrustHost {
    fn call(&self, action: oxa_proto::HostAction) -> Result<Value, OxaError> {
        use oxa_proto::{Access, Decision, HostAction, Policy};
        let gate = |access: Access| match ModePolicy::new().decide(self.mode, &access, &self.cwd) {
            Decision::Allow => Ok(()),
            _ => Err(OxaError::new(
                codes::DENIED,
                "not allowed while project trust is being resolved",
            )),
        };
        match action {
            HostAction::ReadFile(path) => {
                let p = oxa_perm::normalize(&path, &self.cwd);
                gate(Access::Read(p.clone()))?;
                std::fs::read_to_string(&p)
                    .map(Value::String)
                    .map_err(|e| OxaError::io(&format!("read {}", p.display()), &e))
            }
            HostAction::WriteFile { path, data } => {
                let p = oxa_perm::normalize(&path, &self.cwd);
                gate(Access::Write(p.clone()))?;
                std::fs::write(&p, &data)
                    .map(|_| Value::Null)
                    .map_err(|e| OxaError::io(&format!("write {}", p.display()), &e))
            }
            HostAction::IsIdle => Ok(Value::Bool(true)),
            _ => Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                "only file access is available while project trust is being resolved",
            )),
        }
    }
}

#[cfg(test)]
mod model_limits_tests {
    use super::*;
    use oxa_proto::{AssistantTurn, CancelToken, ChatRequest, EngineInfo, StreamEvent};

    struct CatalogEngine;

    impl Engine for CatalogEngine {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            let models =
                serde_json::from_str(include_str!("../../oxa-engine/tests/fixtures/remote-models.json"))
                    .unwrap();
            let status = serde_json::json!({
                "model": "local", "context_size": 131072, "max_output": 16384,
                "model_limits": {"anthropic/claude-sonnet-5-5": {"capabilities":
                    {"remote": true, "remote_provider": "anthropic"}}}
            });
            oxa_engine::parse_info_for("http://localhost:1234", &models, &status, target)
        }

        fn count_tokens(&self, _: &str) -> Result<u32, OxaError> {
            panic!("listing models must not tokenize")
        }

        fn chat(
            &self,
            _: &ChatRequest,
            _: &CancelToken,
            _: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            panic!("listing models must not generate")
        }
    }

    #[test]
    fn list_models_uses_each_models_limits() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        list_models(&CatalogEngine, "oxe", Some("sonnet"), &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        let row: Vec<_> = text.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(&row[1..4], &["anthropic/claude-sonnet-5-5", "200K", "32K"]);

        let mut out = Vec::new();
        list_models(&CatalogEngine, "oxe", Some("local"), &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        let row: Vec<_> = text.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(&row[1..4], &["local", "131.1K", "16.4K"]);
    }

    #[test]
    fn list_models_shows_the_cloud_provider() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        list_models(&CatalogEngine, "oxe", None, &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        let col0 = |id: &str| {
            let l = text.lines().find(|l| l.split_whitespace().nth(1) == Some(id));
            l.unwrap().split_whitespace().next().unwrap().to_string()
        };
        assert_eq!(col0("anthropic/claude-sonnet-5-5"), "anthropic");
        assert_eq!(col0("local"), "oxe");
    }

    /// A failing per-model `info(Some(id))` degrades only that row: '?' in the context,
    /// max-out, thinking and images columns, while every other model still lists.
    struct FailingInfoEngine {
        fail_id: String,
    }

    impl Engine for FailingInfoEngine {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            if target == Some(self.fail_id.as_str()) {
                return Err(OxaError::new(codes::ENGINE_HTTP, "boom"));
            }
            CatalogEngine.info(target)
        }

        fn count_tokens(&self, _: &str) -> Result<u32, OxaError> {
            panic!("listing models must not tokenize")
        }

        fn chat(
            &self,
            _: &ChatRequest,
            _: &CancelToken,
            _: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            panic!("listing models must not generate")
        }
    }

    #[test]
    fn list_models_degrades_a_failing_info_row() {
        let engine = FailingInfoEngine {
            fail_id: "anthropic/claude-sonnet-5-5".to_string(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        list_models(&engine, "oxe", None, &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();

        // The listing succeeds and still contains every model id.
        for id in ["local", "anthropic/claude-sonnet-5-5"] {
            assert!(
                text.lines().any(|l| l.split_whitespace().nth(1) == Some(id)),
                "missing {id}"
            );
        }

        // A one-line warning on stderr names the degrading row and its error.
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("anthropic/claude-sonnet-5-5") && err.contains("boom"),
            "stderr names the row and error: {err:?}"
        );

        // The failing row shows '?' in the context, max-out, thinking and images columns.
        let row: Vec<_> = text
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some("anthropic/claude-sonnet-5-5"))
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(&row[2..], &["?", "?", "?", "?"]);
    }
}

#[cfg(test)]
mod session_scratch_tests {
    use super::*;
    use oxa_proto::ExecSpec;
    use oxa_sandbox::{Layout, NetSandbox};

    /// The host directory a session's sandbox mounts at `/tmp` (`None` = a fresh tmpfs).
    fn tmp_dir(d: &Deps, cwd: &Path) -> Option<PathBuf> {
        let spec = ExecSpec {
            command: "true".into(),
            cwd: cwd.to_path_buf(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        };
        let a = d.sandbox.bwrap_args(&spec);
        a.windows(3)
            .find(|w| w[0] == "--bind" && w[2] == "/tmp")
            .map(|w| PathBuf::from(&w[1]))
    }

    /// #457 (Codex P2): every session the process opens (the first one, `/new`, `/resume`,
    /// fork and clone all go through `Built::deps`) gets its own scratch directory, and the
    /// directory is deleted when that session's dependencies are dropped. The process sandbox
    /// itself (exec hooks) keeps the per-command tmpfs.
    #[test]
    fn each_session_gets_its_own_scratch_and_drops_it() {
        let env = oxa_fake::TestEnv::new();
        let home = env.home();
        let root = oxa_sandbox::scratch_root(&home);
        let bwrap = Bwrap {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            home: home.clone(),
            layout: Layout {
                scratch_root: Some(root.clone()),
                ..Layout::default()
            },
        };
        let base: Arc<dyn Sandbox> = Arc::new(NetSandbox::new(Arc::new(bwrap), true, false));
        let yolo = oxa_proto::YoloSwitch::new(oxa_proto::Autonomy::Standard);
        let built = Built {
            output_schema: None,
            engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
            store: Arc::new(MemStore::new()),
            sandbox: base.clone(),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            resources: Resources::default(),
            selection: ToolSelection::default(),
            model: None,
            thinking: None,
            mode: None,
            supervised_child: false,
            ask_owner_present: true,
            hook_sets: HookSets::default(),
            bash_sandbox: Arc::new(oxa_sandbox::FullAccess::new(
                base.clone(),
                yolo.clone(),
                "",
                home,
                false,
            )),
            yolo,
        };
        let first = built.deps(None);
        let second = built.deps(None);
        let a = tmp_dir(&first, &env.work()).expect("first session has a scratch directory");
        let b = tmp_dir(&second, &env.work()).expect("second session has a scratch directory");
        assert_ne!(a, b, "two sessions share one /tmp");
        assert!(a.starts_with(&root) && b.starts_with(&root), "{a:?} {b:?}");
        std::fs::write(a.join("left-by-first"), "x").unwrap();
        assert!(!b.join("left-by-first").exists());
        assert_eq!(
            first.sandbox.persistence_notice(),
            Some(oxa_sandbox::SCRATCH_NOTICE)
        );
        assert!(base
            .persistence_notice()
            .unwrap()
            .contains("discarded after each command"));
        // #597: the model names that directory as `/tmp`; the file tools read it unasked, the
        // other session's directory still asks.
        use oxa_proto::{Access, Decision, Mode};
        let cwd = env.work();
        assert_eq!(
            first.sandbox.scratch_dir(),
            Some(a.as_path()),
            "the file tools and the sandbox name one directory"
        );
        assert_eq!(
            first
                .policy
                .decide(Mode::Workspace, &Access::Read(a.join("left-by-first")), &cwd),
            Decision::Allow
        );
        assert_eq!(
            first
                .policy
                .decide(Mode::Workspace, &Access::Read(b.join("left-by-first")), &cwd),
            Decision::Ask
        );
        drop(first);
        assert!(!a.exists(), "the first session's scratch outlived it");
        assert!(b.is_dir(), "ending one session removed another's scratch");
        drop(second);
        assert!(!b.exists(), "the second session's scratch outlived it");
    }
}
