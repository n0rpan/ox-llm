//! Z05: the front-end seam of the interactive mode.
//!
//! `oxa_cli::run_with` builds a session exactly as `oxa_cli::run` does and hands the two things
//! that need a person to a [`Frontend`]: the project trust question, and the opened session
//! with its `InteractiveOpts`. `run` is `run_with` with `Terminal`, so the terminal and any
//! other front end (the desktop app) share one build path: flags, trust, settings, hooks, MCP,
//! limits and the reopen function of `/new`, `/resume`, fork and clone.
//!
//! ## Several sessions in one process
//! `run_with` may be called on several threads at once; each call owns its session, store
//! handle, hook sets, YOLO switch and owner-confirmation broker
//! (`tests/frontend_seam.rs` runs two sessions at the same time). What the calls share is
//! process state, and a front end with several sessions must know it:
//! * the network policy (`oxa_proto::set_net_policy`: `--offline`, `OXA_OFFLINE`, `oxa.offline`,
//!   `oxa.engine.allowRemote`) is one value per process, set at every start. The last start
//!   wins, so every session of a process must be started with the same offline choice;
//! * `--max-time` / `--max-tokens` take the process's SIGTERM (`limits::SigtermOwner`), and the
//!   terminal owner of it is the TUI's: a front end other than the terminal starts its
//!   sessions without these two flags;
//! * `--share-model` binds one LAN listener per call;
//! * the dedicated Lead session (`oxa lead`) prepares through the terminal trust question.
//!   Another front end reaches the lead over its own routes, not through this function.
use oxa_config::{dirs_from_env, Env, Keymap};
use oxa_core::Session;
use oxa_interactive::{ask_trust_interactive, InteractiveOpts, TrustChoice};
use oxa_proto::OxaError;

/// The part of the interactive mode that talks to a person. One value serves one `run_with`
/// call; both methods are called on the thread that called `run_with`.
pub trait Frontend {
    /// The project trust question, asked before the session exists: `cwd` is the project
    /// directory and `gated` lists what waits for the answer (the gated project resources, or
    /// the project files changed since the last approval). `Ok(None)` = dismissed: the project
    /// stays untrusted for this run and the question comes again at the next start. Dismissing
    /// never trusts. The caller saves the decision; the front end only asks.
    fn ask_trust(&mut self, cwd: &str, gated: &[String]) -> Result<Option<TrustChoice>, OxaError>;

    /// Run the interactive mode on the opened session until the person is done, and return the
    /// exit code. `pinned` is the resume command of a session the front end must not leave
    /// (`Some("oxa lead")` for the dedicated Lead session), `None` otherwise.
    ///
    /// The session belongs to the front end from here on. `opts.reopen` builds the next session
    /// for `/new`, `/resume`, fork and clone with the same flags, and `opts.save_trust` saves a
    /// later `/trust` decision.
    fn interactive(
        &mut self,
        session: Session,
        opts: InteractiveOpts,
        pinned: Option<&str>,
    ) -> Result<i32, OxaError>;
}

/// The terminal (the TUI of `oxa-interactive`): the front end of `oxa_cli::run`.
pub(crate) struct Terminal<'a> {
    pub env: &'a Env,
}

impl Frontend for Terminal<'_> {
    fn ask_trust(&mut self, cwd: &str, gated: &[String]) -> Result<Option<TrustChoice>, OxaError> {
        // The keybindings file is in the agent directory, which no session flag moves.
        let (keymap, _) = Keymap::load(&dirs_from_env(self.env, None)?);
        ask_trust_interactive(self.env.pairs(), &keymap, cwd, gated)
    }

    fn interactive(
        &mut self,
        session: Session,
        opts: InteractiveOpts,
        pinned: Option<&str>,
    ) -> Result<i32, OxaError> {
        match pinned {
            Some(resume_command) => oxa_interactive::run_pinned(session, opts, resume_command),
            None => oxa_interactive::run(session, opts),
        }
    }
}
