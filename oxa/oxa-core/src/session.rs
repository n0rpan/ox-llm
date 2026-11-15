//! C1: `Session`, the SDK object. Owner: task C1. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/agent-session.ts
use crate::agent_loop::{persist, run_loop, RunEnd};
use crate::host::{attach_hosts, first_reply, observe_hooks, pumped, serve_host, serve_idle, Serve};
use crate::{Expanded, Handle, SessionState, Shared, Toolset};
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, Settings};
use oxa_proto::{
    codes, image_placeholder, names, AgentEvent, CommandSpec, CompactReason, Engine, EngineInfo, Entry,
    EntryBody, EventSink, FrozenPrefix, HeadlessUi, HookPoint, HookReply, Hooks, Level, Mode, OxaError, Part,
    Policy, Role, Sandbox, ThinkingLevel, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_session::{MemStore, SessionRef, SessionStore, Transcript, Tree};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// What a session is built from. `tools` are the built-ins to offer (normally
/// `oxa_tools::builtin_tools()`); hook tools are added from `hooks` at each epoch.
pub struct Deps {
    pub engine: Arc<dyn Engine>,
    pub store: Arc<dyn SessionStore>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub hooks: Vec<Arc<dyn Hooks>>,
    pub policy: Box<dyn Policy>,
    pub sandbox: Arc<dyn Sandbox>,
    pub settings: Settings,
    pub dirs: Dirs,
    pub cwd: PathBuf,
    /// CLI-level choices that shape the first epoch.
    pub selection: crate::ToolSelection,
    pub resources: oxa_resources::Resources,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub mode: Option<Mode>,
    /// `--session-id`.
    pub session_id: Option<String>,
}

/// How to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Open {
    New,
    /// `-c`: the most recent session of the cwd, or `New` when there is none.
    Continue,
    Resume(SessionRef),
    /// `--fork`: copy the active branch of the given session into a new one.
    Fork(SessionRef),
    /// `--no-session`: nothing is written (the store should be a `MemStore`).
    Ephemeral,
}

/// The RPC `get_state` data.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub session_name: Option<String>,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub mode: Mode,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: oxa_config::QueueMode,
    pub follow_up_mode: oxa_config::QueueMode,
    pub auto_compaction: bool,
    pub auto_retry: bool,
    pub message_count: u32,
    pub pending_message_count: u32,
    pub leaf: String,
    pub prefix_sha256: [u8; 32],
    pub n_ctx: u32,
    /// `oxa.profile` of the epoch in force.
    pub profile: oxa_proto::Profile,
}

/// The RPC `get_session_stats` data plus cache accounting.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    pub user_messages: u32,
    pub assistant_messages: u32,
    pub tool_calls: u32,
    pub tool_results: u32,
    /// Sums over all chat requests of the process.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// Current context size (`Transcript::context`).
    pub context_tokens: u32,
    pub requests: u32,
    pub cache_misses: u32,
    pub compactions: u32,
    pub epochs: u32,
    /// Tokens per second of the last reply (`predicted_n / predicted_ms`).
    pub last_tok_s: f64,
    /// T2: repair messages appended for malformed tool calls on the active branch.
    pub tool_repairs: u32,
    /// F11: the token position of the last checkpoint the engine reported kept for an H4
    /// request (`timings.snap_pos`) of this process; `None` = none reported yet.
    pub last_snap_pos: Option<u32>,
    /// `oxa.profile` of the epoch in force (read once per epoch, stored with it).
    pub profile: oxa_proto::Profile,
    /// B6: the engine sent a reply without `cached_tokens` (the notice was shown).
    pub cached_unreported: bool,
    /// Adaptive thinking on tool follow-ups (`think.rs`) of the current run. Never persisted.
    pub escalation: crate::think::Escalation,
}

/// A running session.
pub struct Session {
    st: SessionState,
    /// O-152: the boot preload ran (or was skipped) for the session entered last.
    preloaded: bool,
}

/// The CLI-level choices of [`Session::open`]. `None` in [`enter`] means a switch inside a
/// running process: the toolset in force is kept and the session's own settings are restored.
struct Cli {
    builtins: Vec<Arc<dyn Tool>>,
    selection: crate::ToolSelection,
    session_id: Option<String>,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    mode: Option<Mode>,
}

/// A1/A2: a command / shortcut handler of a hook set that panics fails that one command.
fn contained_handler(run: impl FnOnce() -> Result<(), OxaError>) -> Result<(), OxaError> {
    oxa_proto::panic::contain(run).unwrap_or_else(|error| {
        Err(OxaError::new(
            codes::INTERNAL,
            format!("the extension handler failed with an internal error (panic): {error}"),
        ))
    })
}

/// A1/A2: a panic escaped a run (a bug in oxa, not in a tool or an extension: those are
/// contained where they run). The calls of the last assistant message that have no result get
/// the interrupted result (as on a resume after a crash), so the next request is well-formed;
/// the busy flag is released by the run's guard. The error names the panic.
fn recover_from_panic(st: &mut SessionState, error: &str, sink: &mut dyn EventSink) -> OxaError {
    let _ = crate::resume::close_interrupted(st, sink);
    OxaError::new(
        codes::INTERNAL,
        format!("internal error (panic): {error}; the run was stopped and the session continues"),
    )
}

/// Marks a run in progress. Dropping it clears `busy`, so every exit path releases the session.
struct BusyGuard(Arc<Shared>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

impl Session {
    /// Open per `how`. New / Fork without a stored prefix: fire `ResourcesDiscover` and
    /// `SessionStart`, attach the host to every hook set, read `Engine::info`, build the first
    /// epoch (`epoch::open_epoch`). Resume / Continue: replay the stored prefix bytes
    /// (`epoch::replay_prefix`), fold the active branch, restore model / thinking / mode from
    /// the branch's entries; a missing cwd is `not_found` with code hint `missing_cwd` (the
    /// caller offers Continue/Fork/Cancel). Engine down = `engine_down`.
    pub fn open(d: Deps, how: Open) -> Result<Session, OxaError> {
        let Deps {
            engine,
            store,
            tools,
            hooks,
            policy,
            sandbox,
            settings,
            dirs,
            cwd,
            selection,
            resources,
            model,
            thinking,
            mode,
            session_id,
        } = d;
        // H: `defaultModel` (pi settings.md "Startup model ID") when no model was chosen and the
        // engine serves it; otherwise the engine's own model, as before.
        let model = match model {
            Some(m) => Some(m),
            None => match settings.string("defaultModel").filter(|m| !m.is_empty()) {
                Some(wanted) => {
                    let base = engine.info(None)?;
                    (base.model == wanted || base.models.contains(&wanted)).then_some(wanted)
                }
                None => None,
            },
        };
        // X3: the limits of the model this session will use (the `--model` target when given,
        // loaded or not), so the first request already fits it.
        let info = engine.info(model.as_deref())?;
        // H: the startup thinking level (pi sdk.ts): the CLI choice, else the setting for this
        // model (`modelThinkingLevels`), else `defaultThinkingLevel` (default `medium`). A
        // resumed session restores its own recorded level (`restore_settings`).
        // MOD-004: a model that does not keep its reasoning (`preserve_thinking: false`, Flash)
        // starts with thinking off unless the CLI asks for it.
        let startup_thinking = if info.caps.preserve_thinking == Some(false) {
            ThinkingLevel::Off
        } else {
            settings.startup_thinking(model.as_deref().unwrap_or(&info.model))
        };
        let startup_mode = settings
            .string("oxa.mode")
            .as_deref()
            .and_then(Mode::parse)
            .unwrap_or(Mode::Workspace);
        let compact = policy_for_model(
            &info,
            &settings,
            model.as_deref().unwrap_or(&info.model),
            settings.profile(),
        );
        let auto_compaction = compact.enabled;
        let auto_retry = settings.retry().enabled;
        let shared = Arc::new(Shared::new(settings.steering_mode(), settings.follow_up_mode()));
        let cli = Cli {
            builtins: tools.clone(),
            selection,
            session_id,
            model: model.clone(),
            thinking,
            mode,
        };
        let mut st = SessionState {
            engine,
            store,
            hooks,
            policy,
            sandbox,
            settings,
            dirs,
            cwd,
            all_tools: tools,
            selection: cli.selection.clone(),
            active: Toolset::default(),
            resources,
            sref: SessionRef {
                path: PathBuf::new(),
                id: String::new(),
            },
            tree: Tree::default(),
            transcript: Transcript::default(),
            prefix: empty_prefix(),
            model: model.unwrap_or_else(|| info.model.clone()),
            thinking: thinking.unwrap_or(startup_thinking),
            // `--mode` / `--plan` first, then the `oxa.mode` setting (default `workspace`).
            mode: mode.unwrap_or(startup_mode),
            info,
            compact,
            shared,
            next_turn: Vec::new(),
            last_usage: None,
            stats: Stats::default(),
            spill_dir: PathBuf::new(),
            tool_env: Vec::new(),
            tune: Default::default(),
            auto_compaction,
            auto_retry,
            host_rx: None,
            host_deferred: Vec::new(),
            compact_memo: Default::default(),
        };
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        enter(&mut st, how, Some(&cli), &mut sink, &mut ui)?;
        Ok(Session { st, preloaded: false })
    }

    /// Run one user prompt to settlement (blocks). A line naming an extension command runs it
    /// as [`Session::run_command`] does (pi). Order: `expand_input` (templates, skills;
    /// built-in slash commands are the caller's business) -> hook `Input` -> append pending `NextTurn`
    /// messages and the user message (one `append_entry` each; image parts add
    /// `image_placeholder` lines) -> hook `BeforeAgentStart` -> `run_loop` -> follow-ups ->
    /// `maybe_compact` -> `AgentSettled`, emitted after the session is released. While it
    /// runs, steering / follow-ups / abort arrive through [`Handle`]. `busy` if called
    /// re-entrantly.
    pub fn prompt(
        &mut self,
        text: String,
        parts: Vec<Part>,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<(), OxaError> {
        // pi (agent-session.ts `prompt`): a line naming an extension command runs that command
        // instead of a prompt; no run starts unless the command triggers one.
        if let Expanded::Hook { name, args } = crate::expand_input(&self.st, &text) {
            return self.run_command(&name, &args, sink, ui);
        }
        let guard = self.begin()?;
        let result = oxa_proto::panic::contain(|| self.run_prompt(text, parts, sink, ui))
            .unwrap_or_else(|error| Err(recover_from_panic(&mut self.st, &error, sink)));
        let mut aborted = matches!(result, Ok(Some(RunEnd::Aborted)));
        // E-INT 1: the run is over; the session-shape actions hooks asked for are applied now,
        // while the session is still claimed, and the turns they triggered run.
        let can_run = matches!(result, Ok(None) | Ok(Some(RunEnd::Stopped)));
        let triggered = settle_triggered(&mut self.st, can_run, sink, ui);
        if let Ok(Some(end)) = &triggered {
            aborted = *end == RunEnd::Aborted;
        }
        // Wave H: pi's `agent_before_settle` boundary, then the runs it asked for.
        let last = last_end(&result, &triggered);
        let boundary = crate::fire::before_settle(&mut self.st, last.clone(), sink, ui);
        if let Ok(end) = &boundary {
            if *end != last {
                aborted = *end == Some(RunEnd::Aborted);
            }
        }
        flush_queue(&self.st, sink);
        // The `agent_settled` hook observes the settled run while the session is still claimed;
        // the `AgentSettled` record is the last thing a prompt does: the claim is released
        // first, so a command sent as soon as a client sees the record finds the session idle.
        crate::fire::agent_settled(&mut self.st, aborted, sink, ui);
        drop(guard);
        sink.emit(&AgentEvent::AgentSettled { aborted });
        result.and(triggered).and(boundary).map(|_| ())
    }

    pub fn handle(&self) -> Handle {
        Handle {
            shared: self.st.shared.clone(),
        }
    }

    /// S1 / ENG-002: run the steering and follow-up messages that are queued, as one run, when
    /// the session is idle; `Ok(None)` when nothing is queued. A front end calls it after an
    /// operation that is not a prompt (a navigation, `/compact`), because input typed during
    /// that operation's background request cancels the request and waits in the queue.
    /// `Session::prompt` does the same itself before it returns. `busy` during a run.
    pub fn run_queued(
        &mut self,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<Option<RunEnd>, OxaError> {
        let guard = self.begin()?;
        let ran = oxa_proto::panic::contain(|| queued_run(&mut self.st, sink, ui, true))
            .unwrap_or_else(|error| Err(recover_from_panic(&mut self.st, &error, sink)));
        let result = match ran {
            Ok(Some(end)) => settle_runs(&mut self.st, end, sink, ui).map(Some),
            other => other,
        };
        // E-INT 1: as at the end of a prompt, what hook sets asked for meanwhile is applied
        // and the turns they triggered run, while the session is still claimed.
        let can_run = matches!(result, Ok(None) | Ok(Some(RunEnd::Stopped)));
        let triggered = settle_triggered(&mut self.st, can_run, sink, ui);
        let last = last_end(&result, &triggered);
        let boundary = crate::fire::before_settle(&mut self.st, last.clone(), sink, ui);
        flush_queue(&self.st, sink);
        let ran = match (&boundary, &last) {
            (Ok(Some(end)), _) | (Err(_), Some(end)) => Some(*end == RunEnd::Aborted),
            _ => None,
        };
        if let Some(aborted) = ran {
            crate::fire::agent_settled(&mut self.st, aborted, sink, ui);
        }
        drop(guard);
        emit_settled(sink, ran);
        let result = result?;
        let triggered = triggered?;
        let boundary = boundary?;
        Ok(if boundary != last {
            boundary
        } else {
            triggered.or(result)
        })
    }

    /// `/compact [instructions]` (reason `Manual`).
    pub fn compact(
        &mut self,
        instructions: Option<&str>,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<(), OxaError> {
        let _guard = self.begin()?;
        let result = crate::compact_now(&mut self.st, instructions, CompactReason::Manual, sink, ui);
        serve_host(&mut self.st, true, sink, ui);
        result
    }

    /// `/tree`: move the leaf to `target` (one rebase), optionally summarising the abandoned
    /// branch. Returns the text to refill the editor with when `target` is a user message.
    pub fn navigate(
        &mut self,
        target: &str,
        summarize: bool,
        instructions: Option<&str>,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<Option<String>, OxaError> {
        let _guard = self.begin()?;
        crate::navigate(&mut self.st, target, summarize, instructions, sink, ui)
    }

    /// `/fork`: new session file from the branch ending at `entry`; this session switches to it.
    pub fn fork(&mut self, entry: &str) -> Result<SessionRef, OxaError> {
        let _guard = self.begin()?;
        let mut ui = HeadlessUi::default();
        crate::fork_at(&mut self.st, entry, &mut ui)
    }

    /// `/clone`: copy the active branch into a new session and switch to it.
    pub fn clone_session(&mut self) -> Result<SessionRef, OxaError> {
        let _guard = self.begin()?;
        let mut ui = HeadlessUi::default();
        crate::clone_current(&mut self.st, &mut ui)
    }

    /// Switch this object to another session (`/resume`, `/new` with `Open::New`). Fires
    /// `SessionBeforeSwitch` (cancellable) and re-attaches hosts.
    pub fn switch(&mut self, how: Open, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Result<(), OxaError> {
        self.ensure_idle()?;
        let payload = crate::fire::before_switch_payload(&how);
        if let HookReply::Cancel(reason) =
            first_reply(&mut self.st, HookPoint::SessionBeforeSwitch, &payload, sink, ui)
        {
            return Err(OxaError::new(codes::ABORTED, reason));
        }
        let target = payload["targetSessionFile"].as_str().map(str::to_string);
        let reason = payload["reason"].as_str().unwrap_or("new");
        crate::fire::session_shutdown(&mut self.st, reason, target.as_deref(), sink, ui);
        enter(&mut self.st, how, None, sink, ui)?;
        self.preloaded = false;
        Ok(())
    }

    /// Model switch = a new epoch (W3), applied now when idle. Appends `ModelChange`, then
    /// re-reads the engine's metadata for the target and recomputes the compaction policy (X3,
    /// see [`refresh_budget`]) before the epoch opens.
    pub fn set_model(&mut self, m: &str, sink: &mut dyn EventSink) -> Result<(), OxaError> {
        self.ensure_idle()?;
        switch_model(
            &mut self.st,
            m,
            OverCap::CompactFirst,
            sink,
            &mut oxa_proto::HeadlessUi::default(),
        )
    }

    /// [`Session::set_model`] with the over-cap rule chosen by the caller and the caller's UI
    /// (hooks of a source-model compaction may open dialogs).
    pub fn set_model_with(
        &mut self,
        m: &str,
        over: OverCap,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<(), OxaError> {
        self.ensure_idle()?;
        switch_model(&mut self.st, m, over, sink, ui)
    }

    /// What a switch to `m` would cost, read without changing anything (S3 / ENG-004).
    pub fn plan_model_switch(&self, m: &str) -> SwitchPlan {
        plan_switch(&self.st, m)
    }

    /// Levels are clamped to the model (pi's rule). Appends `ThinkingLevelChange`. Not an epoch.
    pub fn set_thinking(&mut self, l: ThinkingLevel, sink: &mut dyn EventSink) {
        match persist(&mut self.st, EntryBody::ThinkingLevelChange(l), sink) {
            Ok(_) => {
                let previous = std::mem::replace(&mut self.st.thinking, l);
                sink.emit(&AgentEvent::ThinkingLevelChanged { level: l });
                let mut ui = HeadlessUi::default();
                crate::fire::thinking_level_select(&mut self.st, l, previous, sink, &mut ui);
            }
            Err(e) => notice_error(sink, &e),
        }
    }

    /// Appends `ModeChange`. Never touches the wire or the prefix (W3: no mode text).
    pub fn set_mode(&mut self, m: Mode, sink: &mut dyn EventSink) {
        match persist(&mut self.st, EntryBody::ModeChange(m), sink) {
            Ok(_) => {
                self.st.mode = m;
                sink.emit(&AgentEvent::ModeChanged { mode: m });
            }
            Err(e) => notice_error(sink, &e),
        }
    }

    /// `/reload`: re-read resources, settings and hook tools. Returns `true` when a new epoch
    /// was opened (something in the prefix changed), `false` when nothing changed.
    pub fn reload(&mut self, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Result<bool, OxaError> {
        self.ensure_idle()?;
        crate::reload(&mut self.st, sink, ui)
    }

    /// `!cmd` (`excluded = false`) / `!!cmd` (`true`).
    pub fn user_bash(
        &mut self,
        cmd: &str,
        excluded: bool,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<(), OxaError> {
        // `crate::user_bash` claims the session itself (and resets the cancel token) and serves
        // the settled host requests before it releases the claim.
        crate::user_bash(&mut self.st, cmd, excluded, sink, ui)
    }

    /// Run a hook or template command by name (`/name args`). Built-in interactive commands are
    /// handled by the front end. `not_found` when no source has it.
    pub fn run_command(
        &mut self,
        name: &str,
        args: &str,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<(), OxaError> {
        let owner = self
            .st
            .hooks
            .iter()
            .find(|h| h.commands().iter().any(|c| c.name == name))
            .cloned();
        if let Some(hook) = owner {
            let _guard = self.begin()?;
            // E-INT 2: the handler runs beside the session thread, which serves what it asks
            // of the host. No run is in progress, so session-shape actions apply at once.
            let result = pumped(&mut self.st, Serve::Command, sink, ui, &mut |_| {}, |ui| {
                contained_handler(|| hook.run_command(name, args, ui))
            });
            report_hook_errors(hook.as_ref(), sink);
            let (triggered, settled) = settle_and_report(&mut self.st, sink, ui);
            drop(_guard);
            emit_settled(sink, settled);
            return result.and(triggered);
        }
        let line = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        match oxa_resources::expand_template_command(&self.st.resources.templates, &line) {
            Some(text) => self.prompt(text, Vec::new(), sink, ui),
            None => Err(OxaError::new(codes::NOT_FOUND, format!("no command /{name}"))),
        }
    }

    pub fn set_name(&mut self, name: &str, sink: &mut dyn EventSink) -> Result<(), OxaError> {
        let name = name.trim().to_string();
        persist(&mut self.st, EntryBody::SessionInfo { name: name.clone() }, sink)?;
        let shown = if name.is_empty() { None } else { Some(name) };
        sink.emit(&AgentEvent::SessionInfoChanged { name: shown.clone() });
        let mut ui = HeadlessUi::default();
        crate::fire::session_info_changed(&mut self.st, shown.as_deref(), sink, &mut ui);
        Ok(())
    }

    pub fn set_label(&mut self, target: &str, label: Option<&str>) -> Result<(), OxaError> {
        if self.st.tree.get(target).is_none() {
            return Err(OxaError::new(
                codes::NOT_FOUND,
                format!("entry {target} is not in this session"),
            ));
        }
        let label = label.map(str::trim).filter(|l| !l.is_empty()).map(String::from);
        crate::append_entry(
            &mut self.st,
            EntryBody::Label {
                target: target.to_string(),
                label,
            },
        )?;
        Ok(())
    }

    /// Runtime switch for threshold (after-reply) compaction. It overrides the
    /// `compaction.enabled` setting until a `/reload` changes that setting; the overflow path
    /// (`When::MidRun`) stays on either way.
    pub fn set_auto_compaction(&mut self, on: bool) {
        self.st.auto_compaction = on;
        self.st.compact.enabled = on;
    }

    pub fn set_auto_retry(&mut self, on: bool) {
        self.st.auto_retry = on;
    }

    pub fn state(&self) -> State {
        let st = &self.st;
        let (steering_mode, follow_up_mode) = match st.shared.modes.lock() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        };
        let (steering, follow_up) = st.shared.snapshot();
        let session_file = {
            let p = st.sref.path.to_string_lossy();
            if st.sref.path.as_os_str().is_empty() || p.starts_with("mem:") {
                None
            } else {
                Some(st.sref.path.clone())
            }
        };
        State {
            session_id: st.sref.id.clone(),
            session_file,
            session_name: session_name(st),
            model: st.model.clone(),
            thinking: st.thinking,
            mode: st.mode,
            is_streaming: st.shared.busy.load(Ordering::SeqCst),
            is_compacting: false,
            steering_mode,
            follow_up_mode,
            auto_compaction: st.auto_compaction,
            auto_retry: st.auto_retry,
            message_count: st.transcript.wire().len() as u32,
            pending_message_count: (steering.len() + follow_up.len() + st.next_turn.len()) as u32,
            leaf: st.tree.leaf().to_string(),
            prefix_sha256: st.prefix.sha256,
            n_ctx: st.info.n_ctx,
            profile: st.stats.profile,
        }
    }

    pub fn stats(&self) -> Stats {
        self.st.stats
    }

    pub fn commands(&self) -> Vec<CommandSpec> {
        crate::all_commands(&self.st)
    }

    /// Read access for front ends (tree view, export, RPC `get_entries`).
    pub fn inner(&self) -> &SessionState {
        &self.st
    }

    /// Leave this session without stopping the hook sets: fire `SessionShutdown` and drop the
    /// host. For a front end that replaces the session object by another one built from the
    /// same hook sets (`/new`, `/resume`, fork): the new session attaches its own host.
    pub fn close(&mut self, ui: &mut dyn Ui) {
        let mut sink = VecSink::default();
        observe_hooks(
            &mut self.st,
            HookPoint::SessionShutdown,
            &serde_json::json!({ "reason": "quit" }),
            &mut sink,
            ui,
        );
        self.st.host_rx = None;
        self.st.host_deferred.clear();
    }

    /// Fire `SessionShutdown`, shut hook sets down. Called once before exit.
    pub fn shutdown(&mut self, ui: &mut dyn Ui) {
        self.close(ui);
        for h in &self.st.hooks {
            h.shutdown();
        }
    }

    /// O-152: the boot preload ([`crate::boot_preload`]), at most once per entered session and
    /// only while its wire is empty. Front ends call it from their idle tick before the first
    /// prompt; it claims the session like a run, so a busy session is skipped (`false`).
    pub fn preload_prefix(&mut self, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> bool {
        if self.preloaded {
            return false;
        }
        // Requests hook sets made while idle (a triggered turn) are served first: the preload
        // is not a run, and serving them inside its hook calls would turn a triggered turn
        // into a queued message with no run to take it. It is tried again on the next tick
        // (and skipped there once the triggered turn has filled the wire).
        if crate::host::host_pending(&mut self.st) {
            return false;
        }
        self.preloaded = true;
        let Ok(_guard) = self.begin() else {
            return false;
        };
        // A turn an extension triggers while the preload request runs waits for the next idle
        // tick (`serve_host`) instead of being queued into a run that never comes.
        crate::host::holding_triggers(|| crate::compaction::boot_preload(&mut self.st, sink, ui))
    }

    /// V020-OPTIMIZE #13 (`oxa.prefill.paste`): the least bytes of a draft's user message that
    /// is prefilled.
    pub const DRAFT_PREFILL_MIN_BYTES: usize = 2000;
    /// V020-OPTIMIZE #13: how long a draft stays unchanged before a front end prefills it.
    pub const DRAFT_PREFILL_QUIET: std::time::Duration = std::time::Duration::from_millis(700);
    /// OXA-LOCAL-FIRST F1 (`oxa.prefill.tools`): see `exec::TOOL_PREFILL_AFTER`.
    pub const TOOL_PREFILL_AFTER: std::time::Duration = crate::exec::TOOL_PREFILL_AFTER;
    /// OXA-LOCAL-FIRST F1: see `exec::TOOL_PREFILL_MIN_BYTES`.
    pub const TOOL_PREFILL_MIN_BYTES: usize = crate::exec::TOOL_PREFILL_MIN_BYTES;

    /// Tests only: how long a call runs before its tool-result prefill is sent, in this
    /// process (default [`Session::TOOL_PREFILL_AFTER`]).
    #[doc(hidden)]
    pub fn set_tool_prefill_after_for_tests(after: std::time::Duration) {
        crate::exec::set_tool_prefill_after_for_tests(after);
    }

    /// V020-OPTIMIZE #13 (`oxa.prefill.paste`, on under profile `local`): while the user is
    /// still composing, prefill the user message that `prompt(text, parts)` would append now,
    /// in one background zero-token request over `wire + [that message]` (the draft prefill
    /// of `compaction.rs`). The message is built by the functions the
    /// prompt itself uses (`expand_input`, image placeholders), so a prompt sent with the same
    /// draft repeats those bytes exactly and finds them cached. Timing only: nothing is pushed,
    /// stored or recorded.
    ///
    /// Interactive front ends call it from their idle tick once the draft has been unchanged
    /// for [`Session::DRAFT_PREFILL_QUIET`], once per distinct draft; `text` is exactly what
    /// they would pass to [`Session::prompt`]. Skipped (`false`, no request) when the setting
    /// is off, the engine does not advertise zero-token prefill, `cacheWarming` is `off`, the
    /// message is under [`Session::DRAFT_PREFILL_MIN_BYTES`], the line is a command or a `!`
    /// line, a hook (`input`, `before_agent_start`, `context`, `before_provider_request`)
    /// could change or block the prompt, messages for the next turn are pending, the session
    /// is busy, or the engine is not on this machine (privacy: a draft never submitted does
    /// not leave it; loopback only). It claims the session like the boot preload; user input
    /// cancels it through [`Handle::cancel_background`] (never [`Handle::abort`], which would
    /// also cancel a prompt that claims the session right after it), and
    /// `stale` (the front end's own "the draft changed" check) is asked after the cancel is
    /// armed, so a change at any moment either skips it or cancels it. Returns whether the
    /// prefill completed.
    pub fn prefill_draft(
        &mut self,
        text: &str,
        parts: &[Part],
        stale: &dyn Fn() -> bool,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> bool {
        let Some(draft) = draft_message(&mut self.st, text, parts) else {
            return false;
        };
        let Ok(_guard) = self.begin() else {
            return false;
        };
        crate::host::holding_triggers(|| {
            crate::compaction::draft_prefill(&mut self.st, draft, stale, sink, ui)
        })
    }

    /// Serve the host requests hook sets made while the session was idle (a background MCP
    /// thread, a timer of the front end). Front ends call it from their idle tick; requests
    /// are otherwise served at the start of the next operation. `busy` during a run.
    ///
    /// A turn an extension triggered (`sendMessage(.., {triggerTurn: true})`) while the session
    /// was idle runs here, to settlement, with the events of a prompt's run (then
    /// `AgentSettled`).
    /// With nothing pending it returns `Ok` at once without claiming the session, so a front
    /// end may call it on every idle tick.
    pub fn serve_host(&mut self, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Result<(), OxaError> {
        if !crate::host::host_pending(&mut self.st) {
            return Ok(());
        }
        let guard = self.begin()?;
        let (result, settled) = settle_and_report(&mut self.st, sink, ui);
        drop(guard);
        emit_settled(sink, settled);
        result
    }

    /// Run the handler a hook set bound to `key` (extension shortcuts), with the host served
    /// like a command. A key no hook set bound is `Ok`.
    pub fn run_shortcut(
        &mut self,
        key: &oxa_proto::KeyId,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<(), OxaError> {
        let owner = self
            .st
            .hooks
            .iter()
            .find(|h| h.shortcuts().iter().any(|s| &s.key == key))
            .cloned();
        let Some(hook) = owner else {
            return Ok(());
        };
        let _guard = self.begin()?;
        let result = pumped(&mut self.st, Serve::Command, sink, ui, &mut |_| {}, |ui| {
            contained_handler(|| hook.run_shortcut(key, ui))
        });
        report_hook_errors(hook.as_ref(), sink);
        let (triggered, settled) = settle_and_report(&mut self.st, sink, ui);
        drop(_guard);
        emit_settled(sink, settled);
        result.and(triggered)
    }

    /// `busy` while a run is in progress. Operations that open an epoch check this instead of
    /// claiming the session, because `open_epoch` itself requires an idle session (W3).
    fn ensure_idle(&self) -> Result<(), OxaError> {
        if self.st.shared.busy.load(Ordering::SeqCst) {
            return Err(OxaError::new(
                codes::BUSY,
                "a run is in progress; the prefix changes only between runs",
            ));
        }
        Ok(())
    }

    /// Claim the session for one operation. Fails with `busy` when another one holds it. The
    /// cancel tokens are cleared only after the claim, so an abort aimed at the running prompt
    /// is never lost to a later reset.
    fn begin(&self) -> Result<BusyGuard, OxaError> {
        let shared = self.st.shared.clone();
        if shared
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(OxaError::new(codes::BUSY, "the session is busy"));
        }
        shared.cancel.reset();
        shared.cancel_retry.reset();
        shared.gate.bg_cancel.reset();
        shared.gate.bg_abort.store(false, Ordering::SeqCst);
        if shared.gate.abort_next.swap(false, Ordering::SeqCst) {
            // The front end accepted this run and was asked to abort it before it began.
            shared.cancel.cancel();
            shared.gate.bg_cancel.cancel();
        }
        Ok(BusyGuard(shared))
    }

    /// The body of [`Session::prompt`] after the session is claimed. `Ok(None)` when no run
    /// was started (handled or blocked by a hook).
    fn run_prompt(
        &mut self,
        text: String,
        parts: Vec<Part>,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) -> Result<Option<RunEnd>, OxaError> {
        let st = &mut self.st;
        // E-INT 1: what hook sets asked for while the session was idle is applied first. The
        // `NextTurn` messages held at this point belong to this prompt; one queued while this
        // prompt runs (also by its own `input` hook) waits for the next prompt.
        serve_host(st, false, sink, ui);
        let next_turn = std::mem::take(&mut st.next_turn);
        let mut text = prompt_text(st, text)?;

        match first_reply(
            st,
            HookPoint::Input,
            &crate::fire::input_payload(&text, &parts),
            sink,
            ui,
        ) {
            HookReply::Handled => return Ok(None),
            HookReply::ReplaceInput(replaced) => text = replaced,
            HookReply::Block(reason) | HookReply::Cancel(reason) => {
                notice(sink, Level::Warning, reason);
                return Ok(None);
            }
            _ => {}
        }

        // L5 (`oxa.compaction.turnStart`): an aborted turn left its tool result uncounted; the
        // prompt is compacted first when the context is already above the soft limit.
        crate::compaction::compact_at_turn_start(st, sink, ui);
        for wire in next_turn {
            persist(
                st,
                EntryBody::Msg {
                    wire,
                    parts: Vec::new(),
                },
                sink,
            )?;
        }
        let images = crate::fire::images_json(&parts);
        let (wire, note) = crate::envnote::add_note(st, user_message(&text, &parts));
        // The note's provenance is stored with the entry (refill and display strip exactly it).
        let mut parts = parts;
        parts.extend(note.map(Part::EnvNote));
        st.stats.user_messages += 1;
        persist(st, EntryBody::Msg { wire, parts }, sink)?;

        let payload = crate::fire::before_agent_start_payload(st, &text, images);
        match first_reply(st, HookPoint::BeforeAgentStart, &payload, sink, ui) {
            HookReply::Block(reason) | HookReply::Cancel(reason) => {
                notice(sink, Level::Warning, reason);
                return Ok(None);
            }
            // Messages a hook adds for this run: appended after the user message, each as a
            // hidden custom message (the fold replays it as one user message).
            HookReply::Append(msgs) => {
                for m in msgs {
                    if let Some(content) = m.content {
                        persist(
                            st,
                            EntryBody::CustomMessage {
                                custom_type: "before_agent_start".to_string(),
                                content,
                                display: false,
                                details: Value::Null,
                            },
                            sink,
                        )?;
                    }
                }
            }
            _ => {}
        }

        run_from_wire(st, sink, ui).map(Some)
    }
}

/// Settle a claimed session (see [`serve_idle`]): apply what hook sets asked for and, when
/// `can_run` (the operation before did not abort or fail), run the turns they triggered, one
/// after the other, until none is pending or one is aborted. `Ok(Some(end))` = the end of the
/// last triggered run; `Ok(None)` = none ran.
fn settle_triggered(
    st: &mut SessionState,
    can_run: bool,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Option<RunEnd>, OxaError> {
    if !can_run {
        serve_host(st, true, sink, ui);
        return Ok(None);
    }
    let mut last = None;
    while serve_idle(st, sink, ui) {
        crate::fire::triggered_start(st, sink, ui)?;
        let end = run_from_wire(st, sink, ui)?;
        let stop = end != RunEnd::Stopped;
        last = Some(end);
        if stop {
            serve_host(st, true, sink, ui);
            break;
        }
    }
    Ok(last)
}

/// [`settle_triggered`] for an operation that is not a prompt (a command or shortcut handler,
/// the idle tick): when a triggered turn ran, the queue is flushed and the `AgentSettled` to
/// send is returned (`Some(aborted)`). The caller sends it with [`emit_settled`] after it has
/// released its claim, as at the end of a prompt.
fn settle_and_report(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> (Result<(), OxaError>, Option<bool>) {
    let triggered = settle_triggered(st, true, sink, ui);
    let triggered = match triggered {
        Ok(last) => crate::fire::before_settle(st, last, sink, ui),
        Err(e) => Err(e),
    };
    let ran = match &triggered {
        Ok(Some(end)) => Some(*end == RunEnd::Aborted),
        Ok(None) => None,
        Err(_) => Some(false),
    };
    if ran.is_some() {
        flush_queue(st, sink);
        if let Some(aborted) = ran {
            crate::fire::agent_settled(st, aborted, sink, ui);
        }
    }
    (triggered.map(|_| ()), ran)
}

/// The handler failures a hook set recorded become `ExtensionError` events (the `Hooks`
/// contract; pi's `extension_error` record).
pub(crate) fn report_hook_errors(h: &dyn Hooks, sink: &mut dyn EventSink) {
    for e in h.take_errors() {
        sink.emit(&AgentEvent::ExtensionError {
            extension_path: e.extension_path,
            event: e.event,
            error: e.error,
        });
    }
}

/// The closing `AgentSettled` of an operation that ran turns (`Some(aborted)`). Called after
/// the operation's claim on the session is released, so the record is the last thing it does.
fn emit_settled(sink: &mut dyn EventSink, settled: Option<bool>) {
    if let Some(aborted) = settled {
        sink.emit(&AgentEvent::AgentSettled { aborted });
    }
}

/// The end of the last run of an operation: a triggered turn's, else the prompt's own.
fn last_end(
    result: &Result<Option<RunEnd>, OxaError>,
    triggered: &Result<Option<RunEnd>, OxaError>,
) -> Option<RunEnd> {
    match (result, triggered) {
        (_, Ok(Some(end))) | (Ok(Some(end)), Ok(None)) => Some(end.clone()),
        _ => None,
    }
}

/// Wave H (`agent_before_settle`): run what is waiting once a run ended: the queued steering
/// and follow-up messages, else the turns hook sets triggered while the session settled.
/// `None` when nothing was waiting.
pub(crate) fn run_pending(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Option<RunEnd>, OxaError> {
    if let Some(end) = queued_run(st, sink, ui, true)? {
        return settle_runs(st, end, sink, ui).map(Some);
    }
    if serve_idle(st, sink, ui) {
        crate::fire::triggered_start(st, sink, ui)?;
        return run_from_wire(st, sink, ui).map(Some);
    }
    Ok(None)
}

/// The run of a prompt once its messages are on the wire (also a triggered turn): `run_loop`,
/// follow-ups, the after-reply compaction decision.
pub(crate) fn run_from_wire(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<RunEnd, OxaError> {
    // X3: while the engine has not yet published metadata for the selected model (a switch
    // to a model that is not loaded, a resumed session), re-read it before the run and
    // again before the after-reply compaction decision.
    if !budget_confirmed(st) {
        refresh_budget(st, None, sink);
    }
    let end = run_loop(st, sink, ui)?;
    if !budget_confirmed(st) {
        refresh_budget(st, None, sink);
    }
    settle_runs(st, end, sink, ui)
}

/// After a run: follow-ups, the after-reply compaction decision, and then input that arrived
/// while that compaction (or its idle prefill) ran, until the queues are empty (S1 / ENG-002:
/// that input cancelled the background request; a cancelled threshold summary is decided
/// again after the next run). Stops at an abort or a failed run.
fn settle_runs(
    st: &mut SessionState,
    mut end: RunEnd,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<RunEnd, OxaError> {
    loop {
        while end == RunEnd::Stopped {
            let follow = st.shared.take_follow_up();
            if follow.is_empty() {
                break;
            }
            flush_queue(st, sink);
            for msg in follow {
                push_user(st, msg, sink)?;
            }
            end = run_loop(st, sink, ui)?;
        }
        if end != RunEnd::Stopped {
            return Ok(end);
        }
        crate::maybe_compact(st, true, sink, ui)?;
        if st.shared.cancel.is_cancelled() {
            return Ok(RunEnd::Aborted);
        }
        match queued_run(st, sink, ui, false)? {
            Some(next) => end = next,
            None => return Ok(end),
        }
    }
}

/// Append the queued steering messages (by mode) and follow-ups (by mode) as user messages and
/// run once. `None` when nothing was queued. `turn_start` (L5): the queued messages start a run
/// of their own, so a turn-start compaction may come first; `settle_runs` passes `false`, since
/// the after-reply decision has just been taken.
fn queued_run(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    turn_start: bool,
) -> Result<Option<RunEnd>, OxaError> {
    let mut taken = st.shared.take_steering();
    taken.extend(st.shared.take_follow_up());
    if taken.is_empty() {
        return Ok(None);
    }
    if turn_start {
        crate::compaction::compact_at_turn_start(st, sink, ui);
    }
    flush_queue(st, sink);
    for msg in taken {
        push_user(st, msg, sink)?;
    }
    run_loop(st, sink, ui).map(Some)
}

/// One queued message as a user message.
fn push_user(st: &mut SessionState, msg: String, sink: &mut dyn EventSink) -> Result<(), OxaError> {
    st.stats.user_messages += 1;
    persist(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(msg.clone()),
            parts: vec![Part::Text(msg)],
        },
        sink,
    )?;
    Ok(())
}

/// Bring `st` onto the session `how` names: a new file, or an existing one restored from its
/// entries. `cli` is `Some` when opening and `None` when switching inside a process.
fn enter(
    st: &mut SessionState,
    how: Open,
    cli: Option<&Cli>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    if matches!(how, Open::Ephemeral) {
        st.store = Arc::new(MemStore::new());
    }
    // pi's `session_start` reason, and the session left behind on a switch.
    let start = crate::fire::SessionStart {
        reason: match (&how, cli.is_some()) {
            (_, true) => "startup",
            (Open::New | Open::Ephemeral, false) => "new",
            (Open::Fork(_), false) => "fork",
            _ => "resume",
        },
        previous: cli.is_none().then(|| st.sref.path.display().to_string()),
    };
    let target = match how {
        Open::New | Open::Ephemeral => None,
        Open::Continue => latest_of(&*st.store, &st.cwd),
        Open::Resume(sref) => Some(sref),
        // pi `--fork` (`SessionManager.forkFrom`): the whole file, a new id (`--session-id`).
        Open::Fork(src) => {
            let id = cli.and_then(|c| c.session_id.as_deref());
            Some(oxa_session::fork_file(&*st.store, &src, &st.cwd, id)?)
        }
    };
    match target {
        None => start_new(st, cli.and_then(|c| c.session_id.as_deref()), &start, sink, ui),
        Some(sref) => restore(st, sref, cli, &start, sink, ui),
    }
}

/// A new session file: the prefix epoch first (W3), then the settings entries that let a
/// resume restore model, thinking and mode.
fn start_new(
    st: &mut SessionState,
    session_id: Option<&str>,
    start: &crate::fire::SessionStart,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let sref = st.store.create(&st.cwd, session_id, None)?;
    st.store.acquire(&sref)?;
    st.store.commit_lock();
    st.sref = sref;
    st.tree = Tree::new(Vec::new());
    st.transcript = Transcript::default();
    st.stats.last_tok_s = 0.0;
    st.stats.user_messages = 0;
    st.stats.assistant_messages = 0;
    st.stats.tool_calls = 0;
    st.stats.tool_results = 0;
    st.stats.compactions = 0;
    st.stats.epochs = 0;
    st.stats.tool_repairs = 0;
    attach_ids(st);
    attach_hosts(st, sink, ui);
    crate::fire::session_start(st, start, sink, ui);
    // What `session_start` handlers asked for (e.g. `setActiveTools`) shapes the first epoch,
    // as in pi: no run is in progress, so it is applied now (wave H, lane extapi).
    crate::host::serve_level(st, Serve::Settled, sink, ui);
    crate::open_epoch(st, "session_start", true, sink)?;
    let (model, thinking, mode) = (st.model.clone(), st.thinking, st.mode);
    persist(st, EntryBody::ModelChange(model), sink)?;
    persist(st, EntryBody::ThinkingLevelChange(thinking), sink)?;
    persist(st, EntryBody::ModeChange(mode), sink)?;
    Ok(())
}

/// Restore an existing session: fold its active branch, replay its prefix (or open one when the
/// file has none), restore model / thinking / mode, and rebuild the toolset when opening.
fn restore(
    st: &mut SessionState,
    sref: SessionRef,
    cli: Option<&Cli>,
    start: &crate::fire::SessionStart,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let header = st.store.header(&sref)?;
    // One writer per session file: a session open in another oxa is refused before anything
    // of this one is read. The lock is staged; `restore_locked` commits it at the moment the
    // state adopts the target (after validation and load). A failure before that keeps the
    // session we are in locked; a failure after it leaves the target current AND locked.
    st.store.acquire(&sref)?;
    let r = restore_locked(st, sref, header, cli, start, sink, ui);
    if r.is_err() {
        st.store.abort_lock(); // a no-op once committed
    }
    r
}

fn restore_locked(
    st: &mut SessionState,
    sref: SessionRef,
    header: oxa_session::SessionHeader,
    cli: Option<&Cli>,
    start: &crate::fire::SessionStart,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    if !header.cwd.is_dir() {
        return Err(OxaError::new(
            codes::NOT_FOUND,
            format!("the session's directory {} does not exist", header.cwd.display()),
        )
        .with_hint("missing_cwd"));
    }
    let tree = Tree::new(st.store.load(&sref)?);
    let branch: Vec<Entry> = tree.branch(tree.leaf()).into_iter().cloned().collect();
    let replayed = {
        let refs: Vec<&Entry> = branch.iter().collect();
        crate::replay_prefix(&refs)
    };
    // The state adopts the target here, so the target's lock becomes the held one now: every
    // later step (open_epoch, close_interrupted) may fail, and the state must not then point
    // at a session this process does not hold.
    st.store.commit_lock();
    st.transcript = Transcript::project(&branch);
    st.tree = tree;
    st.sref = sref;
    attach_ids(st);
    attach_hosts(st, sink, ui);
    apply_branch_counts(&mut st.stats, &branch);
    st.stats.context_tokens = st.transcript.context();
    // The profile this session keeps: the branch's when its prefix is replayed (a session keeps
    // its epoch's profile until a new epoch opens), else the settings' (the epoch opened below).
    let profile = if replayed.is_some() {
        crate::profile_of_branch(&branch)
    } else {
        st.settings.profile()
    };

    if let Some(cli) = cli {
        let mut all = cli.builtins.clone();
        for h in &st.hooks {
            all.extend(h.tools());
        }
        let (mut active, warnings) = crate::resolve_toolset(&all, &cli.selection);
        crate::toolset::apply_codemode_mode(&mut active, &st.settings);
        for w in warnings {
            notice(sink, Level::Warning, format!("unknown tool in selection: {w}"));
        }
        st.all_tools = all;
        st.active = active;
    }

    match replayed {
        Some(prefix) => {
            st.prefix = prefix;
            st.stats.profile = profile;
            crate::tune::refresh(st);
            // R5 follows the branch's profile too (the policy of `open` used the settings').
            let enabled = st.compact.enabled;
            st.compact = CompactPolicy {
                enabled,
                ..policy_for_model(&st.info, &st.settings, &st.model, profile)
            };
        }
        None => {
            crate::open_epoch(st, "resume", true, sink)?;
        }
    }
    restore_settings(st, &branch, cli);
    crate::resume::close_interrupted(st, sink)?; // R1
                                                 // pi fires `session_start` (and `resources_discover`) for a resumed session too.
    crate::fire::session_start(st, start, sink, ui);
    Ok(())
}

/// The model, thinking level and mode last recorded on the branch. A CLI choice wins over the
/// recorded value; a switch (no `cli`) always takes the recorded one.
fn restore_settings(st: &mut SessionState, branch: &[Entry], cli: Option<&Cli>) {
    let mut model = None;
    let mut thinking = None;
    let mut mode = None;
    for e in branch {
        match &e.body {
            EntryBody::ModelChange(m) => model = Some(m.clone()),
            EntryBody::ThinkingLevelChange(l) => thinking = Some(*l),
            EntryBody::ModeChange(m) => mode = Some(*m),
            _ => {}
        }
    }
    if let Some(m) = model {
        if cli.is_none_or(|c| c.model.is_none()) {
            st.model = m;
        }
    }
    if let Some(l) = thinking {
        if cli.is_none_or(|c| c.thinking.is_none()) {
            st.thinking = l;
        }
    }
    if let Some(m) = mode {
        if cli.is_none_or(|c| c.mode.is_none()) {
            st.mode = m;
        }
    }
}

/// Session-level counters of a branch (process-level sums are left alone).
fn apply_branch_counts(stats: &mut Stats, branch: &[Entry]) {
    stats.user_messages = 0;
    stats.assistant_messages = 0;
    stats.tool_calls = 0;
    stats.tool_results = 0;
    stats.compactions = 0;
    stats.epochs = 0;
    stats.tool_repairs = 0;
    for e in branch {
        match &e.body {
            EntryBody::Msg { wire, .. } => match wire.role {
                Role::User => stats.user_messages += 1,
                Role::Assistant => {
                    stats.assistant_messages += 1;
                    stats.tool_calls += wire.tool_calls.len() as u32;
                }
                Role::Tool => stats.tool_results += 1,
                Role::System => {}
            },
            EntryBody::Compaction { .. } => stats.compactions += 1,
            EntryBody::Prefix(_) => stats.epochs += 1,
            EntryBody::CustomMessage { custom_type, .. } if custom_type == crate::repair::CUSTOM_TYPE => {
                stats.tool_repairs += 1
            }
            _ => {}
        }
    }
}

/// The most recently modified session of `cwd`, if any.
fn latest_of(store: &dyn SessionStore, cwd: &std::path::Path) -> Option<SessionRef> {
    store
        .list(Some(cwd))
        .into_iter()
        .max_by_key(|m| m.modified_ms)
        .map(|m| m.sref)
}

/// Point the per-session paths at the current session id.
fn attach_ids(st: &mut SessionState) {
    let id = st.sref.id.clone();
    st.spill_dir = st.dirs.tmp_dir.join(&id);
    refresh_tool_env(st);
}

/// H (ENV-002, ENV-003): the environment of shell tools, as pi's environment-variables.md
/// lists it: `OXA_SESSION_ID`, `OXA_SESSION_FILE` (unset when ephemeral), `OXA_PROVIDER`
/// (always `oxe`), `OXA_MODEL`, `OXA_REASONING_LEVEL`, `AI_AGENT`. Child-process environment
/// only, never on the model wire (W3/W4). Called when the ids attach and before tools or a
/// user `!cmd` run, so a model or thinking switch is seen by the next command.
pub(crate) fn refresh_tool_env(st: &mut SessionState) {
    let mut env = vec![("OXA_SESSION_ID".to_string(), st.sref.id.clone())];
    let p = st.sref.path.to_string_lossy();
    if !st.sref.path.as_os_str().is_empty() && !p.starts_with("mem:") {
        env.push(("OXA_SESSION_FILE".to_string(), p.into_owned()));
    }
    env.push(("OXA_PROVIDER".to_string(), "oxe".to_string()));
    env.push(("OXA_MODEL".to_string(), st.model.clone()));
    env.push((
        "OXA_REASONING_LEVEL".to_string(),
        st.thinking.as_str().to_string(),
    ));
    env.push(("AI_AGENT".to_string(), names::AI_AGENT.to_string()));
    env.push(("OXA_CODING_AGENT".to_string(), "true".to_string()));
    st.tool_env = env;
}

/// The display name recorded last on the active branch.
fn session_name(st: &SessionState) -> Option<String> {
    st.tree
        .branch(st.tree.leaf())
        .into_iter()
        .rev()
        .find_map(|e| match &e.body {
            EntryBody::SessionInfo { name } => Some(name.clone()),
            _ => None,
        })
}

/// The prompt text after `expand_input` (templates, skills), before hook `Input`: what
/// [`Session::prompt`] and [`Session::prefill_draft`] both start from.
fn prompt_text(st: &SessionState, text: String) -> Result<String, OxaError> {
    match crate::expand_input(st, &text) {
        Expanded::Prompt(p) => Ok(p),
        // pi's `session.prompt` (print mode, RPC `prompt`) sends a `!` line as it is: only the
        // interactive editor runs it as a command (`user_bash`).
        Expanded::Bash { .. } => Ok(text),
        _ => Err(OxaError::new(
            codes::INVALID,
            "slash commands and ! lines go through run_command or user_bash",
        )),
    }
}

/// The user message a prompt appends (image parts become placeholder lines).
fn user_message(text: &str, parts: &[Part]) -> WireMsg {
    WireMsg::user(wire_text(text, parts))
}

/// V020-OPTIMIZE #13: the user message the prompt `text` + `parts` would append if it were sent
/// now, built by the prompt's own functions; `None` when the draft is not prefilled (see
/// [`Session::prefill_draft`]).
fn draft_message(st: &mut SessionState, text: &str, parts: &[Part]) -> Option<WireMsg> {
    if !st.settings.prefill_paste(st.stats.profile)
        || !st.info.prefill_only
        || !engine_on_this_machine(&st.info.base_url)
        || st.settings.string("cacheWarming").as_deref() == Some("off")
        || text.trim_start().starts_with('!')
        || !st.next_turn.is_empty()
    {
        return None;
    }
    // The prompt's own bytes are not known before these hooks run (`before_agent_start` may
    // also block the prompt: then the draft is never sent).
    let hooked = [
        HookPoint::Input,
        HookPoint::BeforeAgentStart,
        HookPoint::Context,
        HookPoint::ContextWithSystem,
        HookPoint::BeforeProviderRequest,
    ]
    .iter()
    .any(|p| crate::fire::wants(st, *p));
    if hooked || crate::host::host_pending(st) {
        return None;
    }
    if !matches!(crate::expand_input(st, text), Expanded::Prompt(_)) {
        return None;
    }
    let text = prompt_text(st, text.to_string()).ok()?;
    let msg = user_message(&text, parts);
    let len = msg.content.as_deref().map_or(0, str::len);
    // The draft carries the note the prompt would get, so the prefilled bytes match the send.
    (len >= Session::DRAFT_PREFILL_MIN_BYTES).then(|| crate::envnote::with_note(st, msg))
}

/// V020-OPTIMIZE #13, privacy: a draft the user never submitted is sent only to an engine on
/// this machine: a loopback host (`localhost`, 127.0.0.0/8, `::1`, as `oxa.engine.allowRemote`
/// judges it), or an in-process engine whose base URL has no network authority (test fakes).
fn engine_on_this_machine(base_url: &str) -> bool {
    match oxa_proto::url_host(base_url) {
        Some(host) => oxa_proto::is_loopback_host(&host),
        None => !base_url.contains("://"),
    }
}

/// The text of a user message on the wire: the typed text, then each text part, then one
/// placeholder line per image (W2: images are shown, never sent).
fn wire_text(text: &str, parts: &[Part]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !text.is_empty() {
        lines.push(text.to_string());
    }
    for part in parts {
        match part {
            Part::Text(t) => lines.push(t.clone()),
            // Provenance of a stored message; never among a prompt's parts.
            Part::EnvNote(_) => {}
            Part::Image { path, w, h, .. } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                lines.push(image_placeholder(&name, *w, *h));
            }
        }
    }
    lines.join("\n")
}

/// S3: what to do when the conversation does not fit the target model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverCap {
    /// O-145: compact with the SOURCE model first (the summary is sized for the target), then
    /// switch. A failed compaction leaves the model unchanged.
    CompactFirst,
    /// Change nothing and return `invalid` with the plan's numbers (the caller offers compact
    /// first / fork / cancel).
    Refuse,
}

/// The cost of a model switch, for the notice and the over-cap guard (S3 / ENG-004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchPlan {
    pub target: String,
    /// Tokens of the current conversation (`Transcript::context`, 0 when unknown).
    pub context_tokens: u32,
    /// The target's context cap, when the engine publishes it before the switch.
    pub target_ctx: Option<u32>,
    /// The target's hard compaction limit under the current settings.
    pub target_hard: Option<u32>,
    /// The conversation is above the target's hard limit: the first request would overflow.
    pub over_cap: bool,
    /// Tokens the target prefills cold on its first request (the whole conversation).
    pub cold_tokens: u32,
    /// Estimated seconds of that prefill ([`prefill_estimate_secs`]).
    pub est_secs: u32,
}

/// The tiled prefill estimate of OXA-ENGINE-SUPPORT sec. 0: `3.0 p + 0.112 p^2 / 1000` ms.
pub fn prefill_estimate_secs(tokens: u32) -> u32 {
    let p = f64::from(tokens);
    ((3.0 * p + 0.112 * p * p / 1000.0) / 1000.0).round() as u32
}

/// The target's limits and compaction policy under the current settings, when the engine
/// publishes limits for `m` (`limits_for == m`, the controller's `model_limits`).
fn target_policy(st: &SessionState, m: &str) -> Option<(EngineInfo, CompactPolicy)> {
    match st.engine.info(Some(m)) {
        Ok(info) if info.limits_for == m => {
            let policy = policy_for_model(&info, &st.settings, m, st.settings.profile());
            Some((info, policy))
        }
        _ => None,
    }
}

/// The tokens of the current conversation as the engine would prefill it: the last request's
/// `prompt + completion` (`Transcript::context`, exact); after a rebase (unknown until the next
/// reply) the engine's exact count of the rebased wire (`Engine::count_prompt`), else the
/// tokenizer estimate (prefix + wire messages). Used by the switch guard (O-145) and its
/// post-compaction fit check.
fn conversation_tokens(st: &SessionState) -> u32 {
    match st.transcript.context() {
        0 => crate::compaction::prompt_tokens(st, st.transcript.wire()).0,
        n => n,
    }
}

/// S3 / ENG-004: the plan of a switch to `m`, read without changing anything.
///
/// * `context_tokens`: [`conversation_tokens`] (the last `prompt + completion`, or an
///   estimate after a rebase).
/// * `target_ctx` / `target_hard`: only when the engine publishes `m`'s limits; `target_hard`
///   is `policy_for_model(target).hard` = `n_ctx - max_output - reserveTokens` (at least 1024).
/// * `over_cap = context_tokens > target_hard`: the first request to the target (the whole
///   conversation plus `max_output` of room) or the summary it would need (plus the reserve)
///   would not fit the target's context. Unknown limits: `false` (the provisional-minimum rule
///   of `refresh_budget` applies after the switch).
/// * `cold_tokens = context_tokens` (the target holds nothing of this session: the whole wire
///   is prefilled cold), `est_secs = prefill_estimate_secs(cold_tokens)`.
pub(crate) fn plan_switch(st: &SessionState, m: &str) -> SwitchPlan {
    let context_tokens = conversation_tokens(st);
    let (target_ctx, target_hard) = match target_policy(st, m) {
        Some((info, policy)) => (Some(info.n_ctx), Some(policy.hard)),
        None => (None, None),
    };
    SwitchPlan {
        target: m.to_string(),
        context_tokens,
        target_ctx,
        target_hard,
        over_cap: target_hard.is_some_and(|hard| context_tokens > hard),
        cold_tokens: context_tokens,
        est_secs: prefill_estimate_secs(context_tokens),
    }
}

/// The one door for a model switch (`Session::set_model*`, `HostAction::SetModel`, RPC).
///
/// 1. [`plan_switch`]. Over the target's cap:
///    * `OverCap::Refuse`: nothing changes, nothing is sent; `invalid` with both numbers.
///    * `OverCap::CompactFirst` (O-145): compact NOW on the SOURCE model (`st.model` is still
///      the old one; W5: whole wire + one instruction, then the `Compaction` entry), in the
///      foreground, sized for the TARGET: summary at most the target's `summary_max`, kept tail
///      at most the target's `keep_tokens` and at most `soft - prefix - summary_max` (else the
///      summary alone). The switch goes on only when the compaction rebased the wire AND the
///      new conversation (prefix + head, estimated with the tokenizer) is within the target's
///      hard limit; otherwise the model is unchanged, an error is returned and nothing was
///      sent to the target.
/// 2. `ModelChange` entry, `ModelSelect` event, [`refresh_budget`] for the target, the hook
///    point `ModelSelect` (`{model, previousModel}`; hook sets see the switch and the new
///    limits before the epoch's tool list is built), then the epoch, whose notice names the
///    target, the tokens it prefills cold and the estimated seconds (one notice, one number:
///    `PrefixEpoch.reprefill_tokens` is the same count).
pub(crate) fn switch_model(
    st: &mut SessionState,
    m: &str,
    over: OverCap,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let plan = plan_switch(st, m);
    let mut cold = plan.cold_tokens;
    if plan.over_cap {
        let (Some(hard), Some((tinfo, tpolicy))) = (plan.target_hard, target_policy(st, m)) else {
            return Err(OxaError::new(
                codes::INTERNAL,
                "the target's limits vanished during the switch",
            ));
        };
        let numbers = format!(
            "the conversation is about {} tokens; {m} takes at most {hard} (context {})",
            plan.context_tokens, tinfo.n_ctx
        );
        if over == OverCap::Refuse {
            return Err(OxaError::new(
                codes::INVALID,
                format!("{numbers}: compact first, fork or cancel"),
            )
            .with_hint("compact_first"));
        }
        let source = st.model.clone();
        notice(
            sink,
            Level::Info,
            format!("{numbers}; compacting on {source} before the switch"),
        );
        let prefix = crate::compaction::prefix_tokens(st);
        let fit = crate::compaction::Fit {
            summary_max: tpolicy.summary_max,
            keep_tokens: tpolicy.keep_tokens,
            tail_max: tpolicy
                .soft
                .saturating_sub(prefix.saturating_add(tpolicy.summary_max)),
        };
        // An abort must reach this foreground compaction: claim the session for its duration
        // when nobody holds it (the epoch below needs it released again).
        let claimed = st
            .shared
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if claimed {
            st.shared.cancel.reset();
        }
        // Released by a guard, so a panic in the compaction cannot leave the session busy.
        let release = claimed.then(|| BusyGuard(st.shared.clone()));
        let compacted = crate::compaction::compact_for_switch(st, fit, sink, ui);
        drop(release);
        if let Err(e) = compacted {
            return Err(OxaError::new(
                e.code,
                format!("the model is still {source}: the compaction before the switch to {m} did not complete ({})", e.message),
            ));
        }
        cold = conversation_tokens(st);
        if cold > hard {
            return Err(OxaError::new(
                codes::INVALID,
                format!(
                    "the model is still {source}: after compaction the conversation is still about {cold} tokens, above {m}'s limit of {hard}"
                ),
            ));
        }
    }
    persist(st, EntryBody::ModelChange(m.to_string()), sink)?;
    let previous = std::mem::replace(&mut st.model, m.to_string());
    // The speed belongs to the model that produced it.
    st.stats.last_tok_s = 0.0;
    sink.emit(&AgentEvent::ModelSelect {
        model: m.to_string(),
        previous: Some(previous.clone()),
    });
    refresh_budget(st, None, sink);
    observe_hooks(
        st,
        HookPoint::ModelSelect,
        &serde_json::json!({ "model": m, "previousModel": previous, "source": "set" }),
        sink,
        ui,
    );
    crate::epoch::open_epoch_costed(
        st,
        "model",
        sink,
        crate::epoch::EpochCost {
            tokens: cold,
            who: Some(m.to_string()),
        },
    )?;
    Ok(())
}

/// Emit the queue state once when it changed since the last emission.
fn flush_queue(st: &SessionState, sink: &mut dyn EventSink) {
    if st.shared.dirty.swap(false, Ordering::SeqCst) {
        let (steering, follow_up) = st.shared.snapshot();
        sink.emit(&AgentEvent::QueueUpdate { steering, follow_up });
    }
}

/// The compaction policy for `info` under `settings` (`compaction.*`). An explicit
/// `oxa.compaction.softTokens` wins; else the controller's compaction point for the selected
/// context mode (D5 `compact_at`); else the policy's default.
#[cfg(test)]
pub(crate) fn policy_for(info: &EngineInfo, settings: &Settings) -> CompactPolicy {
    policy_for_model(info, settings, &info.limits_for, oxa_proto::Profile::Pi)
}

/// [`policy_for`] for the session model `model`, with its `compaction.modelOverrides` entry
/// applied (S4, CMP-015 / SET-026): the setting is an object keyed by engine model id; the
/// entry of `model`, when it is an object, overrides `reserveTokens` and `keepRecentTokens`.
/// Entries of other models do nothing here; a `model` that names another model than `model`
/// is ignored (see [`override_summary_model`]).
///
/// `profile` is the epoch's (R5): under `local` on a small context the kept tail is
/// [`crate::tune::SMALL_KEEP_TOKENS`] unless `compaction.keepRecentTokens` is set in a layer or
/// by the model's override.
pub(crate) fn policy_for_model(
    info: &EngineInfo,
    settings: &Settings,
    model: &str,
    profile: oxa_proto::Profile,
) -> CompactPolicy {
    let comp = settings.compaction();
    let entry = settings
        .get("compaction.modelOverrides")
        .and_then(|o| o.get(model))
        .filter(|v| v.is_object());
    let num = |key: &str, default: u32| {
        entry
            .and_then(|e| e.get(key))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
    };
    let keep_set = settings.get("compaction.keepRecentTokens").is_some()
        || entry.is_some_and(|e| e.get("keepRecentTokens").is_some());
    let keep = crate::tune::keep_tokens(
        profile,
        info.n_ctx,
        keep_set,
        num("keepRecentTokens", comp.keep_recent_tokens),
    );
    oxa_compact::policy_from(
        info.n_ctx,
        info.max_output,
        num("reserveTokens", comp.reserve_tokens),
        keep,
        comp.soft_tokens.or(info.compact_at),
        comp.enabled,
    )
}

/// S4: the other model a `compaction.modelOverrides` entry of `model` would send the summary
/// to (`"<model>": "<other>"` or `"<model>": {"model": "<other>"}`), if any. The caller ignores
/// it with a warning: the summary always runs on the session model.
pub(crate) fn override_summary_model(settings: &Settings, model: &str) -> Option<String> {
    let entry = settings.get("compaction.modelOverrides")?.get(model)?;
    let other = match entry {
        Value::String(s) => s.as_str(),
        Value::Object(o) => o.get("model")?.as_str()?,
        _ => return None,
    };
    (!other.is_empty() && other != model).then(|| other.to_string())
}

/// `st.info` describes the selected model `st.model` and the engine reported it as loaded (so
/// `prefill_only` is the target's too). Until then the budget is re-read around each run.
pub(crate) fn budget_confirmed(st: &SessionState) -> bool {
    st.info.model == st.model && st.info.limits_for == st.model
}

/// X3: re-read `Engine::info(Some(st.model))` and recompute `st.compact` from it and the current
/// settings. Called on a model switch, on `/reload` and before/after a run while the budget is
/// not confirmed. The target-specific contract (oxa-core README, "Engine contract"):
///
/// * The engine publishes the target's limits (`limits_for == st.model`; the controller's
///   per-model `model_limits`, also for a model that is not loaded): its `n_ctx`, `max_output`
///   and `compact_at` are adopted as they are, so the first request to the target already fits
///   it. `prefill_only` (a property of the loaded engine) is the reported one when the target
///   is loaded, else only when the previous and the reported engine both advertise it.
/// * Otherwise (an engine without per-model limits) the target's limits are not known: the
///   budget is provisional, each limit the smaller of the previous one and the reported one,
///   and it is re-read before and after each run until the engine reports the target.
/// * An `info` failure keeps the previous metadata (a notice says so); the policy is still
///   recomputed so changed settings apply.
///
/// Auto-compaction: the runtime switch (`set_auto_compaction`) wins over the
/// `compaction.enabled` setting. `prev_enabled` is that setting before a `/reload`: only when
/// the reload changes it does the switch follow the new setting. `st.compact.enabled` always
/// equals the switch.
pub(crate) fn refresh_budget(st: &mut SessionState, prev_enabled: Option<bool>, sink: &mut dyn EventSink) {
    match st.engine.info(Some(&st.model)) {
        Ok(fresh) if fresh.limits_for == st.model => {
            let prefill_only = if fresh.model == st.model {
                fresh.prefill_only
            } else {
                st.info.prefill_only && fresh.prefill_only
            };
            st.info = EngineInfo {
                prefill_only,
                thinking: fresh.thinking,
                images: fresh.images,
                ..fresh
            };
        }
        Ok(fresh) => {
            let old = &st.info;
            let compact_at = match (old.compact_at, fresh.compact_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let merged = EngineInfo {
                n_ctx: old.n_ctx.min(fresh.n_ctx),
                max_output: old.max_output.min(fresh.max_output),
                compact_at,
                prefill_only: old.prefill_only && fresh.prefill_only,
                caps: oxa_proto::EngineCaps {
                    snapshot_keep: old.caps.snapshot_keep.min(fresh.caps.snapshot_keep),
                    hint_clear: old.caps.hint_clear && fresh.caps.hint_clear,
                    ..fresh.caps
                },
                thinking: fresh.thinking,
                images: fresh.images,
                ..fresh
            };
            if merged != st.info {
                notice(
                    sink,
                    Level::Info,
                    format!(
                        "the engine has not published limits for {} yet (loaded: {}); using {} context / {} output tokens until it does",
                        st.model, merged.model, merged.n_ctx, merged.max_output
                    ),
                );
            }
            st.info = merged;
        }
        Err(e) => notice(
            sink,
            Level::Warning,
            format!("could not re-read the engine limits ({e}); keeping the previous ones"),
        ),
    }
    st.compact = policy_for_model(&st.info, &st.settings, &st.model, st.settings.profile());
    if prev_enabled.is_some_and(|p| p != st.compact.enabled) {
        st.auto_compaction = st.compact.enabled;
    }
    st.compact.enabled = st.auto_compaction;
    crate::tune::refresh(st);
}

fn notice(sink: &mut dyn EventSink, level: Level, text: impl Into<String>) {
    sink.emit(&AgentEvent::Notice {
        level,
        text: text.into(),
    });
}

fn notice_error(sink: &mut dyn EventSink, e: &OxaError) {
    notice(sink, Level::Error, e.to_string());
}

fn empty_prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: Vec::new(),
        tools_json: String::new(),
        sha256: [0u8; 32],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(env: &oxa_fake::TestEnv, store: Arc<dyn SessionStore>) -> Deps {
        Deps {
            engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(oxa_perm::ModePolicy::new()),
            sandbox: Arc::new(oxa_sandbox::NoSandbox),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            selection: crate::ToolSelection::default(),
            resources: oxa_resources::Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
        }
    }

    /// The speed belongs to the model that produced it: a switch drops it for every later status.
    #[test]
    fn model_switch_resets_last_tok_s() {
        let env = oxa_fake::TestEnv::new();
        let mut s = Session::open(deps(&env, Arc::new(MemStore::new())), Open::New).expect("open");
        s.st.stats.last_tok_s = 54.4;
        let other = if s.st.model == "other-model" {
            "x-model"
        } else {
            "other-model"
        };
        let mut sink = VecSink::default();
        let _ = s.set_model(other, &mut sink);
        assert_eq!(s.st.model, other, "the switch must have happened");
        assert_eq!(s.stats().last_tok_s, 0.0);
        assert_eq!(s.stats().last_tok_s, 0.0, "and it stays cleared");
    }

    /// Messages queued for the next turn are appended, in order, before the prompt's own message.
    #[test]
    fn next_turn_is_appended_before_the_prompt() {
        let env = oxa_fake::TestEnv::new();
        let mut s = Session::open(deps(&env, Arc::new(MemStore::new())), Open::New).expect("open");
        s.st.next_turn.push(WireMsg::user("queued earlier"));
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        let _ = s.prompt("now".to_string(), Vec::new(), &mut sink, &mut ui);
        let texts: Vec<String> = s
            .inner()
            .transcript
            .wire()
            .iter()
            .filter_map(|m| m.content.clone())
            .collect();
        assert_eq!(texts[0], "queued earlier");
        // The prompt may carry the default environment note. Compare its typed text using
        // the persisted provenance, while still checking the wire ordering above.
        let parts = s
            .inner()
            .tree
            .entries()
            .iter()
            .find_map(|entry| match &entry.body {
                EntryBody::Msg { wire, parts } if wire.content.as_deref() == Some(&texts[1]) => Some(parts),
                _ => None,
            })
            .expect("stored prompt");
        assert_eq!(crate::envnote::strip_note(&texts[1], parts), "now");
        assert!(s.st.next_turn.is_empty());
    }
}
